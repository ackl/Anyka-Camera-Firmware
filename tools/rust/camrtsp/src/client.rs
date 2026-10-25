//! Upstream RTSP client: pull H.264 (+ G.711 a-law) from an existing RTSP
//! server and republish every access unit through a [`Hub`].
//!
//! On the camera the upstream is the stock `rtsp` on 127.0.0.1:554, the only
//! thing that can reach the hardware encoder (uClibc MPI libraries). Ported
//! from `tools/rust/camweb/src/rtsp.rs` (same parsers, same quirks), but
//! reshaped around one byte-stream demultiplexer and one access-unit
//! assembler instead of a NAL-at-a-time API.
//!
//! Wire conversation (see `work/rtsp-rust-notes.md`):
//!
//! ```text
//! C: DESCRIBE rtsp://host:554/vs0                -> 200 + SDP
//! C: SETUP    rtsp://host:554/vs0/camera   Transport: RTP/AVP/TCP;unicast;interleaved=0-1
//! C: SETUP    rtsp://host:554/vs0/micphone Transport: ...interleaved=2-3   (PCMA only)
//! C: PLAY     rtsp://host:554/vs0/               -> 200, then '$' frames
//! C: OPTIONS  ... Session: id   every 20 s, reply never awaited
//! ```
//!
//! Quirks tolerated on purpose:
//!
//! * nginx/1.17.8 fronts the server and only routes TCP-interleaved SETUP, so
//!   UDP is never offered. Full URL + `RTSP/1.0` on every request line.
//! * `Session:` is `"<id>; timeout=60"`; only the id is echoed back.
//! * No AUD NALs: an access unit ends at the RTP marker bit (or, if a marker is
//!   lost, when the RTP timestamp changes).
//! * Roughly one mid-stream OPTIONS in ten is never answered, so keepalive
//!   replies are not waited for; late replies are swallowed as text between
//!   `'$'` frames. A `454 Session Not Found` triggers one re-PLAY, a second
//!   one a reconnect.
//! * RTCP arrives on the odd channels and is dropped unparsed.
//!
//! Memory is bounded: one fixed 128 KiB inbound buffer per connection, one
//! access unit under assembly (<= 1 MiB), and the Hub keeps only the latest
//! frame. No 64-bit atomics anywhere (ARMv5TE).

use crate::media::{is_vcl, nal_type, AudioFrame, AudioInfo, StreamInfo, VideoFrame};
use crate::source::{Hub, VideoSource};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// Inbound buffer. Interleaved frames are <= 65539 bytes and replies are
/// capped at [`MAX_REPLY`], so any valid item fits after compaction.
const BUF_CAP: usize = 128 * 1024;
const MAX_REPLY: usize = 60 * 1024;
/// Refuse to assemble an access unit larger than this (drop it instead).
const MAX_AU: usize = 1024 * 1024;
const MAX_AU_NALS: usize = 64;
/// Parameter sets / SEI held over waiting for their picture.
const MAX_HELD_NALS: usize = 8;
/// Consecutive bytes that are neither a frame nor a reply before giving up.
const MAX_GARBAGE: usize = 256 * 1024;

#[derive(Clone)]
struct Cfg {
    /// How long `pull()` waits for the SDP (and SPS/PPS) of the first session.
    sdp_wait: Duration,
    /// Socket read timeout: the tick of the streaming loop.
    io_timeout: Duration,
    /// Per handshake request.
    request_timeout: Duration,
    keepalive: Duration,
    /// No complete access unit for this long -> reconnect.
    watchdog: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// Tests observe every published frame here (the Hub keeps only the last).
    tap: Option<mpsc::Sender<Tap>>,
}

impl Default for Cfg {
    fn default() -> Cfg {
        Cfg {
            sdp_wait: Duration::from_secs(10),
            io_timeout: Duration::from_millis(500),
            request_timeout: Duration::from_secs(5),
            keepalive: Duration::from_secs(20),
            watchdog: Duration::from_secs(10),
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(16),
            tap: None,
        }
    }
}

enum Tap {
    Video(Arc<VideoFrame>),
    Audio(AudioFrame),
}

/// Pull `rtsp://<upstream><path>` in a background thread and return its Hub
/// once the stream description is known (up to ~10 s). The thread then keeps
/// pulling, reconnecting with exponential backoff, until the Hub is stopped or
/// dropped by everyone else.
///
/// `upstream` is `"host:port"` (port defaults to 554), `path` is `"/vs0"`.
pub fn pull(upstream: &str, path: &str) -> io::Result<Arc<Hub>> {
    pull_with(upstream, path, Cfg::default())
}

fn pull_with(upstream: &str, path: &str, cfg: Cfg) -> io::Result<Arc<Hub>> {
    let target = Target::new(upstream, path)?;
    let wait = cfg.sdp_wait;
    let (tx, rx) = mpsc::sync_channel::<io::Result<Arc<Hub>>>(1);
    thread::Builder::new()
        .name("rtsp-pull".into())
        .spawn(move || Puller::new(target, cfg).run(tx))?;
    match rx.recv_timeout(wait + Duration::from_secs(2)) {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "upstream rtsp: no stream description in time",
        )),
    }
}

// ---------------------------------------------------------------------------
// Target URL
// ---------------------------------------------------------------------------

struct Target {
    /// "host:port", for connect().
    addr: String,
    /// rtsp://host:port/vs0 (no trailing slash)
    url: String,
    /// url + "/", base for relative a=control values and PLAY.
    base: String,
    /// "vs0"
    name: String,
}

impl Target {
    fn new(upstream: &str, path: &str) -> io::Result<Target> {
        let up = upstream.trim().trim_start_matches("rtsp://").trim_end_matches('/');
        if up.is_empty() || up.contains('/') || up.contains('@') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("upstream rtsp: bad address {:?} (want host:port)", upstream),
            ));
        }
        // Add the default port unless one is there ("h:p", "[v6]:p").
        let has_port = match up.rsplit_once(':') {
            Some((h, p)) => {
                p.parse::<u16>().is_ok() && (!h.contains(':') || h.ends_with(']'))
            }
            None => false,
        };
        let addr = if has_port {
            up.to_string()
        } else if up.contains(':') && !up.starts_with('[') {
            format!("[{}]:554", up)
        } else {
            format!("{}:554", up)
        };
        let p = path.trim().trim_end_matches('/');
        let p = if p.starts_with('/') { p.to_string() } else { format!("/{}", p) };
        if p.len() < 2 || p.bytes().any(|b| b <= b' ' || b >= 0x7f) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("upstream rtsp: bad path {:?}", path),
            ));
        }
        let url = format!("rtsp://{}{}", addr, p);
        Ok(Target {
            base: format!("{}/", url),
            name: p[1..].to_string(),
            addr,
            url,
        })
    }
}

// ---------------------------------------------------------------------------
// The pulling thread
// ---------------------------------------------------------------------------

struct Puller {
    t: Target,
    cfg: Cfg,
    hub: Option<Arc<Hub>>,
    tx: Option<mpsc::SyncSender<io::Result<Arc<Hub>>>>,
    /// Gives up on the first hub at this point (pull() has returned by then).
    deadline: Instant,
    sps: Vec<u8>,
    pps: Vec<u8>,
    fps: u32,
    /// Decided by the first session; later sessions only feed it.
    audio: Option<AudioInfo>,
    last_vts: u64,
    last_ats: u64,
    last_pub: Option<Instant>,
    /// Something was published in the current session (resets backoff).
    produced: bool,
}

/// Why a session ended without an I/O error.
enum End {
    /// The Hub has no consumers left, or was stopped: the thread exits.
    Abandoned,
}

impl Puller {
    fn new(t: Target, cfg: Cfg) -> Puller {
        Puller {
            deadline: Instant::now() + cfg.sdp_wait,
            t,
            cfg,
            hub: None,
            tx: None,
            sps: Vec::new(),
            pps: Vec::new(),
            fps: 15,
            audio: None,
            last_vts: 0,
            last_ats: 0,
            last_pub: None,
            produced: false,
        }
    }

