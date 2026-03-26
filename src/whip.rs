use std::{collections::HashMap, sync::Arc, time::Duration};

use crossfire::{MAsyncRx, mpmc};
use reqwest::{Client, StatusCode};
use snafu::{ResultExt, prelude::*};
use tokio::{
    sync::{mpsc as tokio_mpsc, watch},
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;
use webrtc::{
    api::{APIBuilder, media_engine::MediaEngine},
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::{rtp_codec::RTCRtpCodecCapability, rtp_sender::RTCRtpSender},
    track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample},
};

use crate::Frame;

pub struct WhipStreamerOpt {
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamState {
    Starting,
    Connected,
    Reconnecting,
    Disconnected,
    Error(String),
}
pub struct WhipStreamer {
    state_rx: watch::Receiver<StreamState>,
    cancel_token: CancellationToken,
    bgh: JoinHandle<()>,
}

impl WhipStreamer {
    pub fn new(
        opt: &WhipStreamerOpt,
        video_rx: MAsyncRx<mpmc::Array<Frame>>,
        audio_rx: MAsyncRx<mpmc::Array<Frame>>,
    ) -> WhipStreamer {
        let (state_tx, state_rx) = watch::channel(StreamState::Starting);
        let http_client = reqwest::Client::new();
        let cancel_token = CancellationToken::new();

        let bgh = tokio::spawn(Self::background(
            opt.url.clone(),
            opt.token.clone(),
            http_client,
            state_tx,
            video_rx,
            audio_rx,
            cancel_token.clone(),
        ));

        WhipStreamer {
            state_rx,
            cancel_token,
            bgh,
        }
    }

    pub fn close(&self) {
        self.cancel_token.cancel();
    }

    pub async fn wait(&mut self) {
        let _ = self
            .state_rx
            .wait_for(|x| *x == StreamState::Disconnected)
            .await;
        self.bgh.abort();
    }

    async fn background(
        url: String,
        token: String,
        http_client: Client,
        state_tx: watch::Sender<StreamState>,
        video_rx: MAsyncRx<mpmc::Array<Frame>>,
        audio_rx: MAsyncRx<mpmc::Array<Frame>>,
        cancel_token: CancellationToken,
    ) {
        let mut retry_count = 0;

        // outer loop
        loop {
            if retry_count == 0 {
                let _ = state_tx.send(StreamState::Starting);
            } else {
                let _ = state_tx.send(StreamState::Reconnecting);
            }

            let (first_video_frame, first_audio_frame) = tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("WHIP: 收到取消信号");
                    let _ = state_tx.send(StreamState::Disconnected);
                    return;
                }
                res = async { tokio::join!(video_rx.recv(), audio_rx.recv()) } => {
                    match res {
                        (Ok(vf), Ok(af)) => (vf, af),
                        (Err(_), _) => {
                            tracing::warn!("WHIP: 视频输入管道已关闭，退出流水线");
                            let _ = state_tx.send(StreamState::Disconnected);
                            return;
                        }
                        (_, Err(_)) => {
                            tracing::warn!("WHIP: 音频输入管道已关闭，退出流水线");
                            let _ = state_tx.send(StreamState::Disconnected);
                            return;
                        }
                    }
                }
            };

            let setup_task = async {
                let (peer_connection, video_track, audio_track, video_rtp_sender) =
                    Self::create_webrtc().await?;
                let local_desc = Self::create_sdp_offer(&peer_connection).await?;
                let (resource_url, answer_sdp) =
                    Self::send_sdp_request(&http_client, &local_desc.sdp, &url, &token).await?;
                Self::set_webrtc_remote(&peer_connection, &answer_sdp).await?;

                Ok::<_, Error>((
                    peer_connection,
                    video_track,
                    audio_track,
                    video_rtp_sender,
                    resource_url,
                ))
            };

