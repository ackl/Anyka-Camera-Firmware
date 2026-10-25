//! The hardware encoder side: where H.264 access units are supposed to come
//! from. See `tools/rust/camrtsp/README.md` — the vendor MPI libraries are
//! uClibc dynamic objects, so the pump that calls them lives behind a socket
//! and this process only ever reads frames.
//!
//! Wire format (unix SOCK_STREAM, or TCP when `--pump host:port`):
//!   magic "CAMRTSP1" | u32 flags (bit0 alaw audio) | u32 fps | u32 w | u32 h
//!   u16 sps_len | sps | u16 pps_len | pps
//!   then repeated: u8 kind (1=video,2=audio) | u64 ts_ms | u32 len | payload
//! Video payloads are Annex-B access units as the encoder emits them.

use crate::media::{AudioInfo, StreamInfo};
use crate::source::Hub;
use std::io;
use std::sync::Arc;

pub fn connect(_addr: &str) -> io::Result<Arc<Hub>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "venc pump client not implemented yet",
    ))
}

pub const _PLACEHOLDER: Option<AudioInfo> = None;
pub const _PLACEHOLDER2: Option<StreamInfo> = None;