    fn run(mut self, tx: mpsc::SyncSender<io::Result<Arc<Hub>>>) {
        self.tx = Some(tx);
        let mut backoff = self.cfg.backoff_min;
        loop {
            self.produced = false;
            let err = match self.session() {
                Ok(End::Abandoned) => return,
                Err(e) => e,
            };
            if self.abandoned() {
                return;
            }
            if self.hub.is_none() && Instant::now() >= self.deadline {
                if let Some(tx) = self.tx.take() {
                    let _ = tx.send(Err(err));
                }
                return;
            }
            if self.produced {
                backoff = self.cfg.backoff_min;
            }
            eprintln!(
                "camrtsp: upstream {}: {}; retry in {} ms",
                self.t.url,
                err,
                backoff.as_millis()
            );
            if !self.sleep(backoff) {
                return;
            }
            backoff = (backoff * 2).min(self.cfg.backoff_max);
        }
    }

    /// Nobody can ever read from the Hub again.
    fn abandoned(&self) -> bool {
        match &self.hub {
            Some(h) => Arc::strong_count(h) == 1 || !h.is_running(),
            None => false,
        }
    }

    /// Sleep in io_timeout slices; false if abandoned meanwhile.
    fn sleep(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        loop {
            if self.abandoned() {
                return false;
            }
            let now = Instant::now();
            if now >= until {
                return true;
            }
            thread::sleep((until - now).min(self.cfg.io_timeout));
        }
    }

    /// One connection: handshake, then stream until something breaks.
    fn session(&mut self) -> io::Result<End> {
        let mut c = Conn::open(&self.t.addr, &self.cfg)?;
        let url = self.t.url.clone();
        let d = c.request("DESCRIBE", &url, &[("Accept", "application/sdp")])?;
        if d.status != 200 {
            return Err(other(format!("DESCRIBE returned {}", d.status)));
        }
        let tracks = parse_sdp(&String::from_utf8_lossy(&d.body));
        let video = tracks
            .iter()
            .find(|t| t.media == "video" && t.encoding == "H264")
            .ok_or_else(|| other("SDP has no H264 video track".into()))?
            .clone();
        let audio = tracks
            .iter()
            .find(|t| t.media == "audio" && t.encoding == "PCMA")
            .cloned();

        let vchan = c.setup(&join_control(&self.t.base, &video.control), 0)?;
        let mut achan = None;
        if let Some(a) = &audio {
            if self.hub.is_none() || self.audio.is_some() {
                match c.setup(&join_control(&self.t.base, &a.control), 2) {
                    Ok(ch) => achan = Some(ch),
                    Err(e) => eprintln!("camrtsp: upstream audio SETUP: {}; video only", e),
                }
            }
        }
        let base = self.t.base.clone();
        let p = c.request("PLAY", &base, &[("Range", "npt=0.000-")])?;
        if p.status != 200 {
            return Err(other(format!("PLAY returned {}", p.status)));
        }

        if self.hub.is_none() {
            if let Some((s, pp)) = sprop(&video.fmtp) {
                self.sps = s;
                self.pps = pp;
            }
            if video.framerate > 0 {
                self.fps = video.framerate;
            }
            self.audio = match (&audio, achan) {
                (Some(a), Some(_)) => Some(AudioInfo {
                    rate: if a.clock > 0 { a.clock } else { 8000 },
                    channels: a.channels.max(1),
                    alaw: true,
                }),
                _ => None,
            };
            self.maybe_make_hub();
        }
        if self.audio.is_none() {
            achan = None;
        }
        let aclock = self.audio.as_ref().map(|a| a.rate / 1000).unwrap_or(8).max(1);
        self.stream(&mut c, vchan, achan, aclock)
    }

    fn maybe_make_hub(&mut self) {
        if self.hub.is_some() || self.sps.is_empty() || self.pps.is_empty() {
            return;
        }
        let (width, height) = dimensions(&self.sps).unwrap_or((0, 0));
        let hub = Hub::new(Arc::new(StreamInfo {
            name: self.t.name.clone(),
            sps: self.sps.clone(),
            pps: self.pps.clone(),
            width,
            height,
            fps: self.fps,
            audio: self.audio.clone(),
        }));
        if let Some(tx) = self.tx.take() {
            if tx.send(Ok(hub.clone())).is_err() {
                return; // pull() gave up; hub stays None and the deadline ends us
            }
        }
        self.hub = Some(hub);
    }

    fn stream(&mut self, c: &mut Conn, vchan: u8, achan: Option<u8>, aclock: u32) -> io::Result<End> {
        let play_at = Instant::now();
        // Re-anchor every session after the last published timestamp, plus
        // the wall-clock gap, so ts_ms stays monotonic across reconnects.
        let base = match self.last_pub {
            Some(t) => self.last_vts.max(self.last_ats) + t.elapsed().as_millis() as u64 + 1,
            None => 0,
        };
        let mut vclk = Clock::new(90);
        let mut aclk = Clock::new(aclock);
        let mut asm = Assembler::default();
        let mut out: Vec<Au> = Vec::with_capacity(2);
        let mut last_au = Instant::now();
        let mut next_ka = Instant::now() + self.cfg.keepalive;
        let mut replay_cseq: Option<u32> = None;
        let url = self.t.url.clone();
        let play_url = self.t.base.clone();
        loop {
            if self.abandoned() {
                return Ok(End::Abandoned);
            }
            let now = Instant::now();
            if now >= next_ka {
                c.send("OPTIONS", &url, &[])?;
                next_ka = now + self.cfg.keepalive;
            }
            if now.duration_since(last_au) > self.cfg.watchdog {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no video for too long"));
            }
            if self.hub.is_none() && now >= self.deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no SPS/PPS from upstream"));
            }
            match c.poll()? {
                Ev::Idle => {}
                Ev::Reply(r) => {
                    if r.status == 454 {
                        if replay_cseq.is_some() && replay_cseq == r.cseq() {
                            return Err(other("session lost (454 after re-PLAY)".into()));
                        }
                        eprintln!("camrtsp: upstream said 454, re-PLAY");
                        replay_cseq = Some(c.send("PLAY", &play_url, &[("Range", "npt=0.000-")])?);
                    } else if replay_cseq.is_some() && replay_cseq == r.cseq() {
                        replay_cseq = None;
                    }
                }
                Ev::Frame(ch, lo, hi) => {
                    if ch == vchan {
                        if let Some(pkt) = Rtp::parse(&c.buf[lo..hi]) {
                            asm.push(&pkt, &mut out);
                        }
                        for au in out.drain(..) {
                            last_au = Instant::now();
                            let anchor = base + play_at.elapsed().as_millis() as u64;
                            let ts = vclk.map(au.ts, anchor);
                            self.publish_video(au.nals, ts);
                        }
                    } else if Some(ch) == achan {
                        if let Some(pkt) = Rtp::parse(&c.buf[lo..hi]) {
                            if !pkt.payload.is_empty() {
                                let anchor = base + play_at.elapsed().as_millis() as u64;
                                let ts = aclk.map(pkt.ts, anchor);
                                self.publish_audio(pkt.payload.to_vec(), ts);
                            }
                        }
                    }
                    // anything else: RTCP or a channel we never set up
                }
            }
        }
    }

    fn publish_video(&mut self, nals: Vec<Vec<u8>>, ts: u64) {
        for n in &nals {
            match nal_type(n) {
                7 if n[..] != self.sps[..] => self.sps = n.clone(),
                8 if n[..] != self.pps[..] => self.pps = n.clone(),
                _ => {}
            }
        }
        self.maybe_make_hub();
        let Some(hub) = self.hub.clone() else {
            return; // still waiting for parameter sets
        };
        let keyframe = nals.iter().any(|n| nal_type(n) == 5)
            || matches!(nals.first().map(|n| nal_type(n)), Some(7) | Some(8));
        let ts = ts.max(self.last_vts);
        hub.publish(VideoFrame { nals, keyframe, ts_ms: ts, seq: 0 });
        self.last_vts = ts;
        self.last_pub = Some(Instant::now());
        self.produced = true;
        if let Some(tap) = &self.cfg.tap {
            if let Some(f) = hub.latest() {
                let _ = tap.send(Tap::Video(f));
            }
        }
    }

    fn publish_audio(&mut self, data: Vec<u8>, ts: u64) {
        let Some(hub) = self.hub.clone() else {
            return;
        };
        let ts = ts.max(self.last_ats);
        let f = AudioFrame { data, ts_ms: ts, seq: 0 };
        if let Some(tap) = &self.cfg.tap {
            let _ = tap.send(Tap::Audio(f.clone()));
        }
        hub.publish_audio(f);
        self.last_ats = ts;
        self.last_pub = Some(Instant::now());
    }
}

