use bevy::{DefaultPlugins, app::PluginGroup};

use crossfire::{mpmc, mpsc};
use rslive::{
    Frame, bevy_render,
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
    //     token: "Bearer xxx".to_string(),
    // };

    let whip_opt = WhipStreamerOpt {
        url: "http://127.0.0.1:8889/mystream/whip".to_string(),
        token: "".to_string(),
    };
    let mut streamer = WhipStreamer::new(&whip_opt, video_encoded_rx, audio_encoded_rx);

    let (video_raw_tx_async, video_raw_rx_async) = mpmc::bounded_async::<Frame>(10);
    let video_raw_tx_blocking = video_raw_tx_async.into_blocking();

    let video_encoder = H264Encoder::new(video_raw_rx_async, video_encoded_tx)?;

    tokio::task::spawn_blocking(move || {
        bevy_render::bevy_app(video_raw_tx_blocking);
    });

    tokio::signal::ctrl_c().await?;

    video_encoder.close();
    streamer.close();
    streamer.wait().await;

    Ok(())
}
