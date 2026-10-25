//! Media side of a session: the source fan-out, bounded per-session queues,
//! and the sender thread that turns frames into RTP.
//!
//! Flow: `Fanout` threads are the only readers of the `VideoSource`. Each
//! frame is an `Arc`, pushed into the queue of every playing session; a full
//! queue drops the frame and counts it (and a keyframe flushes the queue, so
//! a lagging client restarts from a decodable picture instead of draining
//! stale ones). The session's sender thread pops frames, packetizes and
//! writes. Only the sender can be slowed by its client — up to the socket
//! write timeout, after which the connection is declared dead.

use super::rtp::{self, Rtp};
use super::{lock, STACK};
use crate::media::{is_vcl, nal_type, AudioFrame, StreamInfo, VideoFrame};
use crate::source::VideoSource;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Per-session video queue bounds: about 2 s at 15 fps, and never more than
/// 512 KB of frames pinned for one viewer.
const QUEUE_FRAMES: usize = 32;
const QUEUE_BYTES: usize = 512 * 1024;
/// Audio frames are 20–40 ms each.
const AUDIO_QUEUE: usize = 50;
/// RTCP Sender Report interval (RFC 3550 suggests 5 s minimum).
const SR_EVERY: Duration = Duration::from_secs(5);
/// Longest a sender sleeps with nothing to send; bounds RTCP/UDP polling.
const IDLE_TICK: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// The control connection's socket, shared with the sender for interleaving.

/// The TCP connection, shared by the control thread (responses) and the
/// sender (interleaved RTP/RTCP). One lock per write keeps `$` frames and
/// response text whole. A failed or timed-out write leaves the stream in an
/// unknown framing state, so it kills the connection outright.
pub struct Wire {
    inner: Mutex<WireInner>,
}

struct WireInner {
    s: TcpStream,
    dead: bool,
}

impl Wire {
    pub fn new(s: TcpStream) -> Wire {
        Wire { inner: Mutex::new(WireInner { s, dead: false }) }
    }

    pub fn send(&self, b: &[u8]) -> bool {
        let mut g = lock(&self.inner);
        if g.dead {
            return false;
        }
        if g.s.write_all(b).is_err() {
            g.dead = true;
            let _ = g.s.shutdown(Shutdown::Both);
            return false;
        }
        true
    }

    pub fn dead(&self) -> bool {
        lock(&self.inner).dead
    }
}

// ---------------------------------------------------------------------------
// Session state shared between the control thread, the fan-out and the sender.

pub struct Shared {
    st: Mutex<State>,
    cv: Condvar,
}