fn other(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

/// Width/height from an SPS (H.264 7.3.2.1.1), only when plausible for a
/// camera. Own parser: `media::sps_dimensions` reads one bit too many after
/// the NAL header (see src/pump-NEEDS.md).
fn dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    struct Bits {
        d: Vec<u8>,
        pos: usize,
    }
    impl Bits {
        fn bit(&mut self) -> Option<u32> {
            let b = (*self.d.get(self.pos / 8)? >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            Some(b as u32)
        }
        fn bits(&mut self, n: u32) -> Option<u32> {
            let mut v = 0;
            for _ in 0..n {
                v = (v << 1) | self.bit()?;
            }
            Some(v)
        }
        fn ue(&mut self) -> Option<u32> {
            let mut z = 0;
            while self.bit()? == 0 {
                z += 1;
                if z > 31 {
                    return None;
                }
            }
            Some(((1u64 << z) - 1 + self.bits(z)? as u64).min(u32::MAX as u64) as u32)
        }
        fn se(&mut self) -> Option<i32> {
            let v = self.ue()? as i64;
            Some(if v % 2 == 1 { (v + 1) / 2 } else { -(v / 2) } as i32)
        }
    }
    if sps.len() < 4 || nal_type(sps) != 7 {
        return None;
    }
    // Drop emulation-prevention bytes (00 00 03) and the NAL header.
    let mut d = Vec::with_capacity(sps.len());
    let mut zeros = 0;
    for &b in &sps[1..] {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        d.push(b);
    }
    let mut b = Bits { d, pos: 0 };
    let profile = b.bits(8)?;
    b.bits(16)?; // constraint flags, level_idc
    b.ue()?; // seq_parameter_set_id
    let mut chroma = 1;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = b.ue()?;
        if chroma == 3 {
            b.bit()?; // separate_colour_plane_flag
        }
        b.ue()?; // bit_depth_luma
        b.ue()?; // bit_depth_chroma
        b.bit()?; // qpprime_y_zero_transform_bypass
        if b.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.bit()? == 1 {
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()?.min(255) {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?; // gaps_in_frame_num_value_allowed
    let w_mbs = b.ue()?.checked_add(1)?;
    let h_units = b.ue()?.checked_add(1)?;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?; // direct_8x8_inference
    let mut crop = [0u32; 4];
    if b.bit()? == 1 {
        for c in crop.iter_mut() {
            *c = b.ue()?;
        }
    }
    let (cx, cy) = match chroma {
        0 | 3 => (1, 2 - frame_mbs_only),
        2 => (2, 2 - frame_mbs_only),
        _ => (2, 2 * (2 - frame_mbs_only)),
    };
    let w = w_mbs.checked_mul(16)?.checked_sub(crop[0].checked_add(crop[1])?.checked_mul(cx)?)?;
    let h = h_units
        .checked_mul(16 * (2 - frame_mbs_only))?
        .checked_sub(crop[2].checked_add(crop[3])?.checked_mul(cy)?)?;
    if (64..=4096).contains(&w) && (64..=4096).contains(&h) {
        Some((w, h))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// RTP timestamps -> milliseconds
// ---------------------------------------------------------------------------

/// Extends a 32-bit RTP clock and maps it onto our millisecond timeline.
struct Clock {
    ticks_per_ms: u32,
    started: bool,
    last: u32,
    ext: u64,
    anchor: u64,
}

impl Clock {
    fn new(ticks_per_ms: u32) -> Clock {
        Clock { ticks_per_ms: ticks_per_ms.max(1), started: false, last: 0, ext: 0, anchor: 0 }
    }

    fn map(&mut self, ts: u32, anchor_now: u64) -> u64 {
        let tpm = self.ticks_per_ms;
        if !self.started {
            self.started = true;
            self.last = ts;
            self.anchor = anchor_now;
        } else {
            let d = ts.wrapping_sub(self.last) as i32;
            let jump = tpm.saturating_mul(10_000) as i64; // 10 s
            if (d as i64) > jump || (d as i64) < -jump {
                // Discontinuity (upstream restarted its clock): step one frame.
                self.ext += tpm as u64 * 66;
                self.last = ts;
            } else if d > 0 {
                self.ext += d as u64;
                self.last = ts;
            }
            // small step backwards (B-frames, reordering): keep the last time
        }
        self.anchor + self.ext / tpm as u64
    }
}

// ---------------------------------------------------------------------------
// RTP -> access units
// ---------------------------------------------------------------------------

struct Rtp<'a> {
    seq: u16,
    ts: u32,
    marker: bool,
    payload: &'a [u8],
}

impl<'a> Rtp<'a> {
    /// Strip the RTP header (+CSRC list, +extension, +padding). Every length
    /// that comes off the wire is bounds-checked.
    fn parse(pkt: &'a [u8]) -> Option<Rtp<'a>> {
        if pkt.len() < 12 || pkt[0] >> 6 != 2 {
            return None;
        }
        let cc = (pkt[0] & 0x0f) as usize;
        let mut off = 12 + cc * 4;
        if pkt[0] & 0x10 != 0 {
            if pkt.len() < off + 4 {
                return None;
            }
            let words = ((pkt[off + 2] as usize) << 8) | pkt[off + 3] as usize;
            off += 4 + words * 4;
        }
        let mut end = pkt.len();
        if pkt[0] & 0x20 != 0 {
            let pad = pkt[end - 1] as usize;
            end = end.checked_sub(pad)?;
        }
        if off > end {
            return None;
        }
        Some(Rtp {
            seq: u16::from_be_bytes([pkt[2], pkt[3]]),
            ts: u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
            marker: pkt[1] & 0x80 != 0,
            payload: &pkt[off..end],
        })
    }
}

struct Au {
    nals: Vec<Vec<u8>>,
    ts: u32,
}

/// H.264 depacketizer (single NAL, STAP-A, FU-A) that groups NALs into
/// access units: an AU ends at the marker bit, or when the RTP timestamp
/// moves on while a picture is open (lost marker).
#[derive(Default)]
struct Assembler {
    nals: Vec<Vec<u8>>,
    bytes: usize,
    ts: u32,
    has_vcl: bool,
    /// Too big: swallow the rest of this AU.
    overflow: bool,
    fu: Vec<u8>,
    in_fu: bool,
    last_seq: Option<u16>,
}

impl Assembler {
    fn push(&mut self, p: &Rtp, out: &mut Vec<Au>) {
        if let Some(l) = self.last_seq {
            if p.seq != l.wrapping_add(1) && self.in_fu {
                // A fragment went missing: that NAL is unusable.
                self.in_fu = false;
                self.fu.clear();
            }
        }
        self.last_seq = Some(p.seq);
        if (self.has_vcl || self.overflow) && p.ts != self.ts {
            self.flush(out);
        }
        self.ts = p.ts;
        let pl = p.payload;
        if pl.is_empty() {
            return;
        }
        match pl[0] & 0x1f {
            1..=23 => self.add(pl.to_vec()),
            24 => {
                // STAP-A: (u16 BE size, NAL) repeated
                let mut i = 1;
                while i + 2 <= pl.len() {
                    let n = ((pl[i] as usize) << 8) | pl[i + 1] as usize;
                    i += 2;
                    if n == 0 || i + n > pl.len() {
                        break;
                    }
                    self.add(pl[i..i + n].to_vec());
                    i += n;
                }
            }
            28 if pl.len() >= 2 => {
                let fu = pl[1];
                if fu & 0x80 != 0 {
                    self.fu.clear();
                    self.fu.push((pl[0] & 0xe0) | (fu & 0x1f));
                    self.in_fu = true;
                }
                if self.in_fu {
                    if self.fu.len() + pl.len() > MAX_AU {
                        self.in_fu = false;
                        self.fu = Vec::new();
                        self.overflow = true;
                    } else {
                        self.fu.extend_from_slice(&pl[2..]);
                        if fu & 0x40 != 0 {
                            self.in_fu = false;
                            let nal = std::mem::take(&mut self.fu);
                            self.add(nal);
                        }
                    }
                }
            }
            // STAP-B, MTAP, FU-B, reserved: never produced by this encoder
            _ => {}
        }
        if p.marker {
            self.flush(out);
        }
    }

    fn add(&mut self, nal: Vec<u8>) {
        let t = nal_type(&nal);
        if t == 12 || t == 0 {
            return; // filler / invalid: nothing downstream wants them
        }
        if self.nals.len() >= MAX_AU_NALS || self.bytes + nal.len() > MAX_AU {
            self.overflow = true;
            return;
        }
        if is_vcl(t) {
            self.has_vcl = true;
        }
        self.bytes += nal.len();
        self.nals.push(nal);
    }

    fn flush(&mut self, out: &mut Vec<Au>) {
        if self.overflow {
            self.reset();
            return;
        }
        if !self.has_vcl {
            // Parameter sets / SEI that ended a packet with the marker set:
            // they belong to the next picture. Keep a few, not forever.
            if self.nals.len() > MAX_HELD_NALS {
                self.reset();
            }
            return;
        }
        out.push(Au { nals: std::mem::take(&mut self.nals), ts: self.ts });
        self.reset();
    }

    fn reset(&mut self) {
        self.nals.clear();
        self.bytes = 0;
        self.has_vcl = false;
        self.overflow = false;
    }
}

// ---------------------------------------------------------------------------
// One RTSP connection: requests out, '$' frames and text replies in
// ---------------------------------------------------------------------------

enum Ev {
    /// Nothing arrived within the read timeout.
    Idle,
    Reply(Response),
    /// Interleaved frame: channel and payload range in `Conn::buf`, valid
    /// until the next `poll()`.
    Frame(u8, usize, usize),
}

struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    lo: usize,
    hi: usize,
    garbage: usize,
    cseq: u32,
    session: String,
    request_timeout: Duration,
    teardown_url: Option<String>,
}

impl Conn {
    fn open(addr: &str, cfg: &Cfg) -> io::Result<Conn> {
        let mut last = None;
        let mut s = None;
        for a in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&a, cfg.request_timeout) {
                Ok(x) => {
                    s = Some(x);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let s = match s {
            Some(s) => s,
            None => {
                return Err(last.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "address resolved to nothing")
                }))
            }
        };
        s.set_read_timeout(Some(cfg.io_timeout))?;
        s.set_write_timeout(Some(cfg.request_timeout))?;
        let _ = s.set_nodelay(true);
        Ok(Conn {
            s,
            buf: vec![0; BUF_CAP],
            lo: 0,
            hi: 0,
            garbage: 0,
            cseq: 0,
            session: String::new(),
            request_timeout: cfg.request_timeout,
            teardown_url: None,
        })
    }

    /// Write one request; returns its CSeq. Never waits for the reply.
    fn send(&mut self, method: &str, uri: &str, extra: &[(&str, &str)]) -> io::Result<u32> {
        self.cseq = self.cseq.wrapping_add(1).max(1);
        let mut req = format!(
            "{} {} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: camrtsp/{}\r\n",
            method,
            uri,
            self.cseq,
            crate::media::VERSION
        );
        for (k, v) in extra {
            req.push_str(&format!("{}: {}\r\n", k, v));
        }
        if !self.session.is_empty() && method != "DESCRIBE" {
            req.push_str(&format!("Session: {}\r\n", self.session));
        }
        req.push_str("\r\n");
        self.s.write_all(req.as_bytes())?;
        Ok(self.cseq)
    }

    /// Send and wait (bounded) for the matching reply. Frames that arrive
    /// meanwhile are dropped: nothing is playing yet during the handshake.
    fn request(&mut self, method: &str, uri: &str, extra: &[(&str, &str)]) -> io::Result<Response> {
        let cseq = self.send(method, uri, extra)?;
        let deadline = Instant::now() + self.request_timeout;
        loop {
            match self.poll()? {
                Ev::Reply(r) if r.cseq().map_or(true, |c| c == cseq) => return Ok(r),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no reply to {}", method),
                ));
            }
        }
    }

    /// SETUP one track over TCP-interleaved channels `chan`/`chan+1`;
    /// returns the RTP channel the server confirmed.
    fn setup(&mut self, uri: &str, chan: u8) -> io::Result<u8> {
        let transport = format!("RTP/AVP/TCP;unicast;interleaved={}-{}", chan, chan + 1);
        let r = self.request("SETUP", uri, &[("Transport", &transport)])?;
        if r.status != 200 {
            return Err(other(format!("SETUP {} returned {}", uri, r.status)));
        }
        if self.session.is_empty() {
            if let Some(v) = r.header("Session") {
                self.session = v.split(';').next().unwrap_or("").trim().to_string();
            }
        }
        if self.teardown_url.is_none() {
            // Base URL: control URL minus its last segment.
            let base = uri.rsplit_once('/').map(|(b, _)| format!("{}/", b));
            self.teardown_url = base.or_else(|| Some(uri.to_string()));
        }
        Ok(r.header("Transport").and_then(parse_interleaved).unwrap_or(chan))
    }

    /// Next frame or reply from the byte stream, `Idle` on read timeout.
    /// Anything else (stray text, server requests, junk) is skipped.
    fn poll(&mut self) -> io::Result<Ev> {
        loop {
            let avail = &self.buf[self.lo..self.hi];
            if let Some(&b0) = avail.first() {
                if b0 == b'$' {
                    if avail.len() >= 4 {
                        let ch = avail[1];
                        let len = ((avail[2] as usize) << 8) | avail[3] as usize;
                        if ch >= 16 {
                            self.skip(1)?; // a '$' in junk, not a frame
                            continue;
                        }
                        if avail.len() >= 4 + len {
                            let start = self.lo + 4;
                            self.lo = start + len;
                            self.garbage = 0;
                            return Ok(Ev::Frame(ch, start, start + len));
                        }
                    }
                } else if b0 == b'R' {
                    let n = avail.len().min(5);
                    if avail[..n] != b"RTSP/"[..n] {
                        self.skip(1)?;
                        continue;
                    }
                    if n == 5 {
                        match parse_response(avail) {
                            Ok(Some((r, used))) => {
                                self.lo += used;
                                self.garbage = 0;
                                return Ok(Ev::Reply(r));
                            }
                            Ok(None) => {}
                            Err(_) => {
                                self.skip(1)?;
                                continue;
                            }
                        }
                    }
                } else {
                    let n = avail
                        .iter()
                        .position(|&b| b == b'$' || b == b'R')
                        .unwrap_or(avail.len());
                    self.skip(n)?;
                    continue;
                }
            }
            if !self.fill()? {
                return Ok(Ev::Idle);
            }
        }
    }

    fn skip(&mut self, n: usize) -> io::Result<()> {
        self.lo += n;
        self.garbage += n;
        if self.garbage > MAX_GARBAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot resynchronise on the stream",
            ));
        }
        Ok(())
    }

    /// Read more bytes; false on read timeout.
    fn fill(&mut self) -> io::Result<bool> {
        if self.lo == self.hi {
            self.lo = 0;
            self.hi = 0;
        } else if self.lo > 0 && (self.hi == BUF_CAP || self.lo >= BUF_CAP / 2) {
            self.buf.copy_within(self.lo..self.hi, 0);
            self.hi -= self.lo;
            self.lo = 0;
        }
        if self.hi == BUF_CAP {
            // A whole buffer that is neither a frame nor a reply.
            return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized item on the stream"));
        }
        loop {
            match self.s.read(&mut self.buf[self.hi..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "upstream closed the connection",
                    ))
                }
                Ok(n) => {
                    self.hi += n;
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    return Ok(false)
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Conn {
    /// Best-effort TEARDOWN so the camera frees the session right away.
    fn drop(&mut self) {
        if let Some(url) = self.teardown_url.take() {
            let _ = self.s.set_write_timeout(Some(Duration::from_millis(300)));
            let _ = self.send("TEARDOWN", &url, &[]);
        }
        let _ = self.s.shutdown(Shutdown::Both);
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers (from camweb/src/rtsp.rs)
// ---------------------------------------------------------------------------

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn cseq(&self) -> Option<u32> {
        self.header("CSeq").and_then(|v| v.trim().parse().ok())
    }
}

/// One complete reply from the front of `buf`: `Ok(Some((resp, used)))`,
/// `Ok(None)` when more bytes are needed, `Err` when it is not a reply.
fn parse_response(buf: &[u8]) -> io::Result<Option<(Response, usize)>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let hdr_end = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => i,
        None if buf.len() > MAX_REPLY => return Err(bad("reply header too large")),
        None => return Ok(None),
    };
    let text = String::from_utf8_lossy(&buf[..hdr_end]);
    let mut lines = text.split("\r\n");
    let mut sl = lines.next().unwrap_or("").split_whitespace();
    if !sl.next().unwrap_or("").starts_with("RTSP/") {
        return Err(bad("not a status line"));
    }
    let status: u16 = sl.next().and_then(|s| s.parse().ok()).ok_or_else(|| bad("no status"))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let r = Response { status, headers, body: Vec::new() };
    let clen: usize = r.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if clen > MAX_REPLY {
        return Err(bad("absurd Content-Length"));
    }
    let total = hdr_end + 4 + clen;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some((Response { body: buf[hdr_end + 4..total].to_vec(), ..r }, total)))
}

