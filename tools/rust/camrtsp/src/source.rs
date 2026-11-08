//! Where frames come from.
//!
//! The RTSP and HTTP layers only ever see a [`VideoSource`], so they can be
//! tested on the host with a file replay and later fed from the camera's own
//! hardware encoder without changing a line of protocol code.

use crate::media::{
    find_sps_pps, is_vcl, nal_type, split_annexb, AudioFrame, AudioInfo, StreamInfo, VideoFrame,
};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A frame-producing stream, shared by every RTSP session and their threads.
pub trait VideoSource: Send + Sync {
    fn info(&self) -> Arc<StreamInfo>;

    /// Block until a frame newer than `after_seq` exists, or `timeout` lapses.
    fn next(&self, after_seq: u64, timeout: Duration) -> Option<Arc<VideoFrame>>;

    /// Most recent frame, if any (never blocks).
    fn latest(&self) -> Option<Arc<VideoFrame>>;

    /// Has the source finished for good? Default: never. The RTSP session
    /// ends a stream on this instead of guessing from a fast `next()` return.
    fn ended(&self) -> bool {
        false
    }

    /// Audio access unit newer than `after_seq`. Default: none configured.
    fn next_audio(&self, _after_seq: u64, _timeout: Duration) -> Option<Arc<AudioFrame>> {
        None
    }
}

/// The broadcast point every producer publishes into and every session reads
/// from. One mutex + one condvar is plenty at 15 fps.
///
/// Deliberately built from `Mutex`/`Condvar` only: the ARM926EJ-S is ARMv5TE,
/// where `std::sync::atomic::AtomicU64` does not exist (no `ldrexd`) and
/// one-byte atomics are not lock-free either.
pub struct Hub {
    pub info: Arc<StreamInfo>,
    inner: Mutex<Inner>,
    cv: Condvar,
}

struct Inner {
    /// Recent frames, oldest first. A ring, not a single slot: upstream
    /// delivery is bursty (an IDR arrives as ~31 FU-A packets, and the next
    /// P-frame often sits in the same TCP read) and a reader that woke between
    /// two publishes used to lose the first of them. Measured end-to-end, that
    /// dropped all 8 keyframes of 375 access units and left a joiner unable to
    /// decode. `next(after_seq)` now walks the ring in order.
    frames: VecDeque<Arc<VideoFrame>>,
    frame_bytes: usize,
    audio: VecDeque<Arc<AudioFrame>>,
    published: u64,
    audio_published: u64,
    running: bool,
}

/// Deepest history kept for a lagging reader. The RTSP side already buffers 32
/// frames per session and one 720p access unit is ~8 KiB, so this is a few MiB.
const RING_FRAMES: usize = 48;
const RING_BYTES: usize = 4 << 20;
const AUDIO_RING: usize = 64;

impl Hub {
    pub fn new(info: Arc<StreamInfo>) -> Arc<Hub> {
        Arc::new(Hub {
            info,
            inner: Mutex::new(Inner {
                frames: VecDeque::new(),
                frame_bytes: 0,
                audio: VecDeque::new(),
                published: 0,
                audio_published: 0,
                running: true,
            }),
            cv: Condvar::new(),
        })
    }

    /// Publish a frame, assigning it the next sequence number.
    pub fn publish(&self, mut f: VideoFrame) {
        let mut g = self.inner.lock().unwrap();
        f.seq = g.published;
        g.published += 1;
        push_frame(&mut g, Arc::new(f));
        drop(g);
        self.cv.notify_all();
    }

    /// Publish a frame whose sequence number is already assigned (pump input).
    pub fn publish_seq(&self, f: VideoFrame) {
        let mut g = self.inner.lock().unwrap();
        if g.published <= f.seq {
            g.published = f.seq + 1;
        }
        push_frame(&mut g, Arc::new(f));
        drop(g);
        self.cv.notify_all();
    }

    pub fn publish_audio(&self, mut a: AudioFrame) {
        let mut g = self.inner.lock().unwrap();
        a.seq = g.audio_published;
        g.audio_published += 1;
        g.audio.push_back(Arc::new(a));
        while g.audio.len() > AUDIO_RING {
            g.audio.pop_front();
        }
        drop(g);
        self.cv.notify_all();
    }

    pub fn frames_published(&self) -> u64 {
        self.inner.lock().unwrap().published
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().running
    }

    /// Tell any waiter that no more frames will arrive. Frames already in the
    /// ring stay readable, so a session can still drain what it has.
    pub fn stop(&self) {
        self.inner.lock().unwrap().running = false;
        self.cv.notify_all();
    }
}

fn push_frame(g: &mut Inner, f: Arc<VideoFrame>) {
    g.frame_bytes += f.len();
    g.frames.push_back(f);
    while g.frames.len() > RING_FRAMES || (g.frame_bytes > RING_BYTES && g.frames.len() > 1) {
        if let Some(old) = g.frames.pop_front() {
            g.frame_bytes -= old.len();
        }
    }
}

