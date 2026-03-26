pub mod audio_encoder;
pub mod bevy_render;
pub mod video_encoder;
pub mod whip;

/// Generic Frame
#[derive(Clone)]
pub struct Frame {
    pub data: bytes::Bytes,
    pub dur: std::time::Duration,
    pub ts: chrono::DateTime<chrono::Utc>,
}
