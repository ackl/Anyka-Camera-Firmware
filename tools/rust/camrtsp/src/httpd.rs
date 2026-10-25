//! HTTP side: MJPEG live view + a small panel, so the camera can serve its own
//! pixels instead of firefly doing it. Fed by the same `VideoSource`; JPEG
//! frames arrive as `VideoFrame`s whose single NAL is a complete JPEG image.

use crate::source::{Hub, VideoSource};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Cfg {
    pub listen: SocketAddr,
    /// Mount path for the multipart stream.
    pub stream_path: String,
}

pub fn serve(_cfg: Cfg, _source: Arc<dyn VideoSource>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "http/mjpeg server not implemented yet",
    ))
}

/// Placeholder so callers can construct an MJPEG hub uniformly.
pub fn mjpeg_hub(name: &str) -> Arc<Hub> {
    crate::source::empty(name)
}
