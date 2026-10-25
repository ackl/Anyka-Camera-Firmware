//! Clean-room RTSP 1.0 server.
//!
//! Fixed API — `main.rs` is written against exactly this, so keep the
//! signature: `serve(Cfg, Arc<dyn VideoSource>) -> io::Result<()>`, blocking,
//! one thread per connection, publishers keep running until the socket dies.
//!
//! Handles what a camera client actually sends (ffmpeg, VLC, ONVIF recorders,
//! our own `tools/rust/camweb` client): OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE,
//! TEARDOWN, GET_PARAMETER, SET_PARAMETER; RTP over TCP (interleaved, `$`
//! framed) and RTP over UDP; H.264 (PT 96, FU-A + single NAL) and G.711 a-law
//! (PT 8) when the source provides audio.

use crate::source::VideoSource;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

/// Server configuration.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub listen: SocketAddr,
    pub server_name: String,
}

impl Cfg {
    pub fn new(listen: SocketAddr) -> Cfg {
        Cfg {
            listen,
            server_name: format!("camrtsp/{}", crate::media::VERSION),
        }
    }
}

pub fn serve(_cfg: Cfg, _source: Arc<dyn VideoSource>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "rtsp server not implemented yet",
    ))
}
