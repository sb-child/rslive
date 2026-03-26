use bevy::{DefaultPlugins, app::PluginGroup};

use crossfire::{mpmc, mpsc};
use rslive::{
    Frame,
    audio_encoder::OpusEncoder,
    bevy_render,
    video_encoder::H264Encoder,
    whip::{WhipStreamer, WhipStreamerOpt},
};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer())
        .init();
    app().await
}

async fn app() -> anyhow::Result<()> {
    let (video_encoded_tx, video_encoded_rx) = mpsc::bounded_async::<Frame>(5);
    let (audio_encoded_tx, audio_encoded_rx) = mpsc::bounded_async::<Frame>(50);

    // let whip_opt = WhipStreamerOpt {
    //     url: "https://stream.place".to_string(),
    //     token: "Bearer z4Sj4XcsAw2uNuKwxQnGqWYkAebmCbaVjyCav9GR2zfwgwJ8ET7A3KmSusxvPF2GrKtLoiKzYHw7coCSQ2uYNZzYk".to_string(),
    // };

    let whip_opt = WhipStreamerOpt {
        url: "http://127.0.0.1:8889/mystream/whip".to_string(),
        token: "".to_string(),
    };
    let mut streamer = WhipStreamer::new(&whip_opt, video_encoded_rx, audio_encoded_rx);

    let (video_raw_tx_async, video_raw_rx_async) = mpmc::bounded_async::<Frame>(10);
    let video_raw_tx_blocking = video_raw_tx_async.into_blocking();

    let (audio_raw_tx_async, audio_raw_rx_async) = mpmc::bounded_async::<Frame>(50);

    let video_encoder = H264Encoder::new(video_raw_rx_async, video_encoded_tx)?;
    let audio_encoder = OpusEncoder::new(audio_raw_rx_async, audio_encoded_tx)?;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
        let silence_data = bytes::Bytes::from(vec![0u8; 3840]);
        tracing::info!("静音测试发生器已启动");
        loop {
            interval.tick().await;

            let frame = Frame {
                data: silence_data.clone(),
                dur: std::time::Duration::from_millis(20),
                ts: chrono::Utc::now(),
            };
            if let Err(e) = audio_raw_tx_async.try_send(frame) {
                match e {
                    crossfire::TrySendError::Full(_) => {
                        tracing::debug!("音频流拥堵，主动丢弃 20ms 静音");
                    }
                    crossfire::TrySendError::Disconnected(_) => {
                        tracing::info!("音频输入管道已关闭，退出静音发生器");
                        break;
                    }
                }
            }
        }
    });

    tokio::task::spawn_blocking(move || {
        bevy_render::bevy_app(video_raw_tx_blocking);
    });

    tokio::signal::ctrl_c().await?;

    audio_encoder.close();
    video_encoder.close();
    streamer.close();
    streamer.wait().await;

    Ok(())
}
