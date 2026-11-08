//! Segment recorder — motion-gated H.264 on the SD card.
//!
//! The cameras this runs on are deployed offsite behind an LTE hotspot, so
//! continuous upload is impossible and the card is the only buffer. The shape
//! that follows from this:
//!
//! * record **only while motion is detected**, with a short pre-roll so the
//!   recording starts before the trigger (motion detection lives in
//!   `motion.rs` and reaches us as one `bool` per frame — the motion *level*,
//!   "is something moving now", not an edge);
//! * cut the result into short, self-contained Annex-B H.264 files with a
//!   small JSON sidecar, so a batch uploader can move them later, verify each
//!   one, and delete it;
//! * keep a hard byte quota. A site with no operator that silently fills the
//!   card stops recording forever, and nobody notices until the footage they
//!   want is missing. This is the only thing standing between an offsite
//!   camera and a card full of nothing.
//!
//! Nothing here touches the network, the encoder, or the RTSP server: it takes
//! [`VideoFrame`]s and a `bool` and writes files. The interesting failure modes
//! of this device are an SD card that gets yanked mid-write (it has already
//! happened once in this project) and a power cut, not anything a clever
//! abstraction would help with.
//!
//! # Files
//!
//! ```text
//! <dir>/rec/20261025-181500.h264   raw Annex-B: 00 00 00 01 + NAL, one AU after another
//! <dir>/rec/20261025-181500.json   sidecar, written only when the segment is closed
//! ```
//!
//! `<dir>` is the card mount (`/mnt` on the camera); [`Recorder::new`] creates
//! `<dir>/rec`. The name is the UTC wall clock of the segment's first access
//! unit, taken from `SystemTime` once at construction and carried forward on
//! the frame clock, so a clock jump mid-recording cannot make two segments
//! claim the same second. A collision (rotate inside one second, or a restart
//! in the same second) appends `-2`, `-3`, …; the file is created with
//! `create_new`, so an existing file is never truncated.
//!
//! Every segment starts on a keyframe (SPS/PPS included, as the encoder emits
//! them on IDRs) unless the encoder goes a long time without one — see
//! [`RecConfig::keyframe_wait_ms`]. That rule is worth more than pre-roll
//! fidelity: a file that starts mid-GOP cannot be decoded, and an uploader
//! cannot tell that from the bytes alone.
//!
//! # Memory
//!
//! The pre-roll ring holds encoded AUs, capped at `pre_roll_max_bytes` (4 MiB
//! by default) plus one flush buffer of at most the same size while a segment
//! opens; a 3 s 720p pre-roll is ~375 kB, so the caps are headroom, not the
//! normal case. That matters: the board has 64 MB of RAM for everything,
//! including the encoder and the RTSP server.
//!
//! # Failure policy
//!
//! `on_frame` and `tick` never return `Err` for runtime I/O trouble and never
//! panic. The first real error (ENOSPC, EIO, card gone) is printed once, the
//! open segment is finalised as best it can be, and the recorder latches itself
//! **halted**: it stops writing for the rest of the process instead of
//! retrying into a dead card every frame. Only [`Recorder::new`] fails, and
//! only when the directory cannot be created — that is a wiring error and the
//! caller should say so at startup.
//!
//! Losing the last seconds of the open segment to a power cut is expected:
//! `sync_all` costs a flash erase, so it happens on close, not per frame. The
//! kernel writes dirty pages back within ~30 s, which is the real exposure, and
//! the reason `max_segment_len_ms` defaults to 60 s.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::media::{nal_type, sps_dimensions, VideoFrame, VERSION};

/// Directory under the card root that holds our segments.
pub const SUBDIR: &str = "rec";

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Tunables. `Default` is what the camera should use.
#[derive(Clone, Debug)]
pub struct RecConfig {
    /// How much encoded video to keep on hand so a recording starts before the
    /// trigger. Time-capped *and* AU-capped *and* byte-capped; the strictest
    /// wins. 3 s at 15 fps is 45 AUs / ~375 kB at 1 Mbit/s, so the caps below
    /// have room for a 25 fps stream and a noisy night scene.
    pub pre_roll_ms: u64,
    /// Keep recording this long after motion stops, so a bird that hops out of
    /// frame does not truncate the interesting part.
    pub post_roll_ms: u64,
    /// Never close a segment shorter than this (a single-frame trigger with
    /// `post_roll_ms = 0` would otherwise leave a 60 ms file). The file stays
    /// open and keeps growing until the minimum is met. Rotation wins if
    /// `max_segment_len_ms` is the smaller of the two.
    pub min_segment_len_ms: u64,
    /// Rotate at this length: files stay upload-friendly and a power cut can
    /// only lose the current one.
    pub max_segment_len_ms: u64,
    /// Hard cap on the bytes we own under `<dir>/rec`, enforced by deleting the
    /// oldest finished segments. 8 GiB ≈ 25 % of the 29.7 GB card, which is
    /// ~17 h of the 720p stream at ~1 Mbit/s and far more of motion blips.
    pub quota_bytes: u64,
    /// Ring size limits, access units and bytes.
    pub pre_roll_max_aus: usize,
    pub pre_roll_max_bytes: usize,
    /// If motion is detected but the ring holds no keyframe, wait this long for
    /// one before starting anyway. Without the escape hatch a stream that sent
    /// a single IDR at startup would record nothing at all; with it we lose at
    /// most this much of the trigger.
    pub keyframe_wait_ms: u64,
}

impl Default for RecConfig {
    fn default() -> Self {
        RecConfig {
            pre_roll_ms: 3_000,
            post_roll_ms: 3_000,
            min_segment_len_ms: 1_000,
            max_segment_len_ms: 60_000,
            quota_bytes: 8 * 1024 * 1024 * 1024,
            pre_roll_max_aus: 96,
            pre_roll_max_bytes: 4 * 1024 * 1024,
            keyframe_wait_ms: 5_000,
        }
    }
}

/// Why a segment was closed. `cut_short` in the sidecar is true only for `Stop`
/// and `Error` — `Rotate` and `PostRoll` are the plan working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    PostRoll,
    Rotate,
    Stop,
    Error,
}

impl CloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseReason::PostRoll => "post_roll",
            CloseReason::Rotate => "rotate",
            CloseReason::Stop => "stop",
            CloseReason::Error => "error",
        }
    }
    /// True when the segment ended before its own time was up (UI stop, or we
    /// ran into a card error). The uploader can flag these.
    pub fn cut_short(self) -> bool {
        matches!(self, CloseReason::Stop | CloseReason::Error)
    }
}

/// A finished segment: on disk, closed, sidecar written.
#[derive(Clone, Debug)]
pub struct Segment {
    pub path: PathBuf,
    pub sidecar: PathBuf,
    /// Wall clock (ms since the Unix epoch) of the first access unit.
    pub start_unix_ms: u64,
    /// Recorder clock (same units as `VideoFrame::ts_ms`) of the first AU.
    pub start_ts_ms: u64,
    /// Wall clock of the motion trigger that opened the segment: later than
    /// `start_unix_ms` by the pre-roll we managed to flush.
    pub trigger_unix_ms: u64,
    pub trigger_ts_ms: u64,
    /// Last AU ts − first AU ts: the span covered, not plus one frame.
    pub duration_ms: u64,
    /// Bytes in the `.h264` file.
    pub bytes: u64,
    /// Bytes in the `.json` sidecar (0 if it could not be written).
    pub sidecar_bytes: u64,
    pub au_count: u32,
    pub keyframe_count: u32,
    pub width: u32,
    pub height: u32,
    pub reason: CloseReason,
}

impl Segment {
    /// Total bytes this segment occupies on the card.
    pub fn total_bytes(&self) -> u64 {
        self.bytes + self.sidecar_bytes
    }
    pub fn cut_short(&self) -> bool {
        self.reason.cut_short()
    }
}

