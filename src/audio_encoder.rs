use bytes::Bytes;
use crossfire::{MAsyncRx, MAsyncTx, mpmc, mpsc};
use opus::{Application, Channels, Encoder};
use tokio_util::sync::CancellationToken;

use crate::Frame;

pub struct OpusEncoder {
    cancel_token: CancellationToken,
}

impl OpusEncoder {
    pub fn new(
        raw_rx: MAsyncRx<mpmc::Array<Frame>>,
        encoded_tx: MAsyncTx<mpsc::Array<Frame>>,
    ) -> anyhow::Result<OpusEncoder> {
        let cancel_token = CancellationToken::new();
        tokio::spawn(Self::background(raw_rx, encoded_tx, cancel_token.clone()));
        Ok(OpusEncoder { cancel_token })
    }

    pub fn close(&self) {
        self.cancel_token.cancel();
    }

    async fn background(
        raw_rx: MAsyncRx<mpmc::Array<Frame>>,
        encoded_tx: MAsyncTx<mpsc::Array<Frame>>,
        cancel_token: CancellationToken,
    ) {
        let mut encoder = match Encoder::new(48000, Channels::Stereo, Application::Audio) {
            Ok(enc) => enc,
            Err(e) => {
                tracing::error!("无法初始化 Opus 编码器: {}", e);
                return;
            }
        };
        let mut out_buf = [0u8; 4000];
        tracing::info!("OpusEncoder 启动成功，等待音频数据传入...");
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("OpusEncoder 收到取消信号，准备退出...");
                    break;
                }
                Ok(frame) = raw_rx.recv() => {
                    let pcm_data: &[i16] = match bytemuck::try_cast_slice(&frame.data) {
                        Ok(data) => data,
                        Err(_) => {
                            tracing::error!("音频数据大小不对齐，无法转换为 i16 数组，丢弃该帧");
                            continue;
                        }
                    };
                    match encoder.encode(pcm_data, &mut out_buf) {
                        Ok(encoded_len) => {
                            let encoded_bytes = Bytes::copy_from_slice(&out_buf[..encoded_len]);
                            let out_frame = Frame {
                                data: encoded_bytes,
                                dur: frame.dur,
                                ts: frame.ts,
                            };
                            if encoded_tx.try_send(out_frame).is_err() {
                                tracing::warn!("网络拥堵：Encoded 音频队列满，主动丢弃一帧 Opus 数据");
                            }
                        }
                        Err(e) => {
                            tracing::error!("Opus 编码失败: {}", e);
                        }
                    }
                }
            }
        }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        self.close();
    }
}