/// Oldest frame strictly newer than `after`.
///
/// If the reader fell far enough behind that frames were dropped, the first
/// frame it can still be handed is a P-frame whose references are gone. Skip
/// forward to the next keyframe in the ring instead — that is the only frame
/// that decodes on its own. With no keyframe ahead, the oldest is returned and
/// the caller decides.
fn pick_frame(g: &Inner, after: u64) -> Option<Arc<VideoFrame>> {
    if g.frames.is_empty() {
        return None;
    }
    // NB: `after` is a *cursor*, not an index — a reader that has consumed
    // frame 0 passes 0 and must still get frame 1, so 0 cannot be treated as
    // "start of stream". A reader that has no cursor yet uses `latest()` (the
    // session's video pump does exactly that on startup); sequence 0 itself is
    // therefore only reachable through `latest()`.
    let pos = g.frames.iter().position(|f| f.seq > after)?;
    if g.frames[pos].seq == after.wrapping_add(1) {
        return Some(g.frames[pos].clone());
    }
    // Gap: the reader fell off the ring, or it resumed from a stale number.
    g.frames
        .iter()
        .skip(pos)
        .find(|f| f.keyframe)
        .cloned()
        .or_else(|| Some(g.frames[pos].clone()))
}

impl VideoSource for Hub {
    fn info(&self) -> Arc<StreamInfo> {
        self.info.clone()
    }

    fn next(&self, after_seq: u64, timeout: Duration) -> Option<Arc<VideoFrame>> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(f) = pick_frame(&g, after_seq) {
                return Some(f);
            }
            if !g.running {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (ng, _) = self.cv.wait_timeout(g, deadline - now).unwrap();
            g = ng;
        }
    }

    fn latest(&self) -> Option<Arc<VideoFrame>> {
        self.inner.lock().unwrap().frames.back().cloned()
    }

    fn next_audio(&self, after_seq: u64, timeout: Duration) -> Option<Arc<AudioFrame>> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(a) = g.audio.iter().find(|a| a.seq > after_seq) {
                return Some(a.clone());
            }
            if !g.running {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (ng, _) = self.cv.wait_timeout(g, deadline - now).unwrap();
            g = ng;
        }
    }

    fn ended(&self) -> bool {
        !self.is_running()
    }
}

/// Group NAL units into access units (one coded picture each).
pub fn group_access_units(nals: Vec<Vec<u8>>) -> Vec<Vec<Vec<u8>>> {
    let mut aus: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut cur: Vec<Vec<u8>> = Vec::new();
    let mut cur_has_vcl = false;
    for n in nals {
        let t = nal_type(&n);
        let boundary = match t {
            6 | 7 | 8 | 9 => cur_has_vcl, // parameter sets / SEI / AUD start a new AU
            _ if is_vcl(t) => cur_has_vcl, // a second slice of a new picture
            _ => false,
        };
        if boundary && !cur.is_empty() {
            aus.push(std::mem::take(&mut cur));
            cur_has_vcl = false;
        }
        if is_vcl(t) {
            cur_has_vcl = true;
        }
        cur.push(n);
    }
    if !cur.is_empty() {
        aus.push(cur);
    }
    aus
}

fn au_is_keyframe(nals: &[Vec<u8>]) -> bool {
    nals.iter().any(|n| nal_type(n) == 5 || nal_type(n) == 7)
}

/// Source that replays a raw Annex-B H.264 file at a fixed rate. Used for
/// host tests and for exercising the server on the camera without touching
/// the sensor.
pub fn replay(path: &str, fps: u32, loopy: bool) -> std::io::Result<Arc<Hub>> {
    let data = std::fs::read(path)?;
    let nals = split_annexb(&data);
    let aus = group_access_units(nals);
    if aus.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no NAL units found",
        ));
    }
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for au in &aus {
        let (s, p) = find_sps_pps(au);
        if let Some(s) = s {
            sps = s;
        }
        if let Some(p) = p {
            pps = p;
        }
        if !sps.is_empty() && !pps.is_empty() {
            break;
        }
    }
    if sps.is_empty() || pps.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file has no SPS/PPS (need a keyframe frame at the start)",
        ));
    }
    // Dimensions are cosmetic (SDP carries them in the SPS anyway); only use
    // the parse when it produced something a camera could plausibly emit.
    let (mut width, mut height) = (0u32, 0u32);
    if let Some((w, h)) = crate::media::sps_dimensions(&sps) {
        if w >= 64 && w % 16 == 0 && h >= 64 && h % 2 == 0 && w <= 4096 && h <= 4096 {
            width = w;
            height = h;
        }
    }
    let info = Arc::new(StreamInfo {
        name: "vs0".to_string(),
        sps,
        pps,
        width,
        height,
        fps,
        audio: None,
    });
    let hub = Hub::new(info);
    let h = hub.clone();
    thread::Builder::new()
        .name("replay".into())
        .spawn(move || {
            let period = Duration::from_micros(1_000_000 / fps.max(1) as u64);
            let start = Instant::now();
            let mut seq = 0u64;
            'outer: loop {
                for au in &aus {
                    let ts = start.elapsed().as_millis() as u64;
                    h.publish(VideoFrame {
                        nals: au.clone(),
                        keyframe: au_is_keyframe(au),
                        ts_ms: ts,
                        seq,
                    });
                    seq += 1;
                    // Absolute pacing, so the loop keeps its average rate.
                    let due = start + period * seq as u32;
                    let now = Instant::now();
                    if due > now {
                        thread::sleep(due - now);
                    }
                }
                if !loopy {
                    break 'outer;
                }
            }
            // Tell waiters we are done (sessions then see `running == false`).
            h.stop();
        })?;
    Ok(hub)
}