/// What one `on_frame` call did. Runtime failures are reported here rather than
/// as `Err`, so a caller in the RTSP loop can log and keep streaming.
#[derive(Clone, Debug, Default)]
pub struct RecordAction {
    /// A segment file was created by this call.
    pub opened: Option<PathBuf>,
    /// Access units appended by this call (0 while only buffering pre-roll).
    pub appended_aus: u32,
    pub appended_bytes: u64,
    /// Finished segments the quota deleted during this call.
    pub evicted: u32,
    /// Set once, on the call that latched the recorder off.
    pub error: Option<String>,
    /// How many closed segments are waiting to be collected by `tick`.
    pub pending: usize,
}

impl RecordAction {
    /// Nothing was written, opened, evicted or broken.
    pub fn is_quiet(&self) -> bool {
        self.opened.is_none()
            && self.appended_aus == 0
            && self.evicted == 0
            && self.error.is_none()
    }
}

/// One buffered access unit: already encoded as Annex-B, exactly the bytes that
/// go to a file, so flushing the ring is a memcpy chain.
struct Au {
    ts_ms: u64,
    seq: u64,
    keyframe: bool,
    bytes: Vec<u8>,
}

/// A segment currently being written.
struct OpenSeg {
    path: PathBuf,
    sidecar: PathBuf,
    file: File,
    start_unix_ms: u64,
    start_ts_ms: u64,
    trigger_unix_ms: u64,
    trigger_ts_ms: u64,
    last_ts_ms: u64,
    last_seq: Option<u64>,
    bytes: u64,
    au_count: u32,
    keyframe_count: u32,
    width: u32,
    height: u32,
}

/// A closed segment as tracked for the quota. We deliberately do not keep a
/// full [`Segment`] for every file on the card: 17 h of 60 s segments is a
/// thousand of them and this process has 64 MB of RAM in total.
struct Finished {
    path: PathBuf,
    sidecar: PathBuf,
    h264_bytes: u64,
    json_bytes: u64,
}

enum State {
    /// Buffering pre-roll, no file open.
    Idle,
    /// Motion was seen but the ring had no keyframe to start on; waiting for
    /// one, or for `keyframe_wait_ms` to expire.
    Arming,
    Writing(OpenSeg),
}

/// Where to start a segment inside the pre-roll ring.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartPoint {
    /// Oldest keyframe: maximum pre-roll (the motion trigger).
    OldestKeyframe,
    /// Newest keyframe: minimum overlap with the segment just closed
    /// (rotation).
    NewestKeyframe,
    /// Oldest AU whatever it is — last resort, when waiting for a keyframe has
    /// gone on too long.
    OldestAny,
}

pub struct Recorder {
    dir: PathBuf,
    cfg: RecConfig,
    state: State,
    ring: VecDeque<Au>,
    ring_bytes: usize,
    /// Recorder-clock time of the trigger we are arming for.
    arming_since: Option<u64>,
    finished: Vec<Finished>,
    pending: VecDeque<Segment>,
    used: u64,
    /// Wall clock at construction, carried forward on the frame clock.
    wall_base_ms: u64,
    ts_base: Option<u64>,
    last_ts_ms: u64,
    last_motion_ms: u64,
    motion_now: bool,
    halted: Option<String>,
    quota_warned: bool,
    /// Total segments evicted over the recorder's life; `RecordAction.evicted`
    /// is the delta across one call.
    evicted_total: u64,
}

impl Recorder {
    /// `dir` is the card root; segments go in `dir/rec`, created if missing.
    /// Existing segments are adopted, so the quota covers earlier runs.
    pub fn new(dir: &Path, cfg: RecConfig) -> io::Result<Recorder> {
        Recorder::with_wall_clock(dir, cfg, now_unix_ms())
    }