            let setup_result = tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("收到取消信号");
                    let _ = state_tx.send(StreamState::Disconnected);
                    return ();
                }
                res = setup_task => res,
            };

            let (peer_connection, video_track, audio_track, video_rtp_sender, resource_url) =
                match setup_result {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("连接建立失败: {e:#?} (准备第 {} 次重试)", retry_count + 1);

                        retry_count += 1;
                        let sleep_secs = std::cmp::min(2_u64.pow(retry_count), 30);

                        tokio::select! {
                            _ = cancel_token.cancelled() => return (),
                            _ = time::sleep(Duration::from_secs(sleep_secs)) => continue,
                        }
                    }
                };

            tracing::info!("连接建立成功");
            let _ = state_tx.send(StreamState::Connected);
            retry_count = 0;

            let video_rtp_sender_clone = Arc::clone(&video_rtp_sender);
            // let cmd_tx_clone = cmd_tx.clone();
            let cancel_clone = cancel_token.clone();

            tokio::spawn(async move {
                let mut rtcp_buf = vec![0u8; 1500];

                async fn on_packet(
                    packets: Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>>,
                    _attr: HashMap<usize, usize>,
                ) {
                    for packet in packets {
                        // let any_packet = packet.as_any();
                        let _h = packet.header();
                        tracing::info!("WHIP: video_rtp_sender: {packet}");
                    }
                }
                async fn on_error(e: webrtc::Error) -> bool {
                    tracing::error!("WHIP: video_rtp_sender read error: {e}");
                    true
                }

                loop {
                    tokio::select! {
                        _ = cancel_clone.cancelled() => break,
                        res = video_rtp_sender_clone.read(&mut rtcp_buf) => {
                            match res {
                                Ok((packets, attr)) => on_packet(packets, attr).await,
                                Err(e) => {
                                    let should_break = on_error(e).await;
                                    if should_break {
                                        break;
                                    }
                                },
                            };
                        }
                    }
                }
            });

            let video_sample = webrtc::media::Sample {
                data: first_video_frame.data,
                duration: first_video_frame.dur,
                timestamp: std::time::SystemTime::from(first_video_frame.ts),
                ..Default::default()
            };
            let audio_sample = webrtc::media::Sample {
                data: first_audio_frame.data,
                duration: first_audio_frame.dur,
                timestamp: std::time::SystemTime::from(first_audio_frame.ts),
                ..Default::default()
            };

            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("WHIP: 收到取消信号");
                    let _ = state_tx.send(StreamState::Disconnected);
                    return;
                }
                res = async { tokio::join!(
                    video_track.write_sample(&video_sample), audio_track.write_sample(&audio_sample)
                ) } => {
                    match res {
                        (Ok(()), Ok(())) => {
                            tracing::info!("WHIP: 已写入首帧");
                        },
                        (r1, r2) => {
                            tracing::error!("写入首帧失败: video {r1:?}, audio {r2:?}");
                        }
                    }
                }
            };

            let (webrtc_failed_tx, mut webrtc_failed_rx) =
                tokio_mpsc::channel::<RTCPeerConnectionState>(1);
            peer_connection.on_peer_connection_state_change(Box::new(move |s| {
                use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                if s == RTCPeerConnectionState::Failed
                    || s == RTCPeerConnectionState::Disconnected
                    || s == RTCPeerConnectionState::Closed
                {
                    let _ = webrtc_failed_tx.try_send(s);
                }
                Box::pin(async {})
            }));

            // inner loop
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        tracing::info!("收到取消信号");
                        Self::stop_streaming(&http_client, &peer_connection, &resource_url, &token).await;
                        let _ = state_tx.send(StreamState::Disconnected);
                        return ();
                    }

                    s = webrtc_failed_rx.recv() => {
                        tracing::warn!("WebRTC P2P 连接意外断开({s:?})，准备重连...");
                        // let _ = state_tx.send(StreamState::Error("连接断开".into()));
                        let _ = state_tx.send(StreamState::Reconnecting);
                        Self::stop_streaming(&http_client, &peer_connection, &resource_url, &token).await;
                        break; // outer loop
                    }

                    Ok(frame) = video_rx.recv() => {
                        let sample = webrtc::media::Sample {
                            data: frame.data,
                            duration: frame.dur,
                            timestamp: std::time::SystemTime::from(frame.ts),
                            ..Default::default()
                        };
                        // tracing::info!("写入视频帧...");
                        if let Err(e) = video_track.write_sample(&sample).await {
                            tracing::error!("写入视频帧失败: {e}");
                        }
                        // tracing::info!("写入视频帧完成...");
                    }

                    Ok(frame) = audio_rx.recv() => {
                        let sample = webrtc::media::Sample {
                            data: frame.data,
                            duration: frame.dur,
                            timestamp: std::time::SystemTime::from(frame.ts),
                            ..Default::default()
                        };
                            // tracing::info!("写入音频帧...");
                        if let Err(e) = audio_track.write_sample(&sample).await {
                            tracing::error!("写入音频帧失败: {e}");
                        }
                        // tracing::info!("写入音频帧完成...");
                    }
                }
            }
        }
    }

    async fn create_webrtc() -> Result<
        (
            Arc<RTCPeerConnection>,
            Arc<TrackLocalStaticSample>,
            Arc<TrackLocalStaticSample>,
            Arc<RTCRtpSender>,
        ),
        Error,
    > {
        let mut me = MediaEngine::default();
        me.register_default_codecs().context(UnhandledWebrtcSnafu)?;
        let api = APIBuilder::new().with_media_engine(me).build();
        let config = RTCConfiguration {
            // ice_servers: vec![
            //     RTCIceServer {
            //         urls: vec!["stun:stun.turnix.io:3478".to_owned()],
            //         ..Default::default()
            //     },
            //     RTCIceServer {
            //         urls: vec![
            //             "turn:eu-central.turnix.io:3478",
            //             "turns:eu-central.turnix.io:443",
            //         ]
            //         .into_iter()
            //         .map(|x| x.to_owned())
            //         .collect::<Vec<String>>(),
            //         credential: "".to_owned(),
            //         username: "".to_owned(),
            //     },
            // ],
            // ice_transport_policy: RTCIceTransportPolicy::All,
            ..Default::default()
        };
        let peer_connection = Arc::new(
            api.new_peer_connection(config)
                .context(UnhandledWebrtcSnafu)
                .await?,
        );
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/H264".to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "rslive".to_owned(),
        ));

        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "audio/opus".to_owned(),
                clock_rate: 48000,
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "rslive".to_owned(),
        ));

        let video_rtp_sender = peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .context(UnhandledWebrtcSnafu)
            .await?;
        let _audio_rtp_sender = peer_connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .context(UnhandledWebrtcSnafu)
            .await?;

        Ok((peer_connection, video_track, audio_track, video_rtp_sender))
    }

    async fn create_sdp_offer(
        peer_connection: &RTCPeerConnection,
    ) -> Result<RTCSessionDescription, Error> {
        let offer = peer_connection
            .create_offer(None)
            .context(UnhandledWebrtcSnafu)
            .await?;
        let mut gather_complete = peer_connection.gathering_complete_promise().await;
        peer_connection
            .set_local_description(offer)
            .context(UnhandledWebrtcSnafu)
            .await?;
        let _ = gather_complete.recv().await;
        let local_desc = peer_connection.local_description().await.unwrap();
        Ok(local_desc)
    }

    async fn send_sdp_request(
        http_client: &Client,
        sdp: &String,
        url: &String,
        token: &String,
    ) -> Result<(String, String), Error> {
        // tracing::info!("SDP Offer:\n{}", sdp);

        let resp = http_client
            .post(url)
            .header("Authorization", token)
            .header("Content-Type", "application/sdp")
            .body::<String>(sdp.into())
            .send()
            .context(HttpConnectionSnafu)
            .await?;

        ensure!(
            resp.status().is_success(),
            RequestSnafu {
                status: resp.status(),
                body: resp.text().context(HttpConnectionSnafu).await?
            }
        );

        let resource_url = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|h| h.to_str().ok())
            .map(|s| resolve_url(url, s))
            .unwrap_or_else(|| {
                tracing::warn!("server did not return a Location header");
                url.to_string()
            });

        let answer_sdp = resp.text().context(HttpConnectionSnafu).await?;

        // tracing::info!("SDP Answer:\n{}", answer_sdp);

        Ok((resource_url, answer_sdp))
    }

    async fn set_webrtc_remote(
        peer_connection: &RTCPeerConnection,
        answer_sdp: &String,
    ) -> Result<(), Error> {
        let answer =
            RTCSessionDescription::answer(answer_sdp.to_string()).context(UnhandledWebrtcSnafu)?;
        peer_connection
            .set_remote_description(answer)
            .context(UnhandledWebrtcSnafu)
            .await?;
        Ok(())
    }

    async fn stop_streaming(
        http_client: &Client,
        peer_connection: &RTCPeerConnection,
        resource_url: &String,
        token: &String,
    ) {
        let delete_res = http_client
            .delete(resource_url)
            .header("Authorization", token)
            .send()
            .await;

        match delete_res {
            Ok(res) if res.status().is_success() => {
                tracing::info!("WHIP session is closed by sending a DELETE request");
            }
            Ok(res) => tracing::warn!(
                "the server responds with {} during the WHIP session closure",
                res.status()
            ),
            Err(e) => tracing::error!("failed to send a DELETE request to server: {}", e),
        }

        if let Err(e) = peer_connection.close().await {
            tracing::error!("failed to close WebRTC connection: {}", e);
        }
    }
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("unhandled webrtc error"))]
    UnhandledWebrtcError {
        source: webrtc::Error,
        backtrace: snafu::Backtrace,
    },
    #[snafu(display("http connection error"))]
    HttpConnectionError {
        source: reqwest::Error,
        backtrace: snafu::Backtrace,
    },
    #[snafu(display("request error: {status}: {body}"))]
    RequestError {
        status: StatusCode,
        body: String,
        backtrace: snafu::Backtrace,
    },
}