struct State {
    alive: bool,
    playing: bool,
    /// Set by PLAY: the sender must restart at a keyframe with SPS/PPS.
    resync: bool,
    want_video: bool,
    want_audio: bool,
    video: VecDeque<Arc<VideoFrame>>,
    vbytes: usize,
    audio: VecDeque<Arc<AudioFrame>>,
    /// Frames this session lost to a full queue.
    dropped: u64,
    /// Last RTCP/RTP seen from the client over UDP (liveness for reaping).
    last_rx: Option<Instant>,
    /// The source has ended for good: say BYE once the queue is sent.
    eos: bool,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            st: Mutex::new(State {
                alive: true,
                playing: false,
                resync: false,
                want_video: false,
                want_audio: false,
                video: VecDeque::new(),
                vbytes: 0,
                audio: VecDeque::new(),
                dropped: 0,
                last_rx: None,
                eos: false,
            }),
            cv: Condvar::new(),
        })
    }

    pub fn set_playing(&self, on: bool) {
        let mut g = lock(&self.st);
        g.playing = on;
        if on {
            g.resync = true;
        } else {
            g.video.clear();
            g.vbytes = 0;
            g.audio.clear();
        }
        drop(g);
        self.cv.notify_all();
    }

    pub fn kill(&self) {
        let mut g = lock(&self.st);
        g.alive = false;
        g.playing = false;
        g.video.clear();
        g.vbytes = 0;
        g.audio.clear();
        drop(g);
        self.cv.notify_all();
    }

    fn end_of_stream(&self) {
        lock(&self.st).eos = true;
        self.cv.notify_all();
    }

    pub fn last_rx(&self) -> Option<Instant> {
        lock(&self.st).last_rx
    }

    /// Returns false once the session is gone (the fan-out then forgets it).
    fn push_video(&self, f: &Arc<VideoFrame>) -> bool {
        let mut g = lock(&self.st);
        if !g.alive {
            return false;
        }
        if !g.playing || !g.want_video {
            return true;
        }
        let len = f.len();
        if g.video.len() >= QUEUE_FRAMES || g.vbytes + len > QUEUE_BYTES {
            g.dropped += 1;
            if !f.keyframe {
                return true;
            }
            // A keyframe is a fresh start: drop the backlog, not the keyframe.
            g.dropped += g.video.len() as u64;
            g.video.clear();
            g.vbytes = 0;
        }
        g.vbytes += len;
        g.video.push_back(f.clone());
        drop(g);
        self.cv.notify_all();
        true
    }

    fn push_audio(&self, a: &Arc<AudioFrame>) -> bool {
        let mut g = lock(&self.st);
        if !g.alive {
            return false;
        }
        if !g.playing || !g.want_audio {
            return true;
        }
        if g.audio.len() >= AUDIO_QUEUE {
            g.dropped += 1;
            return true;
        }
        g.audio.push_back(a.clone());
        drop(g);
        self.cv.notify_all();
        true
    }
}

// ---------------------------------------------------------------------------
// Fan-out: the only readers of the source.

pub struct Fanout {
    subs: Mutex<Vec<Arc<Shared>>>,
    /// The source will never produce another video frame.
    ended: Mutex<bool>,
}

impl Fanout {
    pub fn start(src: Arc<dyn VideoSource>) -> io::Result<Arc<Fanout>> {
        let fan = Arc::new(Fanout { subs: Mutex::new(Vec::new()), ended: Mutex::new(false) });
        let (f2, s2) = (fan.clone(), src.clone());
        thread::Builder::new()
            .name("rtsp-video".into())
            .stack_size(STACK)
            .spawn(move || video_pump(f2, s2))?;
        if src.info().audio.is_some() {
            let (f2, s2) = (fan.clone(), src);
            thread::Builder::new()
                .name("rtsp-audio".into())
                .stack_size(STACK)
                .spawn(move || audio_pump(f2, s2))?;
        }
        Ok(fan)
    }

    pub fn add(&self, s: Arc<Shared>) {
        if *lock(&self.ended) {
            s.end_of_stream();
        }
        lock(&self.subs).push(s);
    }

    fn end(&self) {
        let mut e = lock(&self.ended);
        if !*e {
            *e = true;
            for s in lock(&self.subs).iter() {
                s.end_of_stream();
            }
        }
    }
}

/// `next()` returned nothing. A wait that returned at once means the source
/// has stopped (`Hub::stop`, e.g. a replay without `loop` ran out): sleep
/// instead of spinning, and tell the caller so it can end the stream.
fn backoff(t0: Instant) -> bool {
    if t0.elapsed() < Duration::from_millis(20) {
        thread::sleep(Duration::from_millis(100));
        return true;
    }
    false
}

fn video_pump(fan: Arc<Fanout>, src: Arc<dyn VideoSource>) {
    let mut last: Option<u64> = None;
    let mut poll = Duration::from_millis(5);
    loop {
        let t0 = Instant::now();
        let f = match last {
            // Startup: `next(after)` can never return seq 0, and the first
            // frame is usually the keyframe a stream must start on. Poll
            // `latest()` — fast at first, backing off to 50 ms for a source
            // that stays empty — so it is not lost.
            None => match src.latest() {
                Some(f) => f,
                None => {
                    thread::sleep(poll);
                    poll = (poll * 2).min(Duration::from_millis(50));
                    continue;
                }
            },
            Some(l) => match src.next(l, Duration::from_secs(1)) {
                Some(f) => f,
                None => {
                    if backoff(t0) {
                        fan.end();
                    }
                    continue;
                }
            },
        };
        last = Some(f.seq);
        lock(&fan.subs).retain(|s| s.push_video(&f));
    }
}