/// `interleaved=0-1` out of a Transport header -> the RTP channel.
fn parse_interleaved(t: &str) -> Option<u8> {
    let rest = &t[t.find("interleaved=")? + "interleaved=".len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn join_control(base: &str, control: &str) -> String {
    let c = control.trim();
    if c.is_empty() || c == "*" {
        base.to_string()
    } else if c.len() >= 7 && c[..7].eq_ignore_ascii_case("rtsp://") {
        c.to_string()
    } else {
        format!("{}{}", base, c.trim_start_matches('/'))
    }
}

#[derive(Clone, Debug, Default)]
struct Track {
    media: String,
    encoding: String,
    clock: u32,
    channels: u32,
    control: String,
    fmtp: String,
    framerate: u32,
}

/// SDP -> media sections, in order.
fn parse_sdp(sdp: &str) -> Vec<Track> {
    let mut tracks: Vec<Track> = Vec::new();
    let mut pts: Vec<u32> = Vec::new();
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            let f: Vec<&str> = rest.split_whitespace().collect();
            pts = f.iter().skip(3).filter_map(|s| s.parse().ok()).collect();
            let mut t = Track {
                media: f.first().copied().unwrap_or("").to_ascii_lowercase(),
                ..Track::default()
            };
            // Static payload type 8 is PCMA/8000 even without an rtpmap.
            if pts.first() == Some(&8) {
                t.encoding = "PCMA".into();
                t.clock = 8000;
                t.channels = 1;
            }
            tracks.push(t);
            continue;
        }
        let Some(t) = tracks.last_mut() else {
            continue;
        };
        let ours = |rest: &str| -> Option<String> {
            let (pt, v) = rest.split_once(' ')?;
            let pt: u32 = pt.trim().parse().ok()?;
            (pts.is_empty() || pts.first() == Some(&pt)).then(|| v.trim().to_string())
        };
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some(enc) = ours(rest) {
                let mut p = enc.split('/');
                t.encoding = p.next().unwrap_or("").trim().to_ascii_uppercase();
                t.clock = p.next().and_then(|c| c.trim().parse().ok()).unwrap_or(0);
                t.channels = p.next().and_then(|c| c.trim().parse().ok()).unwrap_or(1);
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            if let Some(v) = ours(rest) {
                t.fmtp = v;
            }
        } else if let Some(rest) = line.strip_prefix("a=control:") {
            t.control = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("a=framerate:") {
            if let Ok(f) = rest.trim().parse::<f32>() {
                if f >= 1.0 && f <= 120.0 {
                    t.framerate = (f + 0.5) as u32;
                }
            }
        }
    }
    tracks
}

/// SPS and PPS from `sprop-parameter-sets=` in an H.264 fmtp value.
fn sprop(fmtp: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let rest = &fmtp[fmtp.find("sprop-parameter-sets=")? + "sprop-parameter-sets=".len()..];
    let end = rest.find(|c: char| c == ';' || c.is_whitespace()).unwrap_or(rest.len());
    let (mut sps, mut pps) = (None, None);
    for part in rest[..end].split(',') {
        let mut n = base64_decode(part)?;
        // ffmpeg pads its extradata: trailing_zero_8bits are not NAL bytes.
        while n.len() > 1 && n.last() == Some(&0) {
            n.pop();
        }
        match nal_type(&n) {
            7 => sps = Some(n),
            8 => pps = Some(n),
            _ => {}
        }
    }
    Some((sps?, pps?))
}

/// Standard or URL-safe base64; whitespace ignored, padding optional.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits >= 6 {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests: unit parsers, then a fake upstream speaking the camera's dialect
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CAM_SPS_B64: &str = "J03gKI1oBQBboQAAAwABAAADADIPFCKg";

    #[test]
    fn sdp_and_sprop_of_the_camera() {
        let sdp = "v=0\r\no=- 11 1 IN IP4 192.168.1.79\r\ns=/vs0\r\ni=luo_zhanzhao\r\n\
c=IN IP4 192.168.1.103\r\nt=0 0\r\na=control:*\r\na=range:npt=0-\r\nm=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\na=fmtp:96 sprop-parameter-sets=J03gKI1oBQBboQAAAwABAAADADIPFCKg,KO4C0kg=\r\n\
a=recvonly\r\na=control:camera\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=recvonly\r\n\
a=control:micphone\r\n";
        let t = parse_sdp(sdp);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].media.as_str(), t[0].encoding.as_str(), t[0].clock), ("video", "H264", 90000));
        assert_eq!(t[0].control, "camera");
        assert_eq!((t[1].encoding.as_str(), t[1].clock, t[1].channels), ("PCMA", 8000, 1));
        let (sps, pps) = sprop(&t[0].fmtp).expect("sprop");
        assert_eq!(sps.len(), 24);
        assert_eq!(pps, vec![0x28, 0xee, 0x02, 0xd2, 0x48]);
        assert_eq!(dimensions(&sps), Some((1280, 720)));
        // ffmpeg's -f rtp SDP for the same stream, zero-padded
        let ff = "packetization-mode=1; sprop-parameter-sets=J03gKI1oBQBboQAAAwABAAADADIPFCKgAA==,KO4C0kgA; profile-level-id=4DE028";
        assert_eq!(sprop(ff), Some((sps, pps)));
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn sps_sizes() {
        // x264 High 4:2:0 1920x1080 (cropped from 1088) and High 4:2:2 640x360
        let hi = unhex("67640028acd940780227e5c044000003000400000300783c60c658");
        assert_eq!(dimensions(&hi), Some((1920, 1080)));
        let h422 = unhex("677a0016bcd940a02ff89c0440000003004000000783c58b6580");
        assert_eq!(dimensions(&h422), Some((640, 360)));
        // garbage never panics
        let mut x = 0x1234_5678u32;
        for n in 0..2000 {
            let mut v = vec![0x67];
            for _ in 0..(n % 40) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(x as u8);
            }
            let _ = dimensions(&v);
        }
        assert_eq!(dimensions(&[0x67, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]), None);
        assert_eq!(join_control("rtsp://h:554/vs0/", "camera"), "rtsp://h:554/vs0/camera");
        assert_eq!(join_control("rtsp://h:554/vs0/", "*"), "rtsp://h:554/vs0/");
        assert_eq!(join_control("rtsp://h:554/vs0/", "rtsp://x/y"), "rtsp://x/y");
    }

    #[test]
    fn base64_and_headers() {
        assert_eq!(base64_decode(CAM_SPS_B64).map(|v| v.len()), Some(24));
        assert!(base64_decode("a").is_none());
        assert!(base64_decode("!!").is_none());
        assert_eq!(parse_interleaved("RTP/AVP/TCP;unicast;interleaved=2-3;ssrc=9"), Some(2));
        assert_eq!(parse_interleaved("RTP/AVP;unicast"), None);
        let raw = b"RTSP/1.0 200 OK\r\nContent-Length: 4\r\nCSeq: 2\r\nSession: 00000007; timeout=60\r\n\r\nv=0\r\n$";
        let (r, used) = parse_response(raw).unwrap().unwrap();
        assert_eq!((r.status, r.cseq(), r.body.as_slice()), (200, Some(2), &b"v=0\r"[..]));
        assert_eq!(&raw[used..], b"\n$");
        assert!(parse_response(&raw[..used - 1]).unwrap().is_none());
        assert!(parse_response(b"RTSP/1.0 abc\r\n\r\n").is_err());
        assert!(parse_response(b"RTSP/1.0 200 OK\r\nContent-Length: 99999999\r\n\r\n").is_err());
    }

    #[test]
    fn targets() {
        let t = Target::new("127.0.0.1:554", "/vs0").unwrap();
        assert_eq!((t.addr.as_str(), t.url.as_str(), t.name.as_str()), ("127.0.0.1:554", "rtsp://127.0.0.1:554/vs0", "vs0"));
        assert_eq!(t.base, "rtsp://127.0.0.1:554/vs0/");
        assert_eq!(Target::new("cam", "vs1/").unwrap().url, "rtsp://cam:554/vs1");
        assert_eq!(Target::new("[::1]:8554", "/vs0").unwrap().addr, "[::1]:8554");
        assert_eq!(Target::new("::1", "/vs0").unwrap().addr, "[::1]:554");
        assert!(Target::new("", "/vs0").is_err());
        assert!(Target::new("h:1", "/").is_err());
        assert!(Target::new("h:1", "/a b").is_err());
    }

    #[test]
    fn rtp_header_bounds() {
        let mut p = vec![0x80, 0xe0, 0, 1, 0, 0, 0x10, 0, 0, 0, 0, 0, 0x65, 1];
        let r = Rtp::parse(&p).unwrap();
        assert_eq!((r.seq, r.ts, r.marker, r.payload), (1, 0x1000, true, &[0x65, 1][..]));
        p[0] = 0x40;
        assert!(Rtp::parse(&p).is_none(), "RTP v1");
        // extension claiming more words than exist
        let q = [0x90, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0xbe, 0xde, 0x03, 0x04, 1];
        assert!(Rtp::parse(&q).is_none());
        // padding longer than the packet
        let mut pad = vec![0xa0, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x41, 0xff];
        assert!(Rtp::parse(&pad).is_none());
        pad[13] = 1;
        assert_eq!(Rtp::parse(&pad).unwrap().payload, &[0x41]);
        assert!(Rtp::parse(&[0x80; 11]).is_none());
    }

    #[test]
    fn clock_wraps_and_survives_jumps() {
        let mut c = Clock::new(90);
        assert_eq!(c.map(u32::MAX - 2999, 100), 100);
        assert_eq!(c.map(3000, 0), 166); // wrapped: +6000 ticks
        assert_eq!(c.map(2000, 0), 166); // small step back: held
        assert_eq!(c.map(3_000_000_000, 0), 232); // jump: one frame
    }

    #[test]
    fn assembler_groups_and_drops() {
        let mut a = Assembler::default();
        let mut out = Vec::new();
        let feed = |a: &mut Assembler, seq, ts, m, pl: &[u8], out: &mut Vec<Au>| {
            let p = super::tests_rtp(seq, ts, m, pl);
            a.push(&Rtp::parse(&p).unwrap(), out);
        };
        // SPS with marker alone is held for the picture that follows
        feed(&mut a, 1, 0, true, &[0x67, 1], &mut out);
        assert!(out.is_empty());
        feed(&mut a, 2, 0, true, &[0x65, 2], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].nals, vec![vec![0x67, 1], vec![0x65, 2]]);
        // lost marker: timestamp change closes the picture
        feed(&mut a, 3, 10, false, &[0x41, 3], &mut out);
        feed(&mut a, 4, 20, true, &[0x41, 4], &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!((out[1].ts, out[2].ts), (10, 20));
        // FU-A with a missing middle fragment is dropped, filler ignored
        feed(&mut a, 5, 30, false, &[0x7c, 0x85, 9], &mut out);
        feed(&mut a, 7, 30, false, &[0x7c, 0x45, 9], &mut out);
        feed(&mut a, 8, 30, true, &[0x0c, 0xff], &mut out);
        assert_eq!(out.len(), 3, "nothing usable in that picture");
        // continuation without start is ignored
        feed(&mut a, 9, 40, false, &[0x7c, 0x05, 9], &mut out);
        feed(&mut a, 10, 40, true, &[0x41, 5], &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[3].nals, vec![vec![0x41, 5]]);
    }
}