fn resolve_url(base_url: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }

    let origin = base_url
        .find("://")
        .and_then(|scheme_end| {
            let after_scheme = &base_url[scheme_end + 3..];
            let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
            Some(format!(
                "{}://{}",
                &base_url[..scheme_end],
                &after_scheme[..host_end]
            ))
        })
        .unwrap_or_default();

    if location.starts_with('/') {
        format!("{}{}", origin, location)
    } else {
        let base_dir = base_url
            .rfind('/')
            .map(|i| &base_url[..=i])
            .unwrap_or(base_url);
        format!("{}{}", base_dir, location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

    #[test]
    fn test_resolve_url() {
        assert_eq!(
            resolve_url("https://example.com/whip", "https://other.com/res/1"),
            "https://other.com/res/1"
        );
        assert_eq!(
            resolve_url("https://example.com/whip", "/mystream/whip/abc"),
            "https://example.com/mystream/whip/abc"
        );
        assert_eq!(
            resolve_url("https://example.com/whip/", "abc"),
            "https://example.com/whip/abc"
        );
        assert_eq!(
            resolve_url("https://example.com:8080/whip", "/res/1"),
            "https://example.com:8080/res/1"
        );
    }

    #[tokio::test]
    async fn whip_streamer() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .unwrap();
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer())
            .init();

        let o = WhipStreamerOpt {
            url: "https://stream.place".to_owned(),
            token: "Bearer xxx".to_owned(),
        };

        let (video_tx, video_rx) = crossfire::mpmc::bounded_async::<Frame>(5);
        let (audio_tx, audio_rx) = crossfire::mpmc::bounded_async::<Frame>(50);

        let mut s = WhipStreamer::new(&o, video_rx, audio_rx);

        tokio::time::sleep(Duration::from_secs(5)).await;

        for _ in 0..10 {
            let now = chrono::Utc::now();
            let f = Frame {
                data: Bytes::new(),
                dur: Duration::from_secs(1),
                ts: now,
            };
            let _ = audio_tx.send(f.clone()).await;
            let _ = video_tx.send(f).await;

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
        s.close();
        s.wait().await;
    }
}
