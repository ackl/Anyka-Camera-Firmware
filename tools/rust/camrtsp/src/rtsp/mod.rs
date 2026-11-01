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
//!
//! Layout:
//! * `msg`       — request parsing, response rendering, URL → stream/track
//! * `sdp`       — the session description and profile-level-id
//! * `transport` — the `Transport:` header
//! * `rtp`       — RTP packetization (single NAL / FU-A), RTCP SR, RTCP checks
//! * `session`   — per-session queues, the source fan-out, the sender thread
//! * `conn`      — one control connection and the session it owns
//!
//! Threads: the caller's thread accepts; one `rtsp-conn` thread per
//! connection; one `rtsp-send` thread per playing session; plus one
//! `rtsp-video` (and, with audio, one `rtsp-audio`) fan-out thread that is the
//! only reader of the source. The fan-out copies `Arc`s into bounded
//! per-session queues and never blocks on a client, so a dead or slow viewer
//! costs only its own queue (dropped and counted), never the publisher's time
//! or another viewer's frames.
//!
//! No 64-bit atomics anywhere: the AK3918 is ARMv5TE. Shared state is
//! `Mutex`/`Condvar` only, like `source::Hub`.

mod conn;
mod msg;
mod rtp;
mod sdp;
mod session;
mod transport;

#[cfg(test)]
mod tests;

use crate::source::VideoSource;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

/// Stack for every thread we spawn. Nothing here recurses or keeps big
/// arrays on the stack; the default 2 MiB per thread is wasted address space
/// on a 36 MB board.
const STACK: usize = 256 * 1024;

/// Server configuration.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub listen: SocketAddr,
    pub server_name: String,
    /// Concurrent control connections; more are refused with 503.
    pub max_conns: usize,
    /// Seconds advertised as `Session: …;timeout=N`. Connections that show no
    /// sign of life for this long (plus a little grace) are reaped.
    pub session_timeout: u32,
}

impl Cfg {
    pub fn new(listen: SocketAddr) -> Cfg {
        Cfg {
            listen,
            server_name: format!("camrtsp/{}", crate::media::VERSION),
            max_conns: 16,
            session_timeout: 60,
        }
    }
}

/// What every connection thread shares.
struct Server {
    cfg: Cfg,
    source: Arc<dyn VideoSource>,
    fanout: Arc<session::Fanout>,
    conns: Mutex<usize>,
    /// SDP `o=` session id: stable for the life of the process.
    origin: u64,
}

impl Server {
    fn enter(&self) -> bool {
        let mut n = lock(&self.conns);
        if *n >= self.cfg.max_conns {
            return false;
        }
        *n += 1;
        true
    }

    fn leave(&self) {
        let mut n = lock(&self.conns);
        *n = n.saturating_sub(1);
    }
}

/// Bind `cfg.listen` and serve forever.
pub fn serve(cfg: Cfg, source: Arc<dyn VideoSource>) -> io::Result<()> {
    let listener = TcpListener::bind(cfg.listen)?;
    serve_listener(listener, cfg, source)
}

/// Serve on an already-bound listener (tests bind `127.0.0.1:0` and read the
/// port back before handing it over). `cfg.listen` is not used here.
pub fn serve_listener(
    listener: TcpListener,
    cfg: Cfg,
    source: Arc<dyn VideoSource>,
) -> io::Result<()> {
    let fanout = session::Fanout::start(source.clone())?;
    let srv = Arc::new(Server {
        cfg,
        source,
        fanout,
        conns: Mutex::new(0),
        origin: rand_u64() >> 16,
    });
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(x) => x,
            Err(_) => {
                // EMFILE/ENFILE/ECONNABORTED: back off instead of spinning.
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if !srv.enter() {
            conn::refuse(stream);
            continue;
        }
        let s2 = srv.clone();
        let spawned = thread::Builder::new()
            .name("rtsp-conn".into())
            .stack_size(STACK)
            .spawn(move || conn::run(s2, stream, peer));
        if spawned.is_err() {
            srv.leave();
        }
    }
}

/// Lock that survives a poisoned mutex (a panicking test thread must not
/// take the server with it; release builds abort on panic anyway).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Unpredictable 64-bit value from std alone: SipHash keyed by `RandomState`
/// (seeded from the OS once per process) over a counter and the clock. Used
/// for session ids, SSRCs, initial sequence numbers and timestamps.
fn rand_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    static CTR: Mutex<u64> = Mutex::new(0);
    let n = {
        let mut g = lock(&CTR);
        *g = g.wrapping_add(1);
        *g
    };
    let mut h = RandomState::new().build_hasher();
    h.write_u64(n);
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u64(d.as_secs());
        h.write_u32(d.subsec_nanos());
    }
    h.finish()
}
