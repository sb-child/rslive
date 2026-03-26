use std::process::Stdio;

use h264_parser::AnnexBParser;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt},
    process::Command,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use crossfire::{AsyncRx, MAsyncRx, MAsyncTx, mpmc, mpsc};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::Frame;

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
        "-stats_period",
        "1",
        // "-progress",
        // "pipe:2",
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
        // "-c:v",
        // "libx264",
        // "-profile:v",
        // "baseline",
        // "-pix_fmt",
        // "yuv420p",
        "-bf", // B Frame
        "0",
        "-g", // Group of Pictures
        "10",
        "-b:v",     // 目标视频码率
        "6000k",    // 6000k
        "-maxrate", // 最大码率
        "6000k",
        "-bufsize", // 缓冲区大小
        "12000k",   // 12000k
        "-f",       // 输出格式
        "h264",
        "pipe:1",
    ];
    a.into_iter()
        .chain(b.into_iter())
        .chain(c.into_iter())
        .map(|s| s.to_string())
        .collect()
}

pub struct H264Encoder {
    // raw_tx: mpsc::Sender<RawFrame>,
    cancel_token: CancellationToken,
}

impl H264Encoder {
    pub fn new(
        raw_rx: MAsyncRx<mpmc::Array<Frame>>,
        encoded_tx: MAsyncTx<mpmc::Array<Frame>>,
    ) -> anyhow::Result<H264Encoder> {
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

        // let (raw_tx, raw_rx) = mpsc::channel::<RawFrame>(5);

        let (pts_tx, pts_rx) = mpmc::bounded_async::<(Duration, DateTime<Utc>)>(30);

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
            // raw_tx,
            cancel_token,
        })
    }

    // pub fn push_frame(&self, frame: RawFrame) -> Result<(), &'static str> {
    //     match self.raw_tx.try_send(frame) {
    //         Ok(_) => Ok(()),
    //         Err(mpsc::error::TrySendError::Full(_)) => Err("编码器队列满，主动丢弃原始帧"),
    //         Err(mpsc::error::TrySendError::Closed(_)) => Err("编码器已关闭"),
    //     }
    // }

    pub fn close(&self) {
        self.cancel_token.cancel();
    }

    async fn background(
        mut child: tokio::process::Child,
        mut stdin: tokio::process::ChildStdin,
        mut stdout: tokio::process::ChildStdout,
        mut stderr: tokio::process::ChildStderr,
        raw_rx: MAsyncRx<mpmc::Array<Frame>>,
        pts_tx: MAsyncTx<mpmc::Array<(Duration, DateTime<Utc>)>>,
        mut pts_rx: MAsyncRx<mpmc::Array<(Duration, DateTime<Utc>)>>,
        encoded_tx: MAsyncTx<mpmc::Array<Frame>>,
        cancel_token: CancellationToken,
    ) {
        let stderr_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(bytes) = read_until_any_eol(&mut reader, &mut line).await {
                if bytes == 0 {
                    break;
                }
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    tracing::info!("FFmpeg: {}", trimmed);
                }
                line.clear();
            }
        });

        let stdin_task = tokio::spawn(async move {
            let mut total_frames = 0;
            let mut fps_counter = 0;
            let mut last_time = std::time::Instant::now();

            while let Ok(frame) = raw_rx.recv().await {
                total_frames += 1;
                fps_counter += 1;

                let elapsed = last_time.elapsed();
                if elapsed.as_secs_f32() >= 1.0 {
                    let actual_fps = fps_counter as f32 / elapsed.as_secs_f32();
                    tracing::info!(
                        "H264Encoder input: Total Frames: {} | Size: ({:.2} MB) | Actual FPS: {:.2}",
                        total_frames,
                        (frame.data.len() as f64) / 1024.0 / 1024.0,
                        actual_fps
                    );
                    fps_counter = 0;
                    last_time = std::time::Instant::now();
                }

                if let Err(e) = pts_tx.send((frame.dur, frame.ts)).await {
                    tracing::error!("failed to send pts: {}", e);
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
            let mut parser = AnnexBParser::new();

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

        stderr_task.abort();
    }
}

async fn process_access_unit(
    au: h264_parser::AccessUnit,
    pts_rx: &mut MAsyncRx<mpmc::Array<(Duration, DateTime<Utc>)>>,
    encoded_tx: &MAsyncTx<mpmc::Array<Frame>>,
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
        Ok(pts_info) => pts_info,
        Err(_) => {
            tracing::error!("严重异常: 拿到一帧画面，但时间戳队列已经空了/关闭了！");
            return;
        }
    };
    let media_frame = Frame {
        data: Bytes::from(frame_data),
        dur,
        ts,
    };
    if encoded_tx.try_send(media_frame).is_err() {
        tracing::warn!(
            "网络拥堵：Encoded 队列满，主动丢弃一个已编码帧 (类型: {:?})",
            au.kind
        );
    } else {
        // tracing::info!("Encoder: 输出帧 (类型: {:?})", au.kind);
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        self.close();
    }
}

async fn read_until_any_eol<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> std::io::Result<usize> {
    let mut read_bytes = 0;
    loop {
        let (done, used) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(read_bytes); // EOF
            }
            match available.iter().position(|&b| b == b'\r' || b == b'\n') {
                Some(i) => {
                    let chunk = &available[..=i];
                    buf.push_str(&String::from_utf8_lossy(chunk));
                    (true, i + 1)
                }
                None => {
                    buf.push_str(&String::from_utf8_lossy(available));
                    (false, available.len())
                }
            }
        };
        reader.consume(used);
        read_bytes += used;
        if done {
            return Ok(read_bytes);
        }
    }
}
