use std::process::Stdio;

use h264_parser::AnnexBParser;
use tokio::process::Command;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::whip::MediaFrame;

pub fn arg_builder() -> Vec<String> {
    let a = [
        // 输入格式
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-s",
        "1920x1080",
        "-r",
        "60",
        "-i",
        "pipe:0",
    ];
    let b = [
        "-vaapi_device",
        "/dev/dri/renderD128",
        "-vf",
        "format=nv12,hwupload",
    ];
    let c = [
        "-c:v",
        "h264_vaapi",
        // VA-API Constrained Baseline Profile
        "-profile:v",
        "578",
        // B Frame
        "-bf",
        "0",
        // Group of Pictures
        "-g",
        "60",
        // 目标视频码率
        "-b:v",
        "6000k",
        // 最大码率
        "-maxrate",
        "6000k",
        // 缓冲区大小
        "-bufsize",
        "12000k",
        // 输出格式
        "-f",
        "h264",
        "pipe:1",
    ];
    a.into_iter()
        .chain(b.into_iter())
        .chain(c.into_iter())
        .map(|s| s.to_string())
        .collect()
}

pub struct RawFrame {
    pub data: Vec<u8>,
    pub dur: Duration,
    pub ts: DateTime<Utc>,
}

pub struct H264Encoder {
    raw_tx: mpsc::Sender<RawFrame>,
    cancel_token: CancellationToken,
}

impl H264Encoder {
    pub fn new(encoded_tx: mpsc::Sender<MediaFrame>) -> anyhow::Result<H264Encoder> {
        let mut child = Command::new("ffmpeg")
            .args(arg_builder())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("无法启动 FFmpeg");

        let ffmpeg_stdin = child.stdin.take().unwrap();
        let ffmpeg_stdout = child.stdout.take().unwrap();
        let ffmpeg_stderr = child.stderr.take().unwrap();

        let (raw_tx, raw_rx) = mpsc::channel::<RawFrame>(5);

        let (pts_tx, pts_rx) = mpsc::channel::<(Duration, DateTime<Utc>)>(30);

        let cancel_token = CancellationToken::new();

        tokio::spawn(Self::background(
            child,
            ffmpeg_stdin,
            ffmpeg_stdout,
            ffmpeg_stderr,
            raw_rx,
            pts_tx,
            pts_rx,
            encoded_tx,
            cancel_token.clone(),
        ));

        Ok(H264Encoder {
            raw_tx,
            cancel_token,
        })
    }

    pub fn push_frame(&self, frame: RawFrame) -> Result<(), &'static str> {
        match self.raw_tx.try_send(frame) {
            Ok(_) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err("编码器队列满，主动丢弃原始帧"),
            Err(mpsc::error::TrySendError::Closed(_)) => Err("编码器已关闭"),
        }
    }

    pub fn close(&self) {
        self.cancel_token.cancel();
    }

    async fn background(
        mut child: tokio::process::Child,
        mut stdin: tokio::process::ChildStdin,
        mut stdout: tokio::process::ChildStdout,
        mut stderr: tokio::process::ChildStderr,
        mut raw_rx: mpsc::Receiver<RawFrame>,
        pts_tx: mpsc::Sender<(Duration, DateTime<Utc>)>,
        mut pts_rx: mpsc::Receiver<(Duration, DateTime<Utc>)>,
        encoded_tx: mpsc::Sender<MediaFrame>,
        cancel_token: CancellationToken,
    ) {
        // let stderr_task = tokio::spawn(async move {
        //     let mut reader = tokio::io::BufReader::new(stderr);
        //     let mut line = String::new();
        //     while let Ok(bytes) =
        //         tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await
        //     {
        //         if bytes == 0 {
        //             break;
        //         }
        //         // FFmpeg 的日志通常比较啰嗦，可以按需改成 debug 级别
        //         tracing::debug!("FFmpeg: {}", line.trim_end());
        //         line.clear();
        //     }
        // });

        let stdin_task = tokio::spawn(async move {
            while let Some(frame) = raw_rx.recv().await {
                if pts_tx.send((frame.dur, frame.ts)).await.is_err() {
                    break;
                }
                if let Err(e) = stdin.write_all(&frame.data).await {
                    tracing::error!("向 FFmpeg stdin 写入失败: {}", e);
                    break;
                }
            }
        });

        let stdout_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 1024 * 1024];
            let mut parser = h264_parser::parser::AnnexBParser::new();

            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => {
                        tracing::info!("FFmpeg stdout EOF");
                        while let Ok(Some(au)) = parser.next_access_unit_final() {
                            process_access_unit(au, &mut pts_rx, &encoded_tx).await;
                        }
                        break;
                    }
                    Ok(n) => {
                        parser.push(&buf[..n]);

                        while let Ok(Some(au)) = parser.next_access_unit() {
                            process_access_unit(au, &mut pts_rx, &encoded_tx).await;
                        }
                    }
                    Err(e) => {
                        tracing::error!("读取 FFmpeg stdout 失败: {}", e);
                        break;
                    }
                }
            }
        });

        tokio::select! {
            _ = cancel_token.cancelled() => {
                tracing::info!("Encoder 收到取消信号，准备清理 FFmpeg 进程...");
            }
            _ = stdin_task => { tracing::warn!("Encoder 输入流水线意外终止"); }
            _ = stdout_task => { tracing::warn!("Encoder 输出流水线意外终止"); }
        }

        if let Err(e) = child.kill().await {
            tracing::error!("无法杀死 FFmpeg 进程: {}", e);
        } else {
            tracing::info!("FFmpeg 进程已成功终止");
        }

        // stderr_task.abort();
    }
}

async fn process_access_unit(
    au: h264_parser::AccessUnit,
    pts_rx: &mut mpsc::Receiver<(std::time::Duration, chrono::DateTime<chrono::Utc>)>,
    encoded_tx: &mpsc::Sender<MediaFrame>,
) {
    let mut frame_data = Vec::with_capacity(au.nals.iter().map(|n| n.ebsp.len() + 5).sum());

    for nal in au.nals {
        let header_byte = (nal.ref_idc << 5) | (nal.nal_type.as_u8());
        // start code
        frame_data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        // header
        frame_data.push(header_byte);
        // ebsp
        frame_data.extend_from_slice(&nal.ebsp);
    }
    let (dur, ts) = match pts_rx.recv().await {
        Some(pts_info) => pts_info,
        None => {
            tracing::error!("严重异常: 拿到一帧画面，但时间戳队列已经空了/关闭了！");
            return;
        }
    };
    let media_frame = MediaFrame {
        data: Bytes::from(frame_data),
        dur,
        ts,
    };
    if encoded_tx.try_send(media_frame).is_err() {
        tracing::warn!(
            "网络拥堵：Encoded 队列满，主动丢弃一个已编码帧 (类型: {:?})",
            au.kind
        );
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        self.close();
    }
}