/// A fake upstream speaking the camera's dialect with deliberately nasty
/// framing. Every test runs against 127.0.0.1:0, no camera involved.
#[cfg(test)]
mod fake_upstream {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::Mutex;

    const SPROP: &str = "J03gKI1oBQBboQAAAwABAAADADIPFCKg,KO4C0kg=";

    fn sdp(audio: bool) -> String {
        let mut s = format!(
            "v=0\r\no=- 1 1 IN IP4 192.168.1.79\r\ns=/vs0\r\nt=0 0\r\na=control:*\r\n\
m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 sprop-parameter-sets={}\r\n\
a=recvonly\r\na=control:camera\r\n",
            SPROP
        );
        if audio {
            s.push_str("m=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=recvonly\r\na=control:micphone\r\n");
        }
        s
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Ka {
        Answer,
        Ignore,
        /// First keepalive gets 454, everything after is answered.
        Lost,
    }

    type W = Arc<Mutex<TcpStream>>;
    type Script = Arc<dyn Fn(&W, usize) + Send + Sync>;

    struct Fake {
        port: u16,
        /// (connection, method, session header)
        log: Arc<Mutex<Vec<(usize, String, String)>>>,
    }

    impl Fake {
        fn count(&self, conn: usize, method: &str) -> usize {
            self.log.lock().unwrap().iter().filter(|(c, m, _)| *c == conn && m == method).count()
        }
        fn conns(&self) -> usize {
            self.log.lock().unwrap().iter().map(|(c, _, _)| c + 1).max().unwrap_or(0)
        }
    }

    /// `script(writer, conn)` runs after each PLAY; connections numbered
    /// below `close_conns` are cut once it returns, the rest left open.
    fn fake(audio: bool, ka: Ka, max_conns: usize, close_conns: usize, script: Script) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        thread::spawn(move || {
            for conn in 0..max_conns {
                let Ok((s, _)) = l.accept() else { return };
                let log = log2.clone();
                let script = script.clone();
                thread::spawn(move || serve(s, conn, audio, ka, conn < close_conns, log, script));
            }
        });
        Fake { port, log }
    }