    /// Same, with a fixed wall clock — for tests, and for a board whose RTC is
    /// wrong but whose clock someone sets later.
    pub fn with_wall_clock(dir: &Path, cfg: RecConfig, wall_unix_ms: u64) -> io::Result<Recorder> {
        if cfg.max_segment_len_ms == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rec: max_segment_len_ms must not be 0",
            ));
        }
        if cfg.pre_roll_max_aus == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rec: pre_roll_max_aus must not be 0",
            ));
        }
        let rec_dir = dir.join(SUBDIR);
        fs::create_dir_all(&rec_dir)?;
        let (finished, used) = scan(&rec_dir)?;
        Ok(Recorder {
            dir: rec_dir,
            cfg,
            state: State::Idle,
            ring: VecDeque::new(),
            ring_bytes: 0,
            arming_since: None,
            finished,
            pending: VecDeque::new(),
            used,
            wall_base_ms: wall_unix_ms,
            ts_base: None,
            last_ts_ms: 0,
            last_motion_ms: 0,
            motion_now: false,
            halted: None,
            quota_warned: false,
            evicted_total: 0,
        })
    }

    /// `<dir>/rec`.
    pub fn rec_dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.state, State::Writing(_))
    }

    /// True once a real I/O error has stopped us for good.
    pub fn is_halted(&self) -> bool {
        self.halted.is_some()
    }

    pub fn halted_reason(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    /// Path of the file being written, for logs.
    pub fn open_path(&self) -> Option<&Path> {
        match &self.state {
            State::Writing(s) => Some(&s.path),
            _ => None,
        }
    }

    /// Bytes we own under `<dir>/rec`, open segment included.
    pub fn space_used(&self) -> u64 {
        self.used
    }

    pub fn quota_bytes(&self) -> u64 {
        self.cfg.quota_bytes
    }

    /// Finished segments on disk, oldest first — the uploader's work list.
    pub fn finished(&self) -> impl Iterator<Item = &Path> {
        self.finished.iter().map(|f| f.path.as_path())
    }

    pub fn finished_count(&self) -> usize {
        self.finished.len()
    }

    /// How many closed segments `tick` / `drain_closed` have not handed out.
    pub fn pending_closed(&self) -> usize {
        self.pending.len()
    }

    /// Take everything that has closed. Use this or `tick`, not both.
    pub fn drain_closed(&mut self) -> Vec<Segment> {
        self.pending.drain(..).collect()
    }

    /// Wall clock for a recorder-clock timestamp.
    pub fn wall_clock_ms(&self, ts_ms: u64) -> u64 {
        let base = self.ts_base.unwrap_or(ts_ms);
        self.wall_base_ms.saturating_add(ts_ms.saturating_sub(base))
    }

    /// Feed one access unit. `motion` is the current motion *level*; a true
    /// that follows a false is the trigger.
    pub fn on_frame(&mut self, f: &VideoFrame, motion: bool) -> io::Result<RecordAction> {
        let mut act = RecordAction::default();
        if self.halted.is_some() {
            return Ok(act);
        }
        let evicted0 = self.evicted_total;
        let ts = self.stamp(f.ts_ms);
        let was_motion = self.motion_now;
        self.motion_now = motion;
        if motion {
            self.last_motion_ms = ts;
        }

        // 1. Buffer first: the trigger flush and a later rotation both read
        //    this ring, and the frame in hand has to be in it.
        self.push_ring(f, ts);

        // 2. A rising edge opens a segment.
        if motion && !was_motion && matches!(self.state, State::Idle) {
            self.arming_since = Some(ts);
            self.state = State::Arming;
        }

        // 3. Arming: start as soon as the ring holds a keyframe, or stop
        //    waiting and start on whatever we have.
        if matches!(self.state, State::Arming) {
            let since = self.arming_since.unwrap_or(ts);
            let have_key = self.ring.iter().any(|a| a.keyframe);
            let waited_out = ts.saturating_sub(since) >= self.cfg.keyframe_wait_ms;
            if have_key || waited_out {
                let sp = if have_key {
                    StartPoint::OldestKeyframe
                } else {
                    StartPoint::OldestAny
                };
                self.begin(sp, since, &mut act);
            } else if !motion && ts.saturating_sub(self.last_motion_ms) >= self.cfg.post_roll_ms {
                // The blip was over before a keyframe arrived: nothing worth
                // recording, go quiet again.
                self.state = State::Idle;
                self.arming_since = None;
            }
        }

        // 4. While a file is open: append, then decide whether to keep it.
        if self.is_recording() {
            // The flush inside `begin` may already have written this frame.
            let already = match &self.state {
                State::Writing(s) => s.last_seq == Some(f.seq) && s.last_ts_ms == ts,
                _ => false,
            };
            if !already {
                self.append(f, ts, &mut act);
            }
            if self.is_recording() {
                let span = match &self.state {
                    State::Writing(s) => ts.saturating_sub(s.start_ts_ms),
                    _ => 0,
                };
                let idle = !motion
                    && ts.saturating_sub(self.last_motion_ms) >= self.cfg.post_roll_ms
                    && span >= self.cfg.min_segment_len_ms;
                if idle {
                    self.close(CloseReason::PostRoll);
                } else if span >= self.cfg.max_segment_len_ms {
                    self.close(CloseReason::Rotate);
                    let keep_going = motion
                        || ts.saturating_sub(self.last_motion_ms) < self.cfg.post_roll_ms;
                    if keep_going {
                        // Rotation continues the recording: new file starting
                        // at the newest keyframe in the ring (a couple of
                        // frames of overlap beats a gap or a mid-GOP start).
                        self.begin(StartPoint::NewestKeyframe, ts, &mut act);
                    }
                }
            }
        }

        act.evicted = (self.evicted_total - evicted0) as u32;
        act.pending = self.pending.len();
        Ok(act)
    }

    /// Clock tick: close on post-roll expiry even if no frames are arriving (a
    /// stalled encoder must not leave a file open forever), enforce the quota,
    /// and hand back one closed segment.
    pub fn tick(&mut self, now_ms: u64) -> io::Result<Option<Segment>> {
        if self.halted.is_none() {
            let ts = self.stamp(now_ms);
            if self.is_recording() {
                let span = match &self.state {
                    State::Writing(s) => ts.saturating_sub(s.start_ts_ms),
                    _ => 0,
                };
                let idle = !self.motion_now
                    && ts.saturating_sub(self.last_motion_ms) >= self.cfg.post_roll_ms
                    && span >= self.cfg.min_segment_len_ms;
                if idle {
                    // Rotation is checked on frames only: with no frames there
                    // is nothing to put in the next file.
                    self.close(CloseReason::PostRoll);
                }
            }
            if matches!(self.state, State::Arming) {
                let since = self.arming_since.unwrap_or(ts);
                if !self.motion_now && ts.saturating_sub(self.last_motion_ms) >= self.cfg.post_roll_ms
                {
                    self.state = State::Idle;
                    self.arming_since = None;
                } else if ts.saturating_sub(since) >= self.cfg.keyframe_wait_ms && !self.ring.is_empty()
                {
                    let mut sink = RecordAction::default();
                    self.begin(StartPoint::OldestAny, since, &mut sink);
                }
            }
            self.evict();
        }
        Ok(self.pending.pop_front())
    }

    /// Close the open segment now, marked `stop` (cut short by the UI or by
    /// shutdown). Returns it if one was open.
    pub fn stop(&mut self) -> io::Result<Option<Segment>> {
        let before = self.pending.len();
        self.close(CloseReason::Stop);
        // A recorder that was arming (motion seen, no keyframe yet) must not
        // open a segment after being told to stop.
        if matches!(self.state, State::Arming) {
            self.state = State::Idle;
        }
        self.arming_since = None;
        if self.pending.len() > before {
            return Ok(self.pending.pop_back());
        }
        Ok(None)
    }

    /// Delete a segment after a successful upload. Missing files on disk are
    /// fine (someone else may have cleaned up); an unknown path is not.
    pub fn remove(&mut self, path: &Path) -> io::Result<()> {
        let idx = match self.finished.iter().position(|f| f.path == path) {
            Some(i) => i,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("rec: {} is not a segment I know about", path.display()),
                ))
            }
        };
        let f = self.finished.remove(idx);
        let mut freed = 0u64;
        if remove_if_present(&f.path)? {
            freed += f.h264_bytes;
        }
        if remove_if_present(&f.sidecar)? {
            freed += f.json_bytes;
        }
        self.used = self.used.saturating_sub(freed);
        Ok(())
    }

    // ---- internals -------------------------------------------------------

    /// Monotonic clamp on the caller's clock, and the base for the wall clock.
    fn stamp(&mut self, ms: u64) -> u64 {
        let base = *self.ts_base.get_or_insert(ms);
        let ts = if ms < base { base } else { ms };
        if ts > self.last_ts_ms {
            self.last_ts_ms = ts;
        }
        ts
    }

    fn push_ring(&mut self, f: &VideoFrame, ts: u64) {
        if f.nals.is_empty() {
            return;
        }
        let bytes = annexb(&f.nals);
        if bytes.is_empty() {
            return;
        }
        self.ring_bytes += bytes.len();
        self.ring.push_back(Au {
            ts_ms: ts,
            seq: f.seq,
            keyframe: f.keyframe,
            bytes,
        });
        // Age out first, then the hard caps. Always keep the newest AU even if
        // it alone is bigger than the byte cap, or the ring could be empty and
        // nothing could ever start.
        while self.ring.len() > 1 {
            let old = match self.ring.front() {
                Some(a) => ts.saturating_sub(a.ts_ms) > self.cfg.pre_roll_ms,
                None => false,
            };
            if !old {
                break;
            }
            self.pop_front();
        }
        while self.ring.len() > 1
            && (self.ring.len() > self.cfg.pre_roll_max_aus
                || self.ring_bytes > self.cfg.pre_roll_max_bytes)
        {
            self.pop_front();
        }
    }

    fn pop_front(&mut self) {
        if let Some(a) = self.ring.pop_front() {
            self.ring_bytes -= a.bytes.len();
        }
    }

    fn ring_start(&self, sp: StartPoint) -> Option<usize> {
        match sp {
            StartPoint::OldestKeyframe => self.ring.iter().position(|a| a.keyframe),
            StartPoint::NewestKeyframe => self.ring.iter().rposition(|a| a.keyframe),
            StartPoint::OldestAny => {
                if self.ring.is_empty() {
                    None
                } else {
                    Some(0)
                }
            }
        }
    }

    /// Create the file and flush the ring into it. On I/O failure the recorder
    /// latches off and goes back to `Idle`.
    fn begin(&mut self, sp: StartPoint, trigger_ts: u64, act: &mut RecordAction) {
        let start = match self.ring_start(sp) {
            Some(i) => i,
            None => {
                // No usable start point yet: stay arming.
                self.state = State::Arming;
                if self.arming_since.is_none() {
                    self.arming_since = Some(trigger_ts);
                }
                return;
            }
        };
        let start_ts = self.ring.get(start).map(|a| a.ts_ms).unwrap_or(trigger_ts);
        let start_unix = self.wall_clock_ms(start_ts);
        let trigger_unix = self.wall_clock_ms(trigger_ts);

        let (file, path) = match self.create_file(start_unix) {
            Ok(v) => v,
            Err(e) => {
                self.halt("create", &e, act);
                return;
            }
        };
        let sidecar = path.with_extension("json");
        let (width, height) = self
            .ring
            .get(start)
            .and_then(|a| sps_dims(&a.bytes))
            .unwrap_or((0, 0));
        self.state = State::Writing(OpenSeg {
            path: path.clone(),
            sidecar,
            file,
            start_unix_ms: start_unix,
            start_ts_ms: start_ts,
            trigger_unix_ms: trigger_unix,
            trigger_ts_ms: trigger_ts,
            last_ts_ms: start_ts,
            last_seq: None,
            bytes: 0,
            au_count: 0,
            keyframe_count: 0,
            width,
            height,
        });
        self.arming_since = None;
        act.opened = Some(path);

        // Flush the pre-roll in one write: a single big write is kinder to the
        // card than 45 small ones, and the buffer is ~0.4 MB.
        let total: usize = self.ring.iter().skip(start).map(|a| a.bytes.len()).sum();
        let mut buf: Vec<u8> = Vec::with_capacity(total);
        let mut aus = 0u32;
        let mut keys = 0u32;
        let mut last_ts = start_ts;
        let mut last_seq = None;
        for au in self.ring.iter().skip(start) {
            buf.extend_from_slice(&au.bytes);
            aus += 1;
            if au.keyframe {
                keys += 1;
            }
            last_ts = au.ts_ms;
            last_seq = Some(au.seq);
        }
        if buf.is_empty() {
            return;
        }
        match self.write_open(&buf) {
            Ok(n) => {
                self.note_written(aus, keys, last_ts, last_seq);
                self.used += n;
                act.appended_aus += aus;
                act.appended_bytes += n;
                self.evict();
            }
            Err(e) => self.halt("write", &e, act),
        }
    }

    fn append(&mut self, f: &VideoFrame, ts: u64, act: &mut RecordAction) {
        let buf = annexb(&f.nals);
        if buf.is_empty() {
            return;
        }
        match self.write_open(&buf) {
            Ok(n) => {
                let k = if f.keyframe { 1 } else { 0 };
                self.note_written(1, k, ts, Some(f.seq));
                self.used += n;
                act.appended_aus += 1;
                act.appended_bytes += n;
                self.evict();
            }
            Err(e) => self.halt("write", &e, act),
        }
    }

    fn write_open(&mut self, data: &[u8]) -> io::Result<u64> {
        let n = data.len() as u64;
        match &mut self.state {
            State::Writing(seg) => {
                seg.file.write_all(data)?;
                seg.bytes += n;
                Ok(n)
            }
            _ => Ok(0),
        }
    }

    fn note_written(&mut self, aus: u32, keys: u32, last_ts: u64, last_seq: Option<u64>) {
        if let State::Writing(seg) = &mut self.state {
            seg.au_count += aus;
            seg.keyframe_count += keys;
            if last_ts >= seg.last_ts_ms {
                seg.last_ts_ms = last_ts;
            }
            if last_seq.is_some() {
                seg.last_seq = last_seq;
            }
        }
    }

    /// Close the open segment and queue it.
    fn close(&mut self, reason: CloseReason) {
        let seg = match std::mem::replace(&mut self.state, State::Idle) {
            State::Writing(s) => s,
            other => {
                self.state = other;
                return;
            }
        };
        let OpenSeg {
            path,
            sidecar,
            mut file,
            start_unix_ms,
            start_ts_ms,
            trigger_unix_ms,
            trigger_ts_ms,
            last_ts_ms,
            bytes,
            au_count,
            keyframe_count,
            width,
            height,
            ..
        } = seg;
        let mut out = Segment {
            path,
            sidecar,
            start_unix_ms,
            start_ts_ms,
            trigger_unix_ms,
            trigger_ts_ms,
            duration_ms: last_ts_ms.saturating_sub(start_ts_ms),
            bytes,
            sidecar_bytes: 0,
            au_count,
            keyframe_count,
            width,
            height,
            reason,
        };
        // Get the video on the card first, then describe it. A power cut in
        // between leaves a decodable .h264 with no sidecar, which `scan` treats
        // as a finished segment with unknown metadata.
        if let Err(e) = file.flush().and_then(|_| file.sync_all()) {
            if self.halted.is_none() {
                eprintln!("camrtsp rec: sync {}: {e}", out.path.display());
            }
        }
        drop(file);
        match write_sidecar(&out) {
            Ok(n) => {
                out.sidecar_bytes = n;
                self.used += n;
            }
            Err(e) => {
                if self.halted.is_none() {
                    eprintln!("camrtsp rec: sidecar {}: {e}", out.sidecar.display());
                }
            }
        }
        self.finished.push(Finished {
            path: out.path.clone(),
            sidecar: out.sidecar.clone(),
            h264_bytes: out.bytes,
            json_bytes: out.sidecar_bytes,
        });
        self.pending.push_back(out);
        self.evict();
    }

    /// Create the .h264 with `create_new`, adding `-2`, `-3` on collision.
    fn create_file(&self, start_unix_ms: u64) -> io::Result<(File, PathBuf)> {
        let base = format_stamp(start_unix_ms);
        for n in 1..=99u32 {
            let stem = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let path = self.dir.join(format!("{stem}.h264"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => return Ok((f, path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("rec: 99 segments already named {base}"),
        ))
    }

    /// Quota: delete the oldest finished segments until we fit. The open
    /// segment is never a candidate (it is not in `finished`), and neither is
    /// the newest finished one, so the last event recorded is never the file
    /// the quota throws away.
    fn evict(&mut self) -> u32 {
        let mut n = 0;
        while self.used > self.cfg.quota_bytes && self.finished.len() > 1 {
            let f = self.finished.remove(0);
            let mut freed = 0u64;
            match remove_if_present(&f.path) {
                Ok(_) => freed += f.h264_bytes,
                Err(e) => {
                    if !self.quota_warned {
                        self.quota_warned = true;
                        eprintln!("camrtsp rec: cannot delete {}: {e}", f.path.display());
                    }
                    self.finished.insert(0, f);
                    break;
                }
            }
            match remove_if_present(&f.sidecar) {
                Ok(_) => freed += f.json_bytes,
                Err(e) => {
                    if !self.quota_warned {
                        self.quota_warned = true;
                        eprintln!("camrtsp rec: cannot delete {}: {e}", f.sidecar.display());
                    }
                }
            }
            self.used = self.used.saturating_sub(freed);
            n += 1;
            self.evicted_total += 1;
        }
        n
    }

    /// Stop for good. Print once, keep the bytes we have, drop the ring.
    fn halt(&mut self, what: &str, e: &io::Error, act: &mut RecordAction) {
        if self.halted.is_some() {
            return;
        }
        let msg = format!("{what}: {e}");
        eprintln!("camrtsp rec: {msg} — recording stopped (segments already on the card are kept)");
        self.halted = Some(msg.clone());
        act.error = Some(msg);
        self.ring.clear();
        self.ring_bytes = 0;
        self.arming_since = None;
        if matches!(self.state, State::Arming) {
            self.state = State::Idle;
        }
        // Best effort: finalise what we have. The sync and sidecar writes
        // inside `close` fail too if the card is gone; they only print.
        self.close(CloseReason::Error);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.is_recording() {
            self.close(CloseReason::Stop);
        }
    }
}

// ---- helpers -------------------------------------------------------------

/// NAL units to a 4-byte-start-code Annex-B access unit.
fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = nals.iter().map(|n| n.len() + START_CODE.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    out
}

/// Dimensions from the SPS in an Annex-B AU, if it carries one.
fn sps_dims(au: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0usize;
    while i + 4 <= au.len() {
        if au[i..].starts_with(&START_CODE) {
            let start = i + 4;
            let end = match next_start_code(au, start) {
                Some(e) => e,
                None => au.len(),
            };
            if start < end && nal_type(&au[start..]) == 7 {
                return sps_dimensions(&au[start..end]);
            }
            i = start;
        } else {
            i += 1;
        }
    }
    None
}

fn next_start_code(au: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 4 <= au.len() {
        if au[i..].starts_with(&START_CODE) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Delete a file; `false` if it was already gone.
fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The sidecar, hand-written: no serde on this device and the shape is fixed.
fn write_sidecar(seg: &Segment) -> io::Result<u64> {
    let name = seg
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("segment.h264");
    let mut s = String::with_capacity(512);
    s.push_str("{\n");
    s.push_str(&format!("  \"file\": \"{}\",\n", json_escape(name)));
    s.push_str(&format!("  \"start_unix_ms\": {},\n", seg.start_unix_ms));
    s.push_str(&format!("  \"start_ts_ms\": {},\n", seg.start_ts_ms));
    s.push_str(&format!("  \"trigger_unix_ms\": {},\n", seg.trigger_unix_ms));
    s.push_str(&format!("  \"trigger_ts_ms\": {},\n", seg.trigger_ts_ms));
    s.push_str(&format!("  \"duration_ms\": {},\n", seg.duration_ms));
    s.push_str(&format!("  \"bytes\": {},\n", seg.bytes));
    s.push_str(&format!("  \"au_count\": {},\n", seg.au_count));
    s.push_str(&format!(
        "  \"keyframe_count\": {},\n",
        seg.keyframe_count
    ));
    s.push_str(&format!("  \"width\": {},\n", seg.width));
    s.push_str(&format!("  \"height\": {},\n", seg.height));
    s.push_str(&format!("  \"cut_short\": {},\n", seg.cut_short()));
    s.push_str(&format!("  \"reason\": \"{}\",\n", seg.reason.as_str()));
    s.push_str(&format!(
        "  \"recorder\": \"camrtsp {}\"\n",
        json_escape(VERSION)
    ));
    s.push_str("}\n");
    // Direct write, no temp+rename: the file only appears now, so a power cut
    // mid-write leaves only a truncated sidecar, and the scan treats an
    // unparseable one as "no metadata".
    let mut f = File::create(&seg.sidecar)?;
    f.write_all(s.as_bytes())?;
    f.sync_all()?;
    Ok(s.len() as u64)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `YYYYMMDD-HHMMSS` in UTC.
pub fn format_stamp(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = days_to_civil(days);
    let (hh, mm, ss) = (rem / 3600, (rem / 60) % 60, rem % 60);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

/// Howard Hinnant's civil_from_days: days since 1970-01-01 to (y, m, d).
/// Avoids libc's gmtime on a uClibc target.
fn days_to_civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Adopt whatever earlier runs left in `<dir>/rec`.
fn scan(dir: &Path) -> io::Result<(Vec<Finished>, u64)> {
    let mut out: Vec<Finished> = Vec::new();
    let mut used = 0u64;
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((out, 0)),
        Err(e) => return Err(e),
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
            "h264" => {
                used += size;
                let sidecar = path.with_extension("json");
                out.push(Finished {
                    path,
                    sidecar,
                    h264_bytes: size,
                    json_bytes: 0,
                });
            }
            "json" => used += size,
            _ => {}
        }
    }
    for f in out.iter_mut() {
        f.json_bytes = fs::metadata(&f.sidecar).map(|m| m.len()).unwrap_or(0);
    }
    // Lexicographic on the file name is chronological here: fixed-width stamp,
    // then an optional `-N` suffix.
    out.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
    Ok((out, used))
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{is_vcl, split_annexb};
    use std::sync::atomic::{AtomicU32, Ordering};

    // A temp dir per test, removed on drop. Declared before the recorder in
    // each test, so the recorder (and its Drop) is torn down first.
    struct Tmp(PathBuf);

    static NEXT: AtomicU32 = AtomicU32::new(0);

    impl Tmp {
        fn new() -> Tmp {
            let p = std::env::temp_dir().join(format!(
                "camrtsp-rec-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn dir(&self) -> &Path {
            &self.0
        }
        fn rec(&self) -> PathBuf {
            self.0.join(SUBDIR)
        }
        fn with_ext(&self, ext: &str) -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = fs::read_dir(self.rec())
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        }
        fn h264s(&self) -> Vec<PathBuf> {
            self.with_ext("h264")
        }
        fn jsons(&self) -> Vec<PathBuf> {
            self.with_ext("json")
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // The SC1346 stream's real 1280x720 SPS, escapes and all (same bytes as
    // media.rs's `camera_sps_is_1280x720`), so dimension extraction is
    // exercised on the thing the camera actually emits.
    const SPS: [u8; 24] = [
        0x27, 0x4d, 0xe0, 0x28, 0x8d, 0x68, 0x05, 0x00, 0x5b, 0xa1, 0x00, 0x00, 0x03, 0x00, 0x01,
        0x00, 0x00, 0x03, 0x00, 0x32, 0x0f, 0x14, 0x22, 0xa0,
    ];
    const PPS: [u8; 4] = [0x68, 0xce, 0x38, 0x80];

    /// Bytes an IDR access unit takes in a file: three NALs, four-byte starts.
    fn key_au_len(slice_len: usize) -> usize {
        4 + SPS.len() + 4 + PPS.len() + 4 + slice_len
    }

    /// 12 s of wall clock we can predict filenames from.
    const WALL: u64 = 1_761_415_500_000; // 2025-10-25 18:05:00 UTC

    fn slice(t: u8, len: usize) -> Vec<u8> {
        let mut v = vec![t];
        v.extend((0..len.saturating_sub(1)).map(|k| (k % 251) as u8));
        v
    }

    fn key_frame(seq: u64, ts: u64, len: usize) -> VideoFrame {
        VideoFrame {
            nals: vec![SPS.to_vec(), PPS.to_vec(), slice(0x65, len)],
            keyframe: true,
            ts_ms: ts,
            seq,
        }
    }

    fn p_frame(seq: u64, ts: u64, len: usize) -> VideoFrame {
        VideoFrame {
            nals: vec![slice(0x41, len)],
            keyframe: false,
            ts_ms: ts,
            seq,
        }
    }

    /// Frame `seq` of a stream whose every `gop`-th frame is an IDR.
    fn frame(seq: u64, ts: u64, gop: u64, len: usize) -> VideoFrame {
        if gop != 0 && seq % gop == 0 {
            key_frame(seq, ts, len)
        } else {
            p_frame(seq, ts, len)
        }
    }

    fn cfg() -> RecConfig {
        RecConfig::default()
    }

    /// Split a recorded file into access units (a new AU starts at a VCL NAL
    /// that follows a VCL NAL).
    fn aus_of(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut cur: Vec<Vec<u8>> = Vec::new();
        let mut cur_vcl = false;
        for n in split_annexb(data) {
            let t = nal_type(&n);
            if cur_vcl && is_vcl(t) {
                out.push(std::mem::take(&mut cur));
                cur_vcl = false;
            }
            cur_vcl |= is_vcl(t);
            cur.push(n);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    fn sidecar_text(seg: &Segment) -> String {
        fs::read_to_string(&seg.sidecar).unwrap()
    }

    // -- the calendar bit (a wrong year in a filename is a wrong date) ------

    #[test]
    fn stamp_is_utc_and_handles_leap_days() {
        assert_eq!(format_stamp(0), "19700101-000000");
        assert_eq!(format_stamp(1_700_000_000_000), "20231114-221320");
        assert_eq!(format_stamp(1_709_208_000_000), "20240229-120000");
        assert_eq!(format_stamp(WALL), "20251025-180500");
        // Sub-second truncation, not rounding.
        assert_eq!(format_stamp(WALL + 999), "20251025-180500");
        assert_eq!(format_stamp(WALL + 1000), "20251025-180501");
    }

    // -- pre-roll ----------------------------------------------------------

    #[test]
    fn preroll_starts_three_seconds_before_the_trigger_on_a_keyframe() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 3_000;
        c.post_roll_ms = 3_000;
        c.min_segment_len_ms = 1_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();

        // 15 fps, IDR every 5th frame, nothing moving.
        for seq in 0..60u64 {
            let a = r.on_frame(&frame(seq, seq * 66, 5, 64), false).unwrap();
            assert!(a.is_quiet(), "idle frames must not open anything");
        }
        assert!(!r.is_recording());
        assert!(t.h264s().is_empty());

        // Motion at ts 3960. The oldest AU still in the ring is ts 990 (the
        // ring aged out 924 because 3960 - 924 > 3000); ts 990 is an IDR, so
        // that is where the file starts: AUs 990..3960 = 46 of them, 10 IDRs.
        let act = r.on_frame(&key_frame(60, 3_960, 64), true).unwrap();
        assert!(r.is_recording());
        let opened = act.opened.clone().expect("segment opened on the trigger");
        assert_eq!(
            opened.file_name().unwrap().to_str().unwrap(),
            format!("{}.h264", format_stamp(WALL + 990))
        );

        // One quiet frame (motion level false) then let the post-roll expire
        // on the clock: no frames need to arrive for a close to happen.
        r.on_frame(&p_frame(61, 4_026, 64), false).unwrap();
        let seg = r.tick(7_000).unwrap().expect("closed by post-roll");
        assert_eq!(seg.reason, CloseReason::PostRoll);
        assert!(!seg.cut_short());
        assert_eq!(seg.start_ts_ms, 990);
        assert_eq!(seg.trigger_ts_ms, 3_960);
        assert_eq!(seg.start_unix_ms, WALL + 990);
        assert_eq!(seg.trigger_unix_ms, WALL + 3_960);
        assert_eq!(seg.au_count, 47); // 46 pre-roll + the quiet frame
        assert_eq!(seg.keyframe_count, 10);
        assert_eq!(seg.duration_ms, 4_026 - 990);
        assert_eq!(seg.width, 1280);
        assert_eq!(seg.height, 720);
        assert_eq!(seg.bytes, fs::metadata(&seg.path).unwrap().len());

        // Byte for byte: 46 access units, first one an IDR with SPS/PPS.
        let data = read(&seg.path);
        assert_eq!(&data[..4], &START_CODE, "file must open on a start code");
        let aus = aus_of(&data);
        assert_eq!(aus.len(), 47);
        assert_eq!(nal_type(&aus[0][0]), 7, "segment must start on the SPS");
        assert_eq!(nal_type(&aus[0][1]), 8);
        assert_eq!(nal_type(&aus[0][2]), 5);
        for (i, au) in aus.iter().enumerate() {
            assert!(!au.is_empty(), "au {i} empty");
        }
        // Everything after the last AU: nothing. (Files must end on an AU.)
        assert_eq!(data.len() as u64, seg.bytes);
        assert_eq!(data[data.len() - 1], *aus.last().unwrap().last().unwrap().last().unwrap());

        // Sidecar: hand-written JSON, written only now that it is closed.
        let js = sidecar_text(&seg);
        assert_eq!(t.jsons().len(), 1);
        for must in [
            "\"au_count\": 47,",
            "\"keyframe_count\": 10,",
            "\"cut_short\": false,",
            "\"reason\": \"post_roll\",",
            "\"width\": 1280,",
            "\"height\": 720,",
            &format!("\"file\": \"{}\",", format!("{}.h264", format_stamp(WALL + 990))),
        ] {
            assert!(js.contains(must), "sidecar missing {must}\n{js}");
        }
        assert!(js.trim_end().ends_with('}'));
        assert_eq!(seg.sidecar_bytes, js.len() as u64);
        assert_eq!(r.space_used(), seg.bytes + seg.sidecar_bytes);
        assert_eq!(r.finished_count(), 1);
    }

    #[test]
    fn preroll_is_capped_by_bytes() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_max_bytes = 1_000; // 204 B per AU ⇒ 4 AUs fit, a 5th does not
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        // Every frame an IDR, so the byte cap is what decides the start (with
        // sparse IDRs the keyframe rule would dominate and hide the cap).
        for seq in 0..50u64 {
            r.on_frame(&key_frame(seq, seq * 66, 200), false).unwrap();
        }
        r.on_frame(&key_frame(50, 50 * 66, 200), true).unwrap();
        // 240 B per IDR: four fit under 1 kB, five would be 1200 B.
        let open = r.open_path().expect("open").to_path_buf();
        let data = read(&open);
        assert_eq!(aus_of(&data).len(), 4, "byte cap did not bite");
        assert_eq!(data.len(), 4 * key_au_len(200));
        r.on_frame(&key_frame(51, 51 * 66, 200), false).unwrap();
        let seg = r.tick(200_000).unwrap().expect("closed");
        assert_eq!(seg.au_count, 5, "four pre-roll AUs plus the appended frame");
        assert_eq!(seg.keyframe_count, 5);
    }

    #[test]
    fn preroll_is_capped_by_access_unit_count_and_trimmed_to_a_keyframe() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 60_000; // time cap out of the way
        c.pre_roll_max_aus = 8;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        for seq in 0..50u64 {
            r.on_frame(&frame(seq, seq * 66, 5, 64), false).unwrap();
        }
        r.on_frame(&key_frame(50, 50 * 66, 64), true).unwrap();
        r.on_frame(&p_frame(51, 51 * 66, 64), false).unwrap();
        let seg = r.tick(200_000).unwrap().expect("closed");
        // The ring holds AUs 44..51 (8 of them); the first keyframe is 45, so
        // the segment starts there: AUs 45..51 = 7, two IDRs (45 and 50).
        assert_eq!(seg.au_count, 7);
        assert_eq!(seg.keyframe_count, 2);
        assert_eq!(seg.start_ts_ms, 45 * 66);
    }

    // -- post-roll, min length, stop --------------------------------------

    #[test]
    fn post_roll_is_recorded_before_the_file_closes() {
        let t = Tmp::new();
        let mut c = cfg();
        c.post_roll_ms = 500;
        c.min_segment_len_ms = 0;
        c.pre_roll_ms = 0; // no pre-roll, so the numbers are about post-roll
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        // Motion for 3 frames, then quiet; the quiet frames still get recorded.
        let mut closed = None;
        for seq in 0..12u64 {
            let motion = (2..5).contains(&seq);
            let a = r.on_frame(&key_frame(seq, seq * 100, 64), motion).unwrap();
            if let Some(seg) = r.tick(seq * 100).unwrap() {
                closed = Some(seg);
                break;
            }
            assert_eq!(a.pending, 0);
        }
        let seg = closed.expect("post-roll closed the file");
        assert_eq!(seg.reason, CloseReason::PostRoll);
        // Trigger at ts 200, last motion at ts 400, post-roll 500 ⇒ closed by
        // the first frame with ts - 400 >= 500, i.e. ts 900: the file runs
        // 200..900, eight AUs, and the 500 ms of quiet is inside it.
        assert_eq!(seg.start_ts_ms, 200);
        assert_eq!(seg.trigger_ts_ms, 200);
        assert_eq!(seg.duration_ms, 700);
        assert_eq!(seg.au_count, 8);
        assert!(!r.is_recording());
        assert_eq!(t.h264s().len(), 1);
    }

    #[test]
    fn min_segment_len_stops_a_one_frame_blip_from_being_a_stub() {
        let t = Tmp::new();
        let mut c = cfg();
        c.post_roll_ms = 0;
        c.min_segment_len_ms = 2_000;
        c.pre_roll_ms = 100;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        r.on_frame(&key_frame(0, 1_000, 64), true).unwrap();
        // Post-roll is 0, so the next quiet frame wants to close — but the
        // segment is younger than min_segment_len_ms.
        r.on_frame(&p_frame(1, 1_100, 64), false).unwrap();
        assert!(r.is_recording(), "closed before the minimum length");
        let mut seg = None;
        for seq in 2..40u64 {
            r.on_frame(&p_frame(seq, 1_000 + seq * 100, 64), false).unwrap();
            if let Some(s) = r.tick(1_000 + seq * 100).unwrap() {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("closed once the minimum was met");
        assert!(
            seg.duration_ms >= 2_000,
            "duration {} shorter than min",
            seg.duration_ms
        );
    }

    #[test]
    fn stop_closes_and_marks_the_segment_cut_short() {
        let t = Tmp::new();
        let mut c = cfg();
        c.post_roll_ms = 60_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        for seq in 0..5u64 {
            r.on_frame(&frame(seq, seq * 66, 5, 64), true).unwrap();
        }
        let seg = r.stop().unwrap().expect("stop returns the open segment");
        assert_eq!(seg.reason, CloseReason::Stop);
        assert!(seg.cut_short());
        assert!(sidecar_text(&seg).contains("\"cut_short\": true,"));
        assert!(sidecar_text(&seg).contains("\"reason\": \"stop\","));
        assert!(!r.is_recording());
        assert!(r.stop().unwrap().is_none(), "second stop has nothing to do");
        assert_eq!(r.pending_closed(), 0, "stop hands its segment over");
    }

    // -- rotation ---------------------------------------------------------

    #[test]
    fn rotates_at_max_segment_len_and_every_file_starts_on_a_keyframe() {
        let t = Tmp::new();
        let mut c = cfg();
        c.max_segment_len_ms = 200;
        c.post_roll_ms = 60_000;
        c.min_segment_len_ms = 0;
        c.pre_roll_ms = 3_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        // Every frame an IDR (the stock encoder runs GOP 2, so this is close to
        // real), 20 fps, motion throughout.
        for seq in 0..100u64 {
            r.on_frame(&key_frame(seq, seq * 50, 64), true).unwrap();
        }
        let closed = r.drain_closed();
        assert!(closed.len() >= 15, "only {} segments", closed.len());
        for seg in &closed {
            assert_eq!(seg.reason, CloseReason::Rotate);
            assert!(
                seg.duration_ms <= 200,
                "segment {} ran {} ms",
                seg.path.display(),
                seg.duration_ms
            );
            let data = read(&seg.path);
            let aus = aus_of(&data);
            assert_eq!(nal_type(&aus[0][0]), 7, "{} is not keyframe-first", seg.path.display());
            assert_eq!(aus.len() as u32, seg.au_count);
        }
        // 20 fps, 200 ms: five AUs per segment, and the new file starts on the
        // AU the old one ended on (one frame of deliberate overlap, never a
        // mid-GOP start and never a gap).
        for seg in &closed {
            assert_eq!(seg.au_count, 5, "{}", seg.path.display());
        }
        assert!(r.is_recording(), "still recording after the last rotation");
        let open = r.open_path().unwrap().to_path_buf();
        let aus = aus_of(&read(&open));
        assert_eq!(aus.len(), 4, "the tail after the last rotation: {} AUs", aus.len());
        assert_eq!(nal_type(&aus[0][0]), 7);
    }

    // -- quota ------------------------------------------------------------

    #[test]
    fn quota_deletes_the_oldest_finished_segments() {
        let t = Tmp::new();
        let mut c = cfg();
        c.quota_bytes = 2_000;
        c.max_segment_len_ms = 100;
        c.post_roll_ms = 60_000;
        c.min_segment_len_ms = 0;
        c.pre_roll_ms = 0;
        let quota = c.quota_bytes;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        let mut first_name = None;
        let mut evicted_seen = 0u64;
        for seq in 0..200u64 {
            let a = r.on_frame(&key_frame(seq, seq * 50, 200), true).unwrap();
            evicted_seen += a.evicted as u64;
            if first_name.is_none() && r.finished_count() > 0 {
                first_name = r.finished().next().map(|p| p.to_path_buf());
            }
        }
        let first = first_name.expect("something closed");
        assert!(
            !first.exists(),
            "the oldest segment should have been evicted: {}",
            first.display()
        );
        assert!(r.finished_count() <= 3, "kept {} segments", r.finished_count());
        let files = t.h264s();
        assert!(files.len() <= 5, "{} files on the card", files.len());
        // The file being written is never a candidate.
        let open = r.open_path().unwrap().to_path_buf();
        assert!(open.exists(), "quota deleted the open segment");
        // Every deletion is reported once, in the call that made it.
        assert!(evicted_seen > 0, "quota never reported an eviction");
        // Bounded: quota plus at most one finished and one open segment.
        assert!(
            r.space_used() <= quota + 2_000,
            "used {} over quota",
            r.space_used()
        );
    }

    #[test]
    fn the_last_finished_segment_is_never_evicted() {
        let t = Tmp::new();
        let mut c = cfg();
        c.quota_bytes = 1; // absurdly small: everything is over quota
        c.max_segment_len_ms = 100;
        c.post_roll_ms = 60_000;
        c.min_segment_len_ms = 0;
        c.pre_roll_ms = 0;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        for seq in 0..80u64 {
            r.on_frame(&key_frame(seq, seq * 50, 200), true).unwrap();
        }
        assert_eq!(r.finished_count(), 1, "keep exactly the newest one");
        assert!(t.h264s().len() <= 2);
    }

    #[test]
    fn existing_segments_are_adopted_and_counted() {
        let t = Tmp::new();
        fs::create_dir_all(t.rec()).unwrap();
        let old = t.rec().join("20200101-000000.h264");
        fs::write(&old, vec![0u8; 1_000]).unwrap();
        fs::write(t.rec().join("20200101-000000.json"), vec![b' '; 100]).unwrap();
        let mut c = cfg();
        c.quota_bytes = 1_200; // the old segment alone nearly fills it
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        assert_eq!(r.space_used(), 1_100);
        assert_eq!(r.finished_count(), 1);
        assert_eq!(r.finished().next(), Some(old.as_path()));

        // A new segment pushes it over the quota, so the old one goes.
        r.on_frame(&key_frame(0, 0, 200), true).unwrap();
        r.on_frame(&key_frame(1, 50, 200), false).unwrap();
        r.tick(120_000).unwrap();
        assert!(!old.exists(), "old segment should be evicted by the quota");
        assert_eq!(r.finished_count(), 1);
    }

    #[test]
    fn remove_deletes_a_segment_after_upload() {
        let t = Tmp::new();
        let mut c = cfg();
        c.post_roll_ms = 10;
        c.min_segment_len_ms = 0;
        c.pre_roll_ms = 0;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        r.on_frame(&key_frame(0, 0, 64), true).unwrap();
        r.on_frame(&key_frame(1, 50, 64), false).unwrap();
        let seg = r.tick(10_000).unwrap().expect("closed");
        assert!(seg.path.exists() && seg.sidecar.exists());
        let used = r.space_used();
        r.remove(&seg.path).unwrap();
        assert!(!seg.path.exists() && !seg.sidecar.exists());
        assert_eq!(r.space_used(), used - seg.total_bytes());
        assert_eq!(r.finished_count(), 0);
        assert!(r.remove(&seg.path).is_err(), "unknown path must error");
    }

    // -- the card vanishing ----------------------------------------------

    #[test]
    fn a_create_error_latches_off_without_panicking() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 0;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        // Take the card away: the rec directory is now a regular file, so
        // creating a segment cannot work.
        fs::remove_dir_all(t.rec()).unwrap();
        fs::write(t.rec(), b"not a directory").unwrap();

        let act = r.on_frame(&key_frame(0, 0, 64), true).unwrap();
        let err = act.error.expect("error reported, not returned as Err");
        assert!(err.contains("create"), "{err}");
        assert!(r.is_halted());
        // Latched: later frames are quietly ignored, and the error is only
        // reported once.
        for seq in 1..10u64 {
            let a = r.on_frame(&key_frame(seq, seq * 66, 64), true).unwrap();
            assert!(a.error.is_none());
            assert!(a.is_quiet());
        }
        assert!(r.halted_reason().is_some());
        assert!(r.tick(100_000).unwrap().is_none());
    }

    #[test]
    fn a_vanished_card_mid_recording_latches_off_and_keeps_the_data() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 0;
        c.max_segment_len_ms = 100; // rotate soon after the card goes away
        c.post_roll_ms = 60_000;
        c.min_segment_len_ms = 0;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        r.on_frame(&key_frame(0, 0, 64), true).unwrap();
        r.on_frame(&key_frame(1, 50, 64), true).unwrap();
        assert!(r.is_recording());
        // Pull the card out from under it. The open fd keeps working (that is
        // exactly what happens on this board — it is how the first yank in this
        // project went unnoticed), the *next* segment cannot be created, and
        // that must not panic, not spin, and not lose what was written.
        fs::remove_dir_all(t.rec()).unwrap();
        let mut errored = None;
        for seq in 2..20u64 {
            let a = r.on_frame(&key_frame(seq, seq * 50, 64), true).unwrap();
            if errored.is_none() {
                errored = a.error;
            }
        }
        let err = errored.expect("a vanished card must be reported, not swallowed");
        assert!(err.contains("create"), "{err}");
        assert!(r.is_halted());
        // The rotation that failed still handed over the segment it had
        // finished, with its frames intact.
        let seg = r.tick(200_000).unwrap().expect("segment handed over");
        assert!(seg.au_count >= 2, "data written before the failure is kept");
        assert!(!seg.path.exists(), "the card is gone: the file is unlinked");
        // And everything after that is ignored quietly.
        assert!(r.on_frame(&key_frame(99, 99 * 50, 64), true).unwrap().is_quiet());
    }

    #[test]
    fn a_write_error_closes_the_open_segment_as_cut_short() {
        // White box: a real ENOSPC cannot be manufactured here without root,
        // so call the branch the way a full card would.
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 0;
        c.post_roll_ms = 60_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        for seq in 0..4u64 {
            r.on_frame(&key_frame(seq, seq * 66, 64), true).unwrap();
        }
        let open = r.open_path().unwrap().to_path_buf();
        let mut act = RecordAction::default();
        let e = io::Error::new(io::ErrorKind::Other, "No space left on device");
        r.halt("write", &e, &mut act);
        assert_eq!(act.error.as_deref(), Some("write: No space left on device"));
        assert!(r.is_halted());
        assert!(!r.is_recording());
        let seg = r.tick(100_000).unwrap().expect("segment finalised");
        assert_eq!(seg.reason, CloseReason::Error);
        assert!(seg.cut_short());
        assert_eq!(seg.au_count, 4);
        assert_eq!(seg.bytes, fs::metadata(&open).unwrap().len());
        let js = sidecar_text(&seg);
        assert!(js.contains("\"cut_short\": true,"), "{js}");
        assert!(js.contains("\"reason\": \"error\","), "{js}");
        // Halting twice does not double-close or double-report.
        let mut act2 = RecordAction::default();
        r.halt("write", &e, &mut act2);
        assert!(act2.error.is_none());
        assert!(r.tick(100_001).unwrap().is_none());
    }

    // -- keyframes --------------------------------------------------------

    #[test]
    fn with_no_keyframe_in_the_ring_it_starts_anyway_after_keyframe_wait() {
        let t = Tmp::new();
        let mut c = cfg();
        c.keyframe_wait_ms = 500;
        c.pre_roll_ms = 3_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        // A stream with no IDR at all (someone pointed us at a weird encoder).
        assert!(r.on_frame(&p_frame(0, 0, 64), true).unwrap().opened.is_none());
        assert!(!r.is_recording());
        let mut opened = None;
        for seq in 1..30u64 {
            let a = r.on_frame(&p_frame(seq, seq * 66, 64), true).unwrap();
            if a.opened.is_some() {
                opened = a.opened;
                break;
            }
        }
        let path = opened.expect("started anyway after keyframe_wait_ms");
        assert!(r.is_recording());
        let data = read(&path);
        let aus = aus_of(&data);
        assert_ne!(nal_type(&aus[0][0]), 7, "there was no keyframe to start on");
    }

    #[test]
    fn arming_gives_up_quietly_when_the_blip_ends_before_a_keyframe() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 3_000;
        c.post_roll_ms = 100;
        c.keyframe_wait_ms = 5_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        r.on_frame(&p_frame(0, 0, 64), true).unwrap();
        r.on_frame(&p_frame(1, 66, 64), false).unwrap();
        for seq in 2..20u64 {
            r.on_frame(&p_frame(seq, seq * 66, 64), false).unwrap();
        }
        drop(r);
        assert!(t.h264s().is_empty(), "nothing worth recording, nothing written");
        assert!(t.jsons().is_empty());
    }

    // -- frame hygiene ----------------------------------------------------

    #[test]
    fn duplicate_frames_are_not_written_twice() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 200;
        c.post_roll_ms = 60_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        for seq in 0..5u64 {
            r.on_frame(&frame(seq, seq * 66, 5, 64), true).unwrap();
        }
        let before = r.space_used();
        // The same frame again (a source that re-delivers on reconnect).
        r.on_frame(&frame(4, 4 * 66, 5, 64), true).unwrap();
        assert_eq!(r.space_used(), before);
        let aus = aus_of(&read(r.open_path().unwrap()));
        let seqs: Vec<&Vec<u8>> = aus.iter().map(|a| &a[0]).collect();
        assert_eq!(aus.len(), 5, "duplicate AU in the file");
        assert!(!seqs.is_empty());
    }

    #[test]
    fn empty_frames_are_ignored() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 0;
        c.post_roll_ms = 60_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        let empty = VideoFrame {
            nals: Vec::new(),
            keyframe: false,
            ts_ms: 10,
            seq: 0,
        };
        let a = r.on_frame(&empty, true).unwrap();
        assert!(a.opened.is_none());
        assert!(r.open_path().is_none());
        // A zero-length NAL is not a frame either, but must not corrupt the AU.
        let zero = VideoFrame {
            nals: vec![Vec::new()],
            keyframe: false,
            ts_ms: 20,
            seq: 1,
        };
        r.on_frame(&zero, true).unwrap();
        assert!(r.open_path().is_none());
    }

    #[test]
    fn a_backwards_clock_does_not_rewind_the_recorder() {
        let t = Tmp::new();
        let mut c = cfg();
        c.pre_roll_ms = 0;
        c.post_roll_ms = 60_000;
        c.max_segment_len_ms = 60_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        r.on_frame(&key_frame(0, 5_000, 64), true).unwrap();
        // A frame from before the first one (source restart with a fresh clock).
        r.on_frame(&key_frame(1, 10, 64), true).unwrap();
        let seg = r.stop().unwrap().unwrap();
        assert_eq!(seg.start_ts_ms, 5_000);
        assert!(
            seg.duration_ms <= 60_000,
            "duration {} looks like a clock jump",
            seg.duration_ms
        );
    }

    #[test]
    fn stop_while_arming_writes_nothing() {
        let t = Tmp::new();
        let mut c = cfg();
        c.keyframe_wait_ms = 60_000;
        let mut r = Recorder::with_wall_clock(t.dir(), c, WALL).unwrap();
        // Motion, but only P frames, so we are arming and waiting.
        r.on_frame(&p_frame(0, 0, 64), true).unwrap();
        assert!(r.stop().unwrap().is_none());
        // A keyframe turning up later must not resurrect the recording.
        for seq in 1..6u64 {
            r.on_frame(&key_frame(seq, seq * 66, 64), true).unwrap();
        }
        assert!(!r.is_recording());
        assert!(t.h264s().is_empty());
    }

    #[test]
    fn config_is_validated() {
        let t = Tmp::new();
        let mut c = cfg();
        c.max_segment_len_ms = 0;
        assert!(Recorder::with_wall_clock(t.dir(), c, WALL).is_err());
        let mut c = cfg();
        c.pre_roll_max_aus = 0;
        assert!(Recorder::with_wall_clock(t.dir(), c, WALL).is_err());
    }

    #[test]
    fn the_rec_dir_is_created_under_the_card_root() {
        let t = Tmp::new();
        let nested = t.dir().join("mnt");
        let r = Recorder::new(&nested, cfg()).unwrap();
        assert!(nested.join("rec").is_dir());
        assert_eq!(r.rec_dir(), nested.join("rec").as_path());
        // Wall clock comes from the system, so it should look like a date.
        assert!(format_stamp(now_unix_ms()).len() == 15);
    }
}