/// A source that produces nothing, so the server can run with no camera.
pub fn empty(name: &str) -> Arc<Hub> {
    Hub::new(Arc::new(StreamInfo {
        name: name.to_string(),
        sps: Vec::new(),
        pps: Vec::new(),
        width: 0,
        height: 0,
        fps: 0,
        audio: None,
    }))
}

/// A tiny synthetic source for tests: correct-looking SPS/PPS and 1 KB
/// "slices" at a fixed interval. Never decoded, only packetized.
pub fn synthetic(fps: u32, alaw: bool) -> Arc<Hub> {
    // Minimal baseline SPS/PPS that real parsers accept as opaque bytes.
    let sps = vec![0x67, 0x42, 0xc0, 0x1e, 0xda, 0x02, 0x80, 0xbf, 0xe5, 0x84, 0x00, 0x00];
    let pps = vec![0x68, 0xce, 0x38, 0x80];
    let info = Arc::new(StreamInfo {
        name: "vs0".to_string(),
        sps,
        pps,
        width: 1280,
        height: 720,
        fps,
        audio: if alaw {
            Some(AudioInfo { rate: 8000, channels: 1, alaw: true })
        } else {
            None
        },
    });
    let hub = Hub::new(info);
    let h = hub.clone();
    let do_audio = alaw;
    thread::Builder::new()
        .name("synth".into())
        .spawn(move || {
            let period = Duration::from_micros(1_000_000 / fps.max(1) as u64);
            let start = Instant::now();
            let mut seq = 0u64;
            let mut aseq = 0u64;
            loop {
                let key = seq % (fps as u64 * 2).max(1) == 0;
                let mut nals = Vec::new();
                if key {
                    nals.push(h.info.sps.clone());
                    nals.push(h.info.pps.clone());
                }
                let mut slice = vec![if key { 0x65 } else { 0x41 }];
                slice.extend(std::iter::repeat((seq % 251) as u8).take(1024));
                nals.push(slice);
                h.publish(VideoFrame {
                    nals,
                    keyframe: key,
                    ts_ms: start.elapsed().as_millis() as u64,
                    seq,
                });
                seq += 1;
                if do_audio {
                    // 20 ms of a-law
                    let mut d = vec![0u8; 160];
                    for (i, b) in d.iter_mut().enumerate() {
                        *b = ((aseq as usize + i) % 128) as u8 | 0xd5;
                    }
                    h.publish_audio(AudioFrame {
                        data: d,
                        ts_ms: start.elapsed().as_millis() as u64,
                        seq: aseq,
                    });
                    aseq += 1;
                }
                thread::sleep(period);
            }
        })
        .expect("spawn synth");
    hub
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(n: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(n);
        v
    }

    #[test]
    fn grouping_splits_on_parameter_sets_and_slices() {
        let mut d = Vec::new();
        d.extend_from_slice(&sc(&[0x67, 1])); // SPS
        d.extend_from_slice(&sc(&[0x68, 2])); // PPS
        d.extend_from_slice(&sc(&[0x65, 3])); // IDR slice
        d.extend_from_slice(&sc(&[0x41, 4])); // P slice
        d.extend_from_slice(&sc(&[0x41, 5])); // P slice
        let aus = group_access_units(split_annexb(&d));
        assert_eq!(aus.len(), 3, "SPS+PPS+IDR | P | P");
        assert!(au_is_keyframe(&aus[0]));
        assert!(!au_is_keyframe(&aus[1]));
    }

    #[test]
    fn hub_hands_out_newer_frames() {
        let hub = synthetic(25, false);
        let first = hub.next(0, Duration::from_secs(2)).expect("a frame");
        let second = hub
            .next(first.seq, Duration::from_secs(2))
            .expect("next frame");
        assert!(second.seq > first.seq);
        assert_eq!(hub.info().width, 1280);
        hub.stop();
    }

    #[test]
    fn empty_source_times_out() {
        let hub = empty("vs0");
        assert!(hub.next(0, Duration::from_millis(50)).is_none());
    }

    // --- the ring: a burst must not lose frames, a lagging reader must
    // resync on a keyframe instead of on a P-frame whose reference is gone ---

    fn frame(seq: u64, keyframe: bool, bytes: usize) -> VideoFrame {
        VideoFrame {
            nals: vec![vec![if keyframe { 0x65 } else { 0x41 }; bytes]],
            keyframe,
            ts_ms: seq * 40,
            seq,
        }
    }

    fn test_info() -> Arc<StreamInfo> {
        Arc::new(StreamInfo {
            name: "vs0".into(),
            sps: vec![0x67, 0x42],
            pps: vec![0x68, 0xce],
            width: 1280,
            height: 720,
            fps: 15,
            audio: None,
        })
    }

    fn hub_with(frames: &[(bool, usize)]) -> Arc<Hub> {
        let hub = Hub::new(test_info());
        for (i, (k, b)) in frames.iter().enumerate() {
            let mut f = frame(i as u64, *k, *b);
            f.seq = 0; // publish() assigns it
            hub.publish(f);
        }
        hub
    }

    #[test]
    fn ring_delivers_every_frame_in_order() {
        // Exactly the burst the pump measured: an IDR and the P-frame behind it
        // published inside one wake-up. The old single-slot Hub kept only the
        // last of them, so the P-frame's reference chain broke.
        let hub = hub_with(&[(true, 4000), (false, 300), (false, 280)]);
        let mut seq = 0u64; // the session's startup cursor is `latest()`
        let mut seen = Vec::new();
        while let Some(f) = hub.next(seq, Duration::from_millis(50)) {
            if f.seq == seq {
                break;
            }
            seq = f.seq;
            seen.push((f.seq, f.len(), f.keyframe));
            if seq >= 2 {
                break;
            }
        }
        assert_eq!(
            seen,
            vec![(1, 300, false), (2, 280, false)],
            "every AU after the cursor, in order, none lost"
        );
    }

    #[test]
    fn ring_is_bounded_and_forgets_oldest() {
        let hub = Hub::new(test_info());
        for i in 0..(RING_FRAMES * 2) {
            let mut f = frame(i as u64, i % 10 == 0, 64);
            f.seq = 0;
            hub.publish(f);
        }
        let g = hub.inner.lock().unwrap();
        assert!(g.frames.len() <= RING_FRAMES);
        assert!(g.frame_bytes <= RING_BYTES + 64);
        drop(g);
        // Oldest surviving frame is well past 0.
        let first = hub.next(0, Duration::from_millis(100)).expect("a frame");
        assert!(first.seq > 0);
    }

    #[test]
    fn lagging_reader_resyncs_on_a_keyframe() {
        // Reader is at seq 0; the ring now holds 40..95 with a keyframe at 50.
        let hub = Hub::new(test_info());
        for i in 0..96u64 {
            let mut f = frame(i, i % 10 == 0, 100);
            f.seq = 0;
            hub.publish(f);
        }
        let f = hub.next(0, Duration::from_millis(100)).expect("a frame");
        assert!(f.keyframe, "must resume at a keyframe, not a P-frame");
        assert_eq!(f.seq % 10, 0);
    }

    #[test]
    fn ended_reports_stop_and_drains_what_is_left() {
        let hub = hub_with(&[(true, 100), (false, 50), (false, 40)]);
        hub.stop();
        assert!(hub.ended());
        // Frames published before the stop are still readable: a session that
        // was at seq 0 drains 1 then 2, and only then gets its end.
        let a = hub.next(0, Duration::from_millis(100)).expect("drained");
        assert_eq!(a.seq, 1);
        let b = hub.next(a.seq, Duration::from_millis(100)).expect("drained");
        assert_eq!(b.seq, 2);
        assert!(hub.next(b.seq, Duration::from_millis(10)).is_none());
    }

    #[test]
    fn audio_ring_keeps_order_and_bounds() {
        let hub = empty("vs0");
        for i in 0..(AUDIO_RING + 10) {
            let mut a = AudioFrame { data: vec![i as u8; 8], seq: 0, ts_ms: i as u64 * 128 };
            a.seq = 0;
            hub.publish_audio(a);
        }
        assert!(hub.inner.lock().unwrap().audio.len() <= AUDIO_RING);
        let a = hub.next_audio(0, Duration::from_millis(100)).expect("audio");
        let b = hub.next_audio(a.seq, Duration::from_millis(100)).expect("next audio");
        assert_eq!(b.seq, a.seq + 1);
    }
}