    fn serve(
        s: TcpStream,
        conn: usize,
        audio: bool,
        ka: Ka,
        close: bool,
        log: Arc<Mutex<Vec<(usize, String, String)>>>,
        script: Script,
    ) {
        let _ = s.set_nodelay(true);
        let mut r = BufReader::new(s.try_clone().unwrap());
        let w: W = Arc::new(Mutex::new(s));
        let mut played = false;
        let mut lost_sent = false;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut f = line.split_whitespace();
            let method = f.next().unwrap_or("").to_string();
            let uri = f.next().unwrap_or("").to_string();
            assert_eq!(f.next(), Some("RTSP/1.0"), "request line {:?}", line);
            let (mut cseq, mut session, mut transport) = (0u32, String::new(), String::new());
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 {
                    return;
                }
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    match k.to_ascii_lowercase().as_str() {
                        "cseq" => cseq = v.trim().parse().unwrap_or(0),
                        "session" => session = v.trim().to_string(),
                        "transport" => transport = v.trim().to_string(),
                        _ => {}
                    }
                }
            }
            log.lock().unwrap().push((conn, method.clone(), session.clone()));
            let reply = match method.as_str() {
                "DESCRIBE" if uri.ends_with("/vs0") => {
                    let body = sdp(audio);
                    format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nContent-Type: application/sdp\r\nContent-Base: {}/\r\nContent-Length: {}\r\n\r\n{}",
                        cseq, uri, body.len(), body
                    )
                }
                "DESCRIBE" => format!("RTSP/1.0 404 Not Found\r\nServer: nginx/1.17.8\r\nCSeq: {}\r\n\r\n", cseq),
                "SETUP" if transport.contains("RTP/AVP/TCP") => format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\nTransport: {};ssrc=9312188\r\nSession: 0000abcd; timeout=60\r\n\r\n",
                    cseq, transport
                ),
                "SETUP" => format!("RTSP/1.0 404 Not Found\r\nServer: nginx/1.17.8\r\nCSeq: {}\r\n\r\n", cseq),
                "PLAY" => format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\nRange: npt=0.000-\r\nSession: 0000abcd; timeout=60\r\n\r\n",
                    cseq
                ),
                "OPTIONS" if played => match ka {
                    Ka::Ignore => String::new(),
                    Ka::Lost if !lost_sent => {
                        lost_sent = true;
                        format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {}\r\n\r\n", cseq)
                    }
                    _ => format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: OPTIONS, PLAY\r\n\r\n", cseq),
                },
                _ => format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq),
            };
            if !reply.is_empty() {
                let _ = w.lock().unwrap().write_all(reply.as_bytes());
            }
            if method == "PLAY" && !played {
                played = true;
                let (w2, script) = (w.clone(), script.clone());
                thread::spawn(move || {
                    script(&w2, conn);
                    if close {
                        let _ = w2.lock().unwrap().shutdown(Shutdown::Both);
                    }
                });
            }
        }
    }

    fn il(chan: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![b'$', chan, (payload.len() >> 8) as u8, payload.len() as u8];
        v.extend_from_slice(payload);
        v
    }

    fn send(w: &W, b: &[u8]) {
        let _ = w.lock().unwrap().write_all(b);
    }

    fn rtp(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        super::tests_rtp(seq, ts, marker, payload)
    }

    fn cam_sps_pps() -> (Vec<u8>, Vec<u8>) {
        sprop(&format!("sprop-parameter-sets={}", SPROP)).unwrap()
    }

    fn cfg(tap: mpsc::Sender<Tap>) -> Cfg {
        Cfg {
            sdp_wait: Duration::from_secs(3),
            io_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_secs(2),
            keepalive: Duration::from_secs(60),
            watchdog: Duration::from_secs(5),
            backoff_min: Duration::from_millis(100),
            backoff_max: Duration::from_millis(400),
            tap: Some(tap),
        }
    }

    /// Collect `n` video frames (and whatever audio arrives meanwhile).
    fn collect(rx: &mpsc::Receiver<Tap>, n: usize) -> (Vec<Arc<VideoFrame>>, Vec<AudioFrame>) {
        let (mut v, mut a) = (Vec::new(), Vec::new());
        while v.len() < n {
            match rx.recv_timeout(Duration::from_secs(4)) {
                Ok(Tap::Video(f)) => v.push(f),
                Ok(Tap::Audio(f)) => a.push(f),
                Err(_) => panic!("only {} of {} frames arrived", v.len(), n),
            }
        }
        while let Ok(t) = rx.recv_timeout(Duration::from_millis(100)) {
            if let Tap::Audio(f) = t {
                a.push(f);
            }
        }
        (v, a)
    }

    fn pause() {
        thread::sleep(Duration::from_millis(15));
    }

    /// (1) the happy path: grouping, keyframes, timestamps, seqs, audio, info.
    #[test]
    fn answers_and_groups_access_units() {
        let (sps, pps) = cam_sps_pps();
        let (s2, p2) = (sps.clone(), pps.clone());
        let f = fake(true, Ka::Answer, 1, 0, Arc::new(move |w: &W, _| {
            let base = 900_000u32;
            send(w, &il(0, &rtp(1, base, false, &s2)));
            send(w, &il(0, &rtp(2, base, false, &p2)));
            send(w, &il(0, &rtp(3, base, true, &[0x65, 0x88, 1])));
            send(w, &il(2, &rtp(1, 5, false, &[0xd5; 320])));
            pause();
            send(w, &il(0, &rtp(4, base + 6000, true, &[0x41, 2])));
            send(w, &il(2, &rtp(2, 325, false, &[0xd4; 320])));
            pause();
            // two slices, one picture: marker only on the second
            send(w, &il(0, &rtp(5, base + 12000, false, &[0x41, 3])));
            send(w, &il(0, &rtp(6, base + 12000, true, &[0x41, 4])));
            pause();
            // marker lost: the next timestamp closes the picture
            send(w, &il(0, &rtp(7, base + 18000, false, &[0x41, 5])));
            send(w, &il(0, &rtp(8, base + 24000, true, &[0x41, 6])));
        }));
        let (tx, rx) = mpsc::channel();
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", cfg(tx)).expect("pull");
        let info = hub.info.clone();
        assert_eq!(info.name, "vs0");
        assert_eq!((info.width, info.height, info.fps), (1280, 720, 15));
        assert_eq!((info.sps.clone(), info.pps.clone()), (sps.clone(), pps.clone()));
        let a = info.audio.as_ref().expect("audio");
        assert_eq!((a.rate, a.channels, a.alaw), (8000, 1, true));

        let (v, au) = collect(&rx, 5);
        let nals: Vec<Vec<Vec<u8>>> = v.iter().map(|f| f.nals.clone()).collect();
        assert_eq!(nals[0], vec![sps, pps, vec![0x65, 0x88, 1]]);
        assert_eq!(nals[1], vec![vec![0x41, 2]]);
        assert_eq!(nals[2], vec![vec![0x41, 3], vec![0x41, 4]]);
        assert_eq!(nals[3], vec![vec![0x41, 5]]);
        assert_eq!(nals[4], vec![vec![0x41, 6]]);
        let keys: Vec<bool> = v.iter().map(|f| f.keyframe).collect();
        assert_eq!(keys, vec![true, false, false, false, false]);
        let seqs: Vec<u64> = v.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
        assert!(v[0].ts_ms < 1000, "starts near 0: {}", v[0].ts_ms);
        let d: Vec<u64> = v.iter().map(|f| f.ts_ms - v[0].ts_ms).collect();
        assert_eq!(d, vec![0, 66, 133, 200, 266]);
        assert_eq!(au.len(), 2);
        assert_eq!((au[0].data.len(), au[1].data[0]), (320, 0xd4));
        assert_eq!(au[1].ts_ms - au[0].ts_ms, 40);
        assert_eq!(f.count(0, "SETUP"), 2);
        drop(hub);
    }

    fn picture(seq: u16, ts: u32, tag: u8) -> Vec<u8> {
        il(0, &rtp(seq, ts, true, &[0x41, tag]))
    }

    /// (2) junk lines, stray replies, RTCP and junk '$' between frames.
    #[test]
    fn survives_garbage_and_rtcp() {
        let f = fake(false, Ka::Answer, 1, 0, Arc::new(|w: &W, _| {
            send(w, &picture(1, 0, 1));
            send(w, b"GARBAGE LINE that is not rtsp\r\n");
            send(w, &il(1, &[0x81, 0xc8, 0x00, 0x06, 1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0]));
            send(w, b"RTSP/1.0 200 OK\r\nCSeq: 999\r\n\r\n");
            send(w, b"$\xff\x00\x10 junk dollar, RTSX/ and RTSP/1.0 nonsense\r\n\r\n");
            send(w, &il(3, &[0x81, 0xc9, 0, 1, 0, 0, 0, 0]));
            send(w, &il(0, &[0x40, 0x60, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0x41, 9])); // RTP v1
            send(w, &il(0, &[]));
            send(w, &picture(3, 6000, 2));
        }));
        let (tx, rx) = mpsc::channel();
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", cfg(tx)).expect("pull");
        assert!(hub.info.audio.is_none());
        let (v, _) = collect(&rx, 2);
        assert_eq!(v[0].nals, vec![vec![0x41, 1]]);
        assert_eq!(v[1].nals, vec![vec![0x41, 2]]);
        assert_eq!(f.conns(), 1, "no reconnect");
    }

    /// (3) packets cut at odd offsets, delivered with gaps.
    #[test]
    fn torn_writes() {
        let big: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8 + 1).collect();
        let big2 = big.clone();
        let f = fake(true, Ka::Answer, 1, 0, Arc::new(move |w: &W, _| {
            let mut all = Vec::new();
            all.extend(il(0, &rtp(1, 0, false, &[0x7c, 0x85])));
            for (k, c) in big2.chunks(1000).enumerate() {
                let mut pl = vec![0x7c, if k == 2 { 0x45 } else { 0x05 }];
                pl.extend_from_slice(c);
                all.extend(il(0, &rtp(2 + k as u16, 0, k == 2, &pl)));
            }
            all.extend(il(2, &rtp(1, 0, false, &[0xd5; 160])));
            all.extend(picture(5, 6000, 7));
            let cuts = [1usize, 3, 2, 7, 1, 513, 4, 1100, 2, 5];
            let (mut i, mut k) = (0, 0);
            while i < all.len() {
                let n = cuts[k % cuts.len()].min(all.len() - i);
                {
                    let mut g = w.lock().unwrap();
                    let _ = g.write_all(&all[i..i + n]);
                    let _ = g.flush();
                }
                i += n;
                k += 1;
                thread::sleep(Duration::from_millis(1));
            }
        }));
        let (tx, rx) = mpsc::channel();
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", cfg(tx)).expect("pull");
        let (v, a) = collect(&rx, 2);
        let mut want = vec![0x65];
        want.extend_from_slice(&big);
        assert_eq!(v[0].nals, vec![want]);
        assert!(v[0].keyframe);
        assert_eq!(v[1].nals, vec![vec![0x41, 7]]);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].data, vec![0xd5; 160]);
        drop(hub);
    }

    /// (4) STAP-A with SPS+PPS+IDR is one keyframe AU, parameter sets kept.
    #[test]
    fn stap_a_parameter_sets_and_idr() {
        let f = fake(false, Ka::Answer, 1, 0, Arc::new(|w: &W, _| {
            let mut stap = vec![0x78];
            for n in [&[0x67u8, 0x42, 0, 0x1e][..], &[0x68, 0xce, 0x38, 0x80], &[0x65, 0x88, 0x80, 0x40]] {
                stap.extend_from_slice(&(n.len() as u16).to_be_bytes());
                stap.extend_from_slice(n);
            }
            send(w, &il(0, &rtp(1, 0, true, &stap)));
            send(w, &picture(2, 6000, 1));
        }));
        let (tx, rx) = mpsc::channel();
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", cfg(tx)).expect("pull");
        let (v, _) = collect(&rx, 2);
        assert_eq!(
            v[0].nals,
            vec![vec![0x67, 0x42, 0, 0x1e], vec![0x68, 0xce, 0x38, 0x80], vec![0x65, 0x88, 0x80, 0x40]]
        );
        assert!(v[0].keyframe);
        assert!(!v[1].keyframe);
        // the info keeps the SDP's parameter sets; frames carry the in-band ones
        assert_eq!(hub.info.sps, cam_sps_pps().0);
    }

    /// (5) one 100 KB IDR in 1400-byte FU-A fragments.
    #[test]
    fn fu_a_large_nal() {
        let nal: Vec<u8> = std::iter::once(0x65).chain((0..100_000u32).map(|i| (i * 7 % 256) as u8)).collect();
        let n2 = nal.clone();
        let f = fake(false, Ka::Answer, 1, 0, Arc::new(move |w: &W, _| {
            let body = &n2[1..];
            let chunks: Vec<&[u8]> = body.chunks(1400).collect();
            for (k, c) in chunks.iter().enumerate() {
                let last = k + 1 == chunks.len();
                let hdr = (if k == 0 { 0x80 } else { 0 }) | (if last { 0x40 } else { 0 }) | 5;
                let mut pl = vec![0x60 | 28, hdr];
                pl.extend_from_slice(c);
                send(w, &il(0, &rtp(k as u16, 90, last, &pl)));
            }
            send(w, &picture(chunks.len() as u16, 6090, 1));
        }));
        let (tx, rx) = mpsc::channel();
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", cfg(tx)).expect("pull");
        let (v, _) = collect(&rx, 2);
        assert_eq!(v[0].nals.len(), 1);
        assert_eq!(v[0].nals[0].len(), nal.len());
        assert!(v[0].nals[0] == nal);
        assert!(v[0].keyframe);
        drop(hub);
    }

    fn steady(w: &W, n: u16, start_ts: u32) {
        for k in 0..n {
            send(w, &picture(k, start_ts + k as u32 * 6000, k as u8));
            thread::sleep(Duration::from_millis(40));
        }
    }

    /// (6) keepalives that are never answered must not stall or kill video.
    #[test]
    fn unanswered_keepalive() {
        let f = fake(false, Ka::Ignore, 1, 0, Arc::new(|w: &W, _| steady(w, 40, 0)));
        let (tx, rx) = mpsc::channel();
        let mut c = cfg(tx);
        c.keepalive = Duration::from_millis(250);
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", c).expect("pull");
        let t0 = Instant::now();
        let (v, _) = collect(&rx, 40);
        let seqs: Vec<u64> = v.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, (0..40).collect::<Vec<u64>>());
        assert!(t0.elapsed() < Duration::from_secs(4));
        let ka = f.log.lock().unwrap().iter().filter(|(c, m, s)| *c == 0 && m == "OPTIONS" && s == "0000abcd").count();
        assert!(ka >= 3, "only {} keepalives", ka);
        assert_eq!(f.conns(), 1, "no reconnect");
        drop(hub);
    }

    /// 454 on a keepalive: re-PLAY on the same connection, keep streaming.
    #[test]
    fn session_not_found_replays() {
        let f = fake(false, Ka::Lost, 1, 0, Arc::new(|w: &W, _| steady(w, 25, 0)));
        let (tx, rx) = mpsc::channel();
        let mut c = cfg(tx);
        c.keepalive = Duration::from_millis(200);
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", c).expect("pull");
        let (v, _) = collect(&rx, 25);
        assert_eq!(v.last().map(|f| f.seq), Some(24));
        assert_eq!(f.count(0, "PLAY"), 2, "one re-PLAY");
        assert_eq!(f.conns(), 1);
        drop(hub);
    }

    /// (7) the upstream hangs up: back off, reconnect, keep seq/ts monotonic.
    #[test]
    fn reconnects_after_close() {
        let f = fake(true, Ka::Answer, 2, 1, Arc::new(|w: &W, conn| {
            // the second session restarts its RTP clock elsewhere
            steady(w, 3, if conn == 0 { 1000 } else { 4_000_000_000 });
        }));
        let (tx, rx) = mpsc::channel();
        let hub = pull_with(&format!("127.0.0.1:{}", f.port), "/vs0", cfg(tx)).expect("pull");
        let (v, _) = collect(&rx, 6);
        let seqs: Vec<u64> = v.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5]);
        for p in v.windows(2) {
            assert!(p[1].ts_ms > p[0].ts_ms, "ts {} then {}", p[0].ts_ms, p[1].ts_ms);
        }
        assert_eq!(f.conns(), 2);
        assert_eq!(f.count(1, "DESCRIBE"), 1);
        assert_eq!(f.count(1, "SETUP"), 2, "audio set up again");
        // the session is torn down when the Hub goes away
        drop(hub);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(f.count(1, "TEARDOWN"), 1);
    }

    #[test]
    fn unreachable_upstream_fails_in_time() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (tx, _rx) = mpsc::channel();
        let mut c = cfg(tx);
        c.sdp_wait = Duration::from_millis(600);
        let t0 = Instant::now();
        let e = pull_with(&format!("127.0.0.1:{}", port), "/vs0", c).err().expect("must fail");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused, "{}", e);
    }

    #[test]
    fn wrong_path_is_an_error() {
        let f = fake(false, Ka::Answer, 8, 0, Arc::new(|_: &W, _| {}));
        let (tx, _rx) = mpsc::channel();
        let mut c = cfg(tx);
        c.sdp_wait = Duration::from_millis(300);
        let e = pull_with(&format!("127.0.0.1:{}", f.port), "/nope", c).err().expect("404");
        assert!(e.to_string().contains("404"), "{}", e);
    }
}

#[cfg(test)]
fn tests_rtp(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0x80, if marker { 0xe0 } else { 0x60 }];
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(&ts.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 7]);
    v.extend_from_slice(payload);
    v
}