fn audio_pump(fan: Arc<Fanout>, src: Arc<dyn VideoSource>) {
    let mut last = 0u64;
    loop {
        let t0 = Instant::now();
        match src.next_audio(last, Duration::from_secs(1)) {
            Some(a) => {
                last = a.seq;
                lock(&fan.subs).retain(|s| s.push_audio(&a));
            }
            None => {
                backoff(t0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tracks and the sender thread.

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Video,
    Audio,
}

pub enum Out {
    Tcp { wire: Arc<Wire>, rtp_ch: u8, rtcp_ch: u8 },
    Udp { rtp: UdpSocket, rtcp: UdpSocket, dst_rtp: SocketAddr, dst_rtcp: SocketAddr },
}

pub struct Track {
    pub kind: Kind,
    pub out: Out,
    pub rtp: Rtp,
}

/// An even/odd UDP port pair on `ip` (RFC 3550 §11 convention), falling back
/// to any two ports. Both non-blocking: a full socket buffer drops the
/// datagram rather than stalling the sender.
pub fn bind_pair(ip: IpAddr) -> io::Result<(UdpSocket, UdpSocket)> {
    let mut pair = None;
    for _ in 0..16 {
        let a = UdpSocket::bind((ip, 0))?;
        let p = a.local_addr()?.port();
        if p % 2 == 0 && p < u16::MAX {
            if let Ok(b) = UdpSocket::bind((ip, p + 1)) {
                pair = Some((a, b));
                break;
            }
        }
    }
    let (a, b) = match pair {
        Some(x) => x,
        None => (UdpSocket::bind((ip, 0))?, UdpSocket::bind((ip, 0))?),
    };
    a.set_nonblocking(true)?;
    b.set_nonblocking(true)?;
    Ok((a, b))
}

pub fn spawn_sender(
    shared: Arc<Shared>,
    info: Arc<StreamInfo>,
    tracks: Vec<Track>,
    peer: IpAddr,
    label: String,
) -> io::Result<JoinHandle<()>> {
    let mut video = None;
    let mut audio = None;
    for t in tracks {
        match t.kind {
            Kind::Video => video = Some(t),
            Kind::Audio => audio = Some(t),
        }
    }
    {
        let mut g = lock(&shared.st);
        g.want_video = video.is_some();
        g.want_audio = audio.is_some();
    }
    let mut s = Sender {
        shared,
        info,
        video,
        audio,
        peer,
        label,
        buf: Vec::with_capacity(64 * 1024),
        bounds: Vec::with_capacity(64),
        need_key: true,
        last_seq: None,
        frames: 0,
        skipped: 0,
        next_sr: Instant::now(),
    };
    thread::Builder::new()
        .name("rtsp-send".into())
        .stack_size(STACK)
        .spawn(move || s.run())
}

struct Sender {
    shared: Arc<Shared>,
    info: Arc<StreamInfo>,
    video: Option<Track>,
    audio: Option<Track>,
    peer: IpAddr,
    label: String,
    buf: Vec<u8>,
    bounds: Vec<usize>,
    /// Waiting for a keyframe (start, resume, or after a gap).
    need_key: bool,
    last_seq: Option<u64>,
    frames: u64,
    skipped: u64,
    next_sr: Instant,
}

fn chan(out: &Out, rtcp: bool) -> Option<u8> {
    match out {
        Out::Tcp { rtp_ch, rtcp_ch, .. } => Some(if rtcp { *rtcp_ch } else { *rtp_ch }),
        Out::Udp { .. } => None,
    }
}

/// Send what `h264_au`/`packet` put in `buf`. False = the connection is dead.
fn emit(out: &Out, buf: &[u8], bounds: &[usize], rtcp: bool) -> bool {
    match out {
        Out::Tcp { wire, .. } => wire.send(buf),
        Out::Udp { rtp, rtcp: rs, dst_rtp, dst_rtcp } => {
            let (sock, dst) = if rtcp { (rs, dst_rtcp) } else { (rtp, dst_rtp) };
            for (i, &start) in bounds.iter().enumerate() {
                let end = bounds.get(i + 1).copied().unwrap_or(buf.len());
                // UDP is lossy by contract; a full buffer just drops.
                let _ = sock.send_to(&buf[start..end], *dst);
            }
            true
        }
    }
}

impl Sender {
    fn run(&mut self) {
        let mut vq: VecDeque<Arc<VideoFrame>> = VecDeque::with_capacity(QUEUE_FRAMES);
        let mut aq: VecDeque<Arc<AudioFrame>> = VecDeque::with_capacity(AUDIO_QUEUE);
        let mut playing;
        let mut eos;
        loop {
            {
                let mut g = lock(&self.shared.st);
                if g.alive && g.video.is_empty() && g.audio.is_empty() {
                    g = match self.shared.cv.wait_timeout(g, IDLE_TICK) {
                        Ok((g, _)) => g,
                        Err(e) => e.into_inner().0,
                    };
                }
                if !g.alive {
                    break;
                }
                if g.resync {
                    g.resync = false;
                    self.need_key = true;
                    self.last_seq = None;
                }
                playing = g.playing;
                eos = g.eos;
                std::mem::swap(&mut g.video, &mut vq);
                g.vbytes = 0;
                std::mem::swap(&mut g.audio, &mut aq);
            }
            let mut ok = true;
            while let Some(f) = vq.pop_front() {
                ok = ok && self.send_video(&f);
            }
            while let Some(a) = aq.pop_front() {
                ok = ok && self.send_audio(&a);
            }
            if ok && eos {
                // Everything queued is out; nothing more will come.
                self.send_reports(true);
                break;
            }
            if ok && playing && Instant::now() >= self.next_sr {
                ok = self.send_reports(false);
            }
            self.drain_udp();
            if !ok {
                self.shared.kill();
                break;
            }
        }
        let dropped = lock(&self.shared.st).dropped;
        eprintln!(
            "rtsp: {} sender done: {} frames sent, {} skipped waiting for a keyframe, {} dropped by a full queue",
            self.label, self.frames, self.skipped, dropped
        );
    }

    fn send_video(&mut self, f: &VideoFrame) -> bool {
        let t = match self.video.as_mut() {
            Some(t) => t,
            None => return true,
        };
        // A gap in source sequence numbers (queue overflow, a missed frame)
        // breaks the reference chain: wait for the next keyframe.
        if let Some(l) = self.last_seq {
            if f.seq != l.wrapping_add(1) {
                self.need_key = true;
            }
        }
        self.last_seq = Some(f.seq);
        let has = |t: u8| f.nals.iter().any(|n| nal_type(n) == t);
        let key = f.keyframe || has(5);
        if self.need_key && !key {
            self.skipped += 1;
            return true;
        }
        // An AU with no picture in it (only parameter sets, SEI, filler) is
        // not a frame: sending it would put a marker bit on a non-picture.
        if !f.nals.iter().any(|n| is_vcl(nal_type(n))) {
            return true;
        }
        self.need_key = false;

        // SPS/PPS go out right before every keyframe: from the frame itself
        // when the encoder put them in-band, else from StreamInfo.
        let sps = (key && !has(7) && !self.info.sps.is_empty()).then_some(&self.info.sps[..]);
        let pps = (key && !has(8) && !self.info.pps.is_empty()).then_some(&self.info.pps[..]);
        let nals = sps
            .into_iter()
            .chain(pps)
            .chain(f.nals.iter().map(|n| &n[..]))
            .filter(|n| !n.is_empty());

        let ts = t.rtp.ts_for(f.ts_ms);
        self.buf.clear();
        self.bounds.clear();
        rtp::h264_au(&mut t.rtp, nals, ts, chan(&t.out, false), &mut self.buf, &mut self.bounds);
        self.frames += 1;
        emit(&t.out, &self.buf, &self.bounds, false)
    }

    fn send_audio(&mut self, a: &AudioFrame) -> bool {
        let t = match self.audio.as_mut() {
            Some(t) => t,
            None => return true,
        };
        if a.data.is_empty() {
            return true;
        }
        let ts = t.rtp.ts_for(a.ts_ms);
        self.buf.clear();
        self.bounds.clear();
        let ch = chan(&t.out, false);
        // One packet per frame; G.711 is one byte per sample, so an
        // oversized frame splits cleanly with the timestamp advanced by the
        // bytes already sent.
        let mut off = 0usize;
        for part in a.data.chunks(rtp::MAX_PAYLOAD) {
            let last = off + part.len() == a.data.len();
            t.rtp.packet(&mut self.buf, &mut self.bounds, ch, last, ts.wrapping_add(off as u32), &[part]);
            off += part.len();
        }
        emit(&t.out, &self.buf, &self.bounds, false)
    }

    fn send_reports(&mut self, bye: bool) -> bool {
        let mut sent = false;
        for t in [self.video.as_ref(), self.audio.as_ref()].into_iter().flatten() {
            if t.rtp.packets == 0 && !bye {
                continue;
            }
            self.buf.clear();
            self.bounds.clear();
            t.rtp.sender_report(&mut self.buf, &mut self.bounds, chan(&t.out, true), bye);
            if !emit(&t.out, &self.buf, &self.bounds, true) {
                return false;
            }
            sent = true;
        }
        // The first report goes out as soon as there is something to report.
        if sent {
            self.next_sr = Instant::now() + SR_EVERY;
        }
        true
    }

    /// Read and discard whatever the client sends to our UDP ports (RTCP
    /// RRs, NAT hole-punching). Anything from the client's address counts as
    /// a sign of life; none of it is acted upon.
    fn drain_udp(&mut self) {
        let mut tmp = [0u8; 1536];
        let mut seen = false;
        for t in [self.video.as_ref(), self.audio.as_ref()].into_iter().flatten() {
            if let Out::Udp { rtp, rtcp, .. } = &t.out {
                for sock in [rtp, rtcp] {
                    for _ in 0..32 {
                        match sock.recv_from(&mut tmp) {
                            Ok((n, from)) => {
                                if from.ip() == self.peer {
                                    let _ = rtp::rtcp_valid(&tmp[..n]);
                                    seen = true;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }
        if seen {
            lock(&self.shared.st).last_rx = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64, key: bool, size: usize) -> Arc<VideoFrame> {
        Arc::new(VideoFrame { nals: vec![vec![0x41; size]], keyframe: key, ts_ms: seq, seq })
    }

    #[test]
    fn queue_is_bounded_and_a_keyframe_flushes_it() {
        let s = Shared::new();
        lock(&s.st).want_video = true;
        assert!(s.push_video(&frame(0, false, 10)), "not playing: ignored, kept");
        assert!(lock(&s.st).video.is_empty());
        s.set_playing(true);
        for i in 0..100 {
            assert!(s.push_video(&frame(i, false, 1000)));
        }
        {
            let g = lock(&s.st);
            assert_eq!(g.video.len(), QUEUE_FRAMES);
            assert_eq!(g.dropped, 100 - QUEUE_FRAMES as u64);
        }
        s.push_video(&frame(100, true, 1000));
        {
            let g = lock(&s.st);
            assert_eq!(g.video.len(), 1);
            assert!(g.video[0].keyframe);
        }
        for i in 0..10 {
            s.push_video(&frame(200 + i, false, 200_000));
        }
        assert!(lock(&s.st).vbytes <= QUEUE_BYTES);
        s.set_playing(false);
        assert!(lock(&s.st).video.is_empty(), "pause frees the backlog");
        s.kill();
        assert!(!s.push_video(&frame(300, false, 10)), "dead sessions leave the fan-out");
    }
}
