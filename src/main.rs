use bevy::{DefaultPlugins, app::PluginGroup};

use rslive::bevy_render;
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
    tokio::task::spawn_blocking(|| {
        bevy_render::bevy_app();
    })
    .await
    .unwrap();
    Ok(())
}
