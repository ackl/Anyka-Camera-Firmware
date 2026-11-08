//! Motion detection from the compressed stream — no decoder, no vendor library.
//!
//! The AK3918 board has no motion-detection library in the image we run
//! (`libmpi_md.so` was never shipped; `libak_mt.so` is PTZ tracking, not
//! detection), and decoding 720p H.264 on an ARM926EJ-S to diff pixels is not
//! an option — it would eat the whole CPU the encoder needs. So detection works
//! on the only numbers we have without decoding: **the byte size of each access
//! unit, whether it is a keyframe, and when it arrived**.
//!
//! # What this is, honestly
//!
//! H.264 spends bits where the picture is changing. A frame that costs three
//! times what the last thirty cost means *something* in the shot changed. This
//! module measures that change; it does not know what changed. It is a **scene
//! change detector**, and v1 deliberately claims nothing more:
//!
//! * it cannot tell a person from a passing car, a cloud shadow, a tree in
//!   wind, or the camera's own auto-exposure stepping;
//! * a light switching on is a real, large change and will fire — correctly by
//!   the rules below, uselessly for an intruder alarm;
//! * a small, slow, far-away person on a low-bitrate stream can be compressed
//!   away almost entirely and be **missed** (see "Failure modes").
//!
//! Everything here is per-`&mut self` plain state: single-threaded, no atomics
//! (ARMv5TE has no `ldrexd`, so anything wider than a `u32` is a trap), no
//! allocation after construction, no `f64`, no panics on malformed input.
//!
//! # The signal
//!
//! For every access unit:
//!
//! * **P-frames carry the evidence.** Each AU's size is compared against the
//!   *median of the last `window` P-frame sizes* — never against an I-frame and
//!   never against a mixed average, because an I-frame is legitimately several
//!   times a P-frame and mixing the two would make every GOP boundary look like
//!   an earthquake.
//! * **A frame counts as evidence only if it clears both tests**: it must be at
//!   least `ratio_threshold` times the baseline *and* at least
//!   `min_delta_bytes` bigger. The ratio catches "much bigger than usual"; the
//!   absolute floor stops a nearly-static stream (say 200 B/AU) where +180 B is
//!   "2× the baseline" but is really quantiser noise from turning on.
//! * **Evidence accumulates with a half-life.** A single AU contributes
//!   `(ratio - threshold) / threshold`, floored at [`MIN_CONTRIBUTION`] and
//!   capped at [`MAX_CONTRIBUTION`]; the score decays by half every
//!   `score_half_life_ms`. [`TRIGGER_SCORE`] is 1.0, so one isolated frame only
//!   fires if it is roughly 3.5× the baseline (a cut, a light switch), while a
//!   steady 1.75–2× stream fires in about half a second. That is the difference
//!   between "a frame was odd" and "the scene is doing something".
//! * **The baseline only learns quiet frames.** An AU that counts as evidence is
//!   kept out of the median window, so a person walking through the shot cannot
//!   drag the baseline up behind them and blind the detector (that is the
//!   "latch" failure this design exists to avoid).
//! * **Keyframes never carry the alarm on their own.** IDRs are periodic, and
//!   on this camera the day/night ISP switch forces one. The I-frame channel is
//!   normalised against the *previous* I-frame only, and a single IDR can
//!   contribute at most `i_frame_weight` (0.25) of the 1.0 needed — so an IDR
//!   alone can never raise an alarm.
//! * **Hysteresis, because the recorder wants an edge.** `active` stays true for
//!   `hold_ms` after the last trigger; `just_started` is true only on the rising
//!   edge, so a recorder can start one clip per event instead of one per frame.
//!   Note that a trigger is refreshed on every AU whose evidence still adds up
//!   to ≥ 1.0, so a single big frame keeps the level up for the second or two
//!   the score takes to decay, and then for `hold_ms` more.
//! * **Time comes from `ts_ms`, not a frame counter.** The stream is 14.7 AU/s by
//!   day and ~10 at night, sessions stall, and a pull can be resumed; deltas
//!   drive both the decay and the gap detection.
//! * **A scene that stays changed is adopted, not alarmed on forever.** If
//!   evidence is continuous for `sustain_ms`, the baseline is re-seeded to the
//!   recent oversized sizes and the score is dropped: the new normal is the new
//!   normal. Without this, a camera nudged by the wind, or a truck parked in
//!   front of it, would alarm for as long as it stayed there — on an LTE link
//!   that is a data bill, not a feature.
//!
//! # Defaults, and why these numbers
//!
//! Measured from our own captures of this camera (`work/video/captures/`):
//! `/vs0` is 1280×720 and averages **0.97 Mbps at 14.7 AU/s by day** (≈8.2 kB per
//! access unit) and **~1.09 Mbps at ~10 AU/s at night with IR on** (≈13.6 kB per
//! unit). The night shift alone is therefore ≈1.65× the day mean *sustained*, so
//! `ratio_threshold` has to sit above it — hence **1.75**, not 1.5. A threshold
//! under ~1.65 would alarm at every dusk.
//!
//! Replaying those captures through this module (scratch harness, real numbers):
//!
//! | capture | AUs | P mean | I mean | edges |
//! |---|---|---|---|---|
//! | `main-20s.mp4` (day) | 284 | 7.1 kB | 71 kB | 1 |
//! | `main-5s.mp4` | 71 | 3.7 kB | 66 kB | 0 |
//! | `rec-225430.mp4` | 114 | 3.0 kB | 86 kB | 0 |
//! | `day-after-switch.mp4` | 69 | 9.8 kB | 57 kB | 0 |
//! | `night-isp-5s.mp4` (IR switch) | 50 | 11.5 kB | 112 kB | 1 |
//! | `coldboot-6s.mp4` (encoder ramp) | 85 | 2.5 kB | 34 kB | 1 |
//! | `sub-5s.mp4` (640×480) | 69 | 0.45 kB | 21 kB | 0 |
//!
//! Three things fall out of that table. **I-frames are ~10× a P-frame here**, not
//! "several times", so any detector that mixes the two is useless — that is what
//! `window` and the separate I-channel are for. **Daytime P-frames are stable**
//! (the quiet clips never came within 2× of a trigger). And **the three edges in
//! the set are exactly the three transitions in it** — a mid-capture scene change
//! at 13 s in `main-20s`, the IR/ISP night switch, and the encoder ramping up
//! after cold boot — which is the behaviour the recorder wants, and also the
//! honest reminder that this module cannot tell those three from an intruder.
//!
//! | field | default | why |
//! |---|---|---|
//! | `window` | 30 | ~2 s of P-frames at 15 fps: long enough that one odd frame cannot move the median, short enough to follow the encoder's bitrate allocation drifting |
//! | `min_samples` | 8 | no evidence until the baseline has seen 8 P-frames; protects the first frames of a session (and makes "one frame" harmless) |
//! | `ratio_threshold` | 1.75 | above the measured day→night shift (1.65), below "a person crossed the shot" (typically 2–4×) |
//! | `min_delta_bytes` | 1500 | a 720p change that costs less than 1.5 kB more is not worth waking a recorder for. Measured: the 640×480 `/vs1` substream averages 451 B per P-frame (max 909 B), so for `/vs1` set this to ~300 or nothing will ever fire |
//! | `score_half_life_ms` | 2000 | two seconds is long enough to accumulate ~7 frames of 2× into a trigger, short enough that scattered odd frames never add up |
//! | `hold_ms` | 5000 | "several seconds": the recorder gets one clip per event, not one per trigger |
//! | `sustain_ms` | 60000 | a minute of continuous change is a different scene, not an event |
//! | `settle_frames_after_idr` | 1 | the P-frame straight after an IDR is coded fresh and its size is not comparable; skip it (set 0 for very short GOPs) |
//! | `i_frame_weight` | 0.25 | an IDR can nudge the score but never trip it |
//! | `gap_ms` | 0 (derive) | 3× the expected AU interval, clamped to ≥500 ms: longer than that means a stall/reconnect, not a slow frame |
//!
//! # Tuning: what to change first when it misbehaves on real footage
//!
//! 1. **Too many alarms** (wind in leaves, rain, sensor grain at night): raise
//!    `ratio_threshold` to 2.0, then `min_delta_bytes` to 3000. Do this before
//!    touching anything else — it is the only knob with a physical meaning.
//! 2. **Alarms at dusk/dawn only**: set `i_frame_weight = 0.0` first (the ISP
//!    day/night switch forces an IDR), then raise `ratio_threshold`.
//! 3. **Misses people**: lower `ratio_threshold` toward 1.6 and
//!    `min_delta_bytes` toward 800, and shorten `score_half_life_ms` to 1000 so
//!    a brief burst still adds up. Watch the false-alarm rate at night.
//! 4. **Clips too long / back-to-back**: shorten `hold_ms`. **Chattering**
//!    (start-stop-start within a second): lengthen it.
//! 5. **Stuck on for a long time**: shorten `sustain_ms`, or raise `window` so the
//!    baseline follows a bitrate change faster (a shorter window adapts sooner,
//!    at the cost of noise sensitivity).
//! 6. **Nothing at all on a quiet stream**: check `min_delta_bytes` against the
//!    actual P-frame size — print `Detector::baseline_bytes()`; on a 200 kB/AU
//!    stream 1500 B is invisible, on a 1 kB/AU substream it is the whole test.
//!
//! # Failure modes worth knowing about
//!
//! * **A slow walker.** A distant person moving a few pixels per frame is cheap
//!    to code; on a 0.97 Mbps stream it may add 20–40 %, below the threshold.
//!    The honest answer is a second stream at a lower ratio threshold, or real
//!    pixel motion detection at 320×240 — not a lower threshold on this one,
//!    which would fire on the wind.
//! * **A light switching on.** Real change, fires immediately, and the following
//!    frames stay oversized for a second or two while AEC/AGC settle. Cannot be
//!    distinguished from an event by bitrate alone.
//! * **A passing car at night.** Headlights flood the sensor: a huge, short
//!    burst. It fires (probably desirable) but the clip is a car, not a person.
//! * **The camera's own night mode.** Bitrate and quantisation step up; the
//!    baseline follows over a few seconds and `sustain_ms` covers a stuck
//!    transition, but the *transition* itself can produce one false event.
//! * **Scene adopted as normal.** Anything that stays changed for `sustain_ms`
//!    becomes the baseline — including a person standing perfectly still.
//! * **The first seconds of a stream.** Encoder ramp-up plus auto-exposure
//!    settling produce one honest-looking edge (measured at 5.3 s in
//!    `coldboot-6s.mp4`). If that is not wanted, ignore `just_started` for the
//!    first few seconds after attaching — a caller's policy, not a knob here.
//! * **Out-of-order or retransmitted AUs** would feed the median twice; the RTP
//!    path over TCP-interleaved RTSP does not do this, and the detector assumes
//!    it does not.
//! * **Very short GOPs**: with an IDR every 2 frames, `settle_frames_after_idr`
//!    eats most P-frames; set it to 0 there.

// TEMPORARY: `main.rs` does not declare `mod motion;` yet. Drop this attribute
// (and this comment) in the same commit that wires the detector into the
// recorder, or the compiler will not tell us when it rots.
#![allow(dead_code)]

use std::sync::Arc;

use crate::media::{StreamInfo, VideoFrame};

/// Score at which `active` flips on.
const TRIGGER_SCORE: f32 = 1.0;
/// Ceiling on the score, so a long burst cannot saturate into weeks of decay.
const MAX_SCORE: f32 = 8.0;
/// Smallest per-AU contribution, so any qualifying frame costs at least this
/// much of the trigger — five of them inside the half-life are an event.
const MIN_CONTRIBUTION: f32 = 0.15;
/// Largest per-AU contribution: one frame can trigger on its own, but a single
/// absurd size (a truncated read, a burst-catch-up artefact) cannot bank more.
const MAX_CONTRIBUTION: f32 = 3.0;
/// Fixed storage: the median window is a `[u32; MAX_WINDOW]`, never a `Vec`.
const MAX_WINDOW: usize = 64;
/// How many recent oversized AUs to remember for a scene-change re-seed.
const MAX_EVIDENCE_HISTORY: usize = 16;

/// Tunables. [`Default`] is the profile that fits this camera (see module docs).
#[derive(Debug, Clone)]
pub struct MotionConfig {
    /// How many P-frame sizes the median baseline is taken over. Clamped to
    /// `1..=64`. Bigger = steadier but slower to follow a bitrate change.
    pub window: usize,
    /// Minimum baseline samples before any AU can count as evidence.
    pub min_samples: usize,
    /// How many times the baseline an AU must be to count. Must exceed the
    /// stream's own day→night bitrate shift (measured 1.65 here).
    pub ratio_threshold: f32,
    /// Absolute floor on `size - baseline`, so a tiny baseline cannot turn
    /// noise into motion. Bytes.
    pub min_delta_bytes: u32,
    /// Half-life of the evidence score, milliseconds.
    pub score_half_life_ms: u64,
    /// How long `active` stays true after the last trigger, milliseconds.
    pub hold_ms: u64,
    /// Continuous evidence this long means the scene has changed: adopt it.
    /// `0` disables the re-seed (and then a permanent change alarms forever).
    pub sustain_ms: u64,
    /// P-frames to ignore after an IDR (they are coded fresh, not comparable).
    pub settle_frames_after_idr: u8,
    /// Weight of the I-frame channel, normalised against the previous I-frame.
    /// Capped at this value per AU, so it stays below [`TRIGGER_SCORE`] and an
    /// IDR can never raise an alarm on its own. `0.0` disables it.
    pub i_frame_weight: f32,
    /// A `ts_ms` jump longer than this is a stall/reconnect, not a slow frame:
    /// the score is cleared and the frame is not learned. `0` derives it from
    /// the stream's fps (3 intervals, at least 500 ms).
    pub gap_ms: u64,
}

impl Default for MotionConfig {
    fn default() -> Self {
        MotionConfig {
            window: 30,
            min_samples: 8,
            ratio_threshold: 1.75,
            min_delta_bytes: 1500,
            score_half_life_ms: 2000,
            hold_ms: 5000,
            sustain_ms: 60_000,
            settle_frames_after_idr: 1,
            i_frame_weight: 0.25,
            gap_ms: 0,
        }
    }
}

/// What the recorder gets back after every observed access unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    /// Level: motion/changed scene is being seen, or was within `hold_ms`.
    pub active: bool,
    /// Edge: `active` went false → true on this observation. Start a clip here.
    pub just_started: bool,
    /// Evidence score (0 = nothing, ≥1 = trigger). For logs and tuning.
    pub score: f32,
    /// Timestamp of the last trigger, 0 if never.
    pub last_active_ms: u64,
}

/// Rolling window of AU sizes, fixed capacity, no allocation.
#[derive(Debug, Clone)]
struct Ring {
    sizes: [u32; MAX_WINDOW],
    len: usize,
    next: usize,
    cap: usize,
}

impl Ring {
    fn new(cap: usize) -> Self {
        let cap = cap.clamp(1, MAX_WINDOW);
        Ring { sizes: [0; MAX_WINDOW], len: 0, next: 0, cap }
    }

    fn capacity(&self) -> usize {
        self.cap
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, v: u32) {
        self.sizes[self.next] = v;
        self.next += 1;
        if self.next == self.cap {
            self.next = 0;
        }
        if self.len < self.cap {
            self.len += 1;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
        self.next = 0;
    }

    /// Median of what we have; `None` when empty. Integer maths, sorted copy.
    /// Even counts take the mean of the two middles (rounded down).
    fn median(&self) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let mut tmp = self.sizes;
        let s = &mut tmp[..self.len];
        s.sort_unstable();
        let mid = s.len() / 2;
        Some(if s.len() % 2 == 0 { s[mid - 1] + (s[mid] - s[mid - 1]) / 2 } else { s[mid] })
    }
}

/// The detector. One instance per stream, owned by whichever thread reads the
/// stream (no interior mutability, no atomics, nothing shared).
#[derive(Debug, Clone)]
pub struct Detector {
    cfg: MotionConfig,
    info: Arc<StreamInfo>,
    /// Median window of non-keyframe AU sizes.
    p: Ring,
    /// Recent oversized sizes, used to re-seed the baseline on a scene change.
    evidence: Ring,
    /// Previous I-frame size, for the (capped) I-frame channel.
    i_prev: Option<u32>,
    /// P-frames still to ignore after an IDR.
    settle: u8,
    score: f32,
    last_ts: Option<u64>,
    last_trigger: Option<u64>,
    /// When the current run of ≥trigger evidence began.
    sustain_start: Option<u64>,
    /// Non-empty AUs seen (diagnostics; also gates gap detection on frame 1).
    frames: u64,
}

impl Detector {
    /// Build with the defaults in [`MotionConfig`]. Only the fps is read from
    /// the stream, to size the "this is a stall, not a frame" gap.
    pub fn new(info: Arc<StreamInfo>) -> Self {
        Self::with_config(info, MotionConfig::default())
    }

    /// Build with explicit tunables (tests, and per-stream profiles).
    pub fn with_config(info: Arc<StreamInfo>, mut cfg: MotionConfig) -> Self {
        // Never panic on a nonsense config: clamp instead, and document it.
        cfg.window = cfg.window.clamp(2, MAX_WINDOW);
        cfg.min_samples = cfg.min_samples.clamp(1, cfg.window);
        cfg.ratio_threshold = cfg.ratio_threshold.clamp(1.05, 16.0);
        cfg.score_half_life_ms = cfg.score_half_life_ms.max(1);
        cfg.hold_ms = cfg.hold_ms.max(1);
        cfg.i_frame_weight = cfg.i_frame_weight.clamp(0.0, 0.5);
        if cfg.gap_ms == 0 {
            // 3 AU intervals, never below 500 ms: a slow frame is not a stall.
            let fps = if info.fps > 0 { info.fps } else { 15 };
            cfg.gap_ms = (3000 / fps as u64).max(500);
        }
        let p = Ring::new(cfg.window);
        let evidence = Ring::new(MAX_EVIDENCE_HISTORY.min(cfg.window));
        Detector {
            cfg,
            info,
            p,
            evidence,
            i_prev: None,
            settle: 0,
            score: 0.0,
            last_ts: None,
            last_trigger: None,
            sustain_start: None,
            frames: 0,
        }
    }

    pub fn config(&self) -> &MotionConfig {
        &self.cfg
    }

    /// The P-frame baseline in bytes, once there is one. Print this when tuning.
    pub fn baseline_bytes(&self) -> Option<u32> {
        self.p.median()
    }

    /// How many P-frames the baseline is currently built from.
    pub fn baseline_samples(&self) -> usize {
        self.p.len()
    }

    /// Non-empty access units observed so far.
    pub fn frames_seen(&self) -> u64 {
        self.frames
    }

    /// Convenience for the recorder: feed a [`VideoFrame`] straight through.
    pub fn observe_frame(&mut self, frame: &VideoFrame) -> Motion {
        self.observe(frame.ts_ms, frame.len(), frame.keyframe)
    }

    /// Feed one access unit. `bytes` is the AU's size (`VideoFrame::len()`),
    /// `keyframe` its IDR flag, `ts_ms` a monotonic millisecond clock (the same
    /// one the frames carry; wall time does not matter).
    pub fn observe(&mut self, ts_ms: u64, bytes: usize, keyframe: bool) -> Motion {
        let size = u32::try_from(bytes).unwrap_or(u32::MAX);

        // An empty AU is not a frame: nothing to learn, nothing to break.
        if size == 0 {
            return self.output(ts_ms, self.is_active(ts_ms));
        }

        let was_active = self.is_active(ts_ms);

        // --- timeline: decay and discontinuities -------------------------
        let dt = match self.last_ts {
            Some(prev) if ts_ms >= prev => ts_ms - prev,
            _ => 0,
        };
        let clock_went_back = matches!(self.last_ts, Some(prev) if ts_ms < prev);
        self.last_ts = Some(ts_ms);

        let mut stalled = clock_went_back;
        if self.frames > 0 && dt > self.cfg.gap_ms {
            stalled = true;
        }
        self.frames += 1;

        if stalled {
            // A stall/reconnect: the sizes on either side are not comparable,
            // and whatever we had accumulated is stale. The held state stays
            // (the recorder's clip is already running), the new frame is not
            // learned in either direction.
            self.score = 0.0;
            self.sustain_start = None;
            self.settle = 0;
            return self.output(ts_ms, was_active);
        }
        self.score *= decay(dt, self.cfg.score_half_life_ms);

        // --- evidence ----------------------------------------------------
        if keyframe {
            // The I-frame channel, normalised against the previous I-frame.
            // Capped at `i_frame_weight` per AU (default 0.25 < trigger), so
            // periodic IDRs can never raise the alarm by themselves.
            if self.cfg.i_frame_weight > 0.0 {
                if let Some(prev) = self.i_prev {
                    if prev > 0 && size.saturating_sub(prev) >= self.cfg.min_delta_bytes {
                        let ratio = size as f32 / prev as f32;
                        let excess = excess_ratio(ratio, self.cfg.ratio_threshold);
                        if excess > 0.0 {
                            self.score += (self.cfg.i_frame_weight * excess).min(self.cfg.i_frame_weight);
                        }
                    }
                }
            }
            self.i_prev = Some(size);
            self.settle = self.cfg.settle_frames_after_idr;
        } else if self.settle > 0 {
            self.settle -= 1;
        } else {
            match self.p.median() {
                Some(base) if self.p.len() >= self.cfg.min_samples && base > 0 => {
                    let ratio = size as f32 / base as f32;
                    let delta = size.saturating_sub(base);
                    if ratio >= self.cfg.ratio_threshold && delta >= self.cfg.min_delta_bytes {
                        // Evidence. Deliberately NOT fed to the baseline: a
                        // scene in motion must not teach the detector that
                        // motion is normal.
                        let excess = excess_ratio(ratio, self.cfg.ratio_threshold);
                        self.score += excess.clamp(MIN_CONTRIBUTION, MAX_CONTRIBUTION);
                        self.evidence.push(size);
                    } else {
                        self.p.push(size);
                    }
                }
                // Not enough baseline yet: gather, judge nothing.
                _ => self.p.push(size),
            }
        }

        if self.score >= TRIGGER_SCORE {
            self.score = self.score.min(MAX_SCORE);
            let started = *self.sustain_start.get_or_insert(ts_ms);
            if self.cfg.sustain_ms > 0 && ts_ms.saturating_sub(started) >= self.cfg.sustain_ms {
                // Changed for a whole minute: this is the new normal.
                self.adopt_scene_change();
                return self.output(ts_ms, was_active);
            }
            self.last_trigger = Some(ts_ms);
        } else {
            self.sustain_start = None;
        }

        self.output(ts_ms, was_active)
    }

    /// Drop the evidence history and rebuild the baseline from it, so a
    /// permanent scene change stops alarming instead of alarming forever.
    fn adopt_scene_change(&mut self) {
        if let Some(level) = self.evidence.median() {
            self.p.clear();
            for _ in 0..self.cfg.min_samples {
                self.p.push(level);
            }
        }
        self.evidence.clear();
        self.score = 0.0;
        self.sustain_start = None;
    }

    fn is_active(&self, ts_ms: u64) -> bool {
        match self.last_trigger {
            Some(t) => ts_ms.saturating_sub(t) < self.cfg.hold_ms,
            None => false,
        }
    }

    fn output(&self, ts_ms: u64, was_active: bool) -> Motion {
        let active = self.is_active(ts_ms);
        Motion {
            active,
            just_started: active && !was_active,
            score: self.score,
            last_active_ms: self.last_trigger.unwrap_or(0),
        }
    }
}

/// `(ratio - threshold) / threshold`, floored at 0: 0 at the threshold, 1.0 at
/// twice the threshold.
fn excess_ratio(ratio: f32, threshold: f32) -> f32 {
    if !(ratio.is_finite()) || ratio <= threshold {
        return 0.0;
    }
    ((ratio - threshold) / threshold).min(MAX_CONTRIBUTION)
}

/// `0.5^(dt / half_life)`, clamped so a huge `dt` cannot produce a NaN/inf.
fn decay(dt_ms: u64, half_life_ms: u64) -> f32 {
    if dt_ms == 0 || half_life_ms == 0 {
        return 1.0;
    }
    let x = (dt_ms as f32 / half_life_ms as f32).min(64.0);
    (-x * std::f32::consts::LN_2).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream like the real `/vs0` unless a test says otherwise.
    fn stream_info(fps: u32) -> Arc<StreamInfo> {
        Arc::new(StreamInfo {
            name: "vs0".to_string(),
            sps: vec![0x67, 0x42, 0x00, 0x1f],
            pps: vec![0x68, 0xce, 0x3c, 0x80],
            width: 1280,
            height: 720,
            fps,
            audio: None,
        })
    }

    fn detector(fps: u32) -> Detector {
        Detector::new(stream_info(fps))
    }

    /// Step `ts` by one AU interval and feed a frame.
    fn frame(d: &mut Detector, ts: &mut u64, fps: u32, bytes: usize, keyframe: bool) -> Motion {
        *ts += 1000 / fps.max(1) as u64;
        d.observe(*ts, bytes, keyframe)
    }

    /// Deterministic ±5 % jitter, because a real encoder never produces the
    /// same size twice and a perfectly flat stream would let a broken median
    /// pass the tests.
    fn jitter(base: usize, i: usize) -> usize {
        let pct = (i * 37 % 11) as isize - 5; // -5..=5
        (base as isize + base as isize * pct / 100) as usize
    }

    /// A quiet stream: P-frames around `p`, an IDR every `gop` frames.
    fn quiet_run(d: &mut Detector, ts: &mut u64, fps: u32, p: usize, i: usize, n: usize, gop: usize) -> Vec<Motion> {
        (0..n)
            .map(|k| {
                let kf = gop > 0 && k % gop == 0;
                frame(d, ts, fps, if kf { i } else { jitter(p, k) }, kf)
            })
            .collect()
    }

    #[test]
    fn static_stream_never_fires() {
        let mut d = detector(15);
        let mut ts = 0u64;
        let mut fired = 0;
        let mut ever_active = false;
        for m in quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 600, 47) {
            fired += m.just_started as usize;
            ever_active |= m.active;
            assert!(m.score < TRIGGER_SCORE, "quiet stream must not accumulate: {}", m.score);
        }
        assert_eq!(fired, 0, "static scene produced {} just_started edges", fired);
        assert!(!ever_active);
    }

    #[test]
    fn oversized_burst_fires_once_on_the_first_frame() {
        let mut d = detector(15);
        let mut ts = 0u64;
        quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 90, 47); // ~6 s of normal footage
        let base = d.baseline_bytes().expect("baseline after 90 frames");
        assert!(base > 7_000 && base < 9_000, "baseline {base} should track the P-frames");

        // 5× the baseline: one frame of this is already well past the trigger.
        let first = frame(&mut d, &mut ts, 15, base as usize * 5, false);
        assert!(first.just_started, "rising edge on the first oversized frame");
        assert!(first.active);
        assert_eq!(first.last_active_ms, ts);
        assert!(first.score >= TRIGGER_SCORE);

        // The rest of the burst is the same event.
        for _ in 0..10 {
            let m = frame(&mut d, &mut ts, 15, base as usize * 5, false);
            assert!(m.active);
            assert!(!m.just_started, "no second edge inside one event");
        }
    }

    #[test]
    fn sustained_mild_excess_needs_several_frames() {
        // 2x the baseline is above threshold but not a cut, so
        // it must take a few frames to add up — one odd frame is not an event.
        let mut d = detector(15);
        let mut ts = 0u64;
        quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 90, 47);
        let base = d.baseline_bytes().unwrap() as usize;
        let mut triggered_at = None;
        for k in 0..12 {
            let m = frame(&mut d, &mut ts, 15, base * 2, false);
            if m.just_started && triggered_at.is_none() {
                triggered_at = Some(k);
            }
            assert!(!m.just_started || k > 0);
        }
        let k = triggered_at.expect("a sustained 2x stream must eventually trigger");
        assert!(k >= 3 && k <= 10, "took {k} frames to accumulate");
    }

    #[test]
    fn rapid_retrigger_has_no_second_edge() {
        let mut d = detector(15);
        let mut ts = 0u64;
        quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 90, 47);
        let base = d.baseline_bytes().unwrap() as usize;

        let edge = frame(&mut d, &mut ts, 15, base * 5, false);
        assert!(edge.just_started);

        // Quiet for less than the hold, then a second, independent burst.
        for k in 0..20 {
            let m = frame(&mut d, &mut ts, 15, jitter(base, k), false);
            assert!(!m.just_started);
        }
        let second = frame(&mut d, &mut ts, 15, base * 5, false);
        assert!(second.active);
        assert!(!second.just_started, "still inside the hold: one clip, not two");

        // Past the hold, the level drops, and the next burst is a new event.
        for k in 0..200 {
            let m = frame(&mut d, &mut ts, 15, jitter(base, k), false);
            assert!(!m.just_started);
        }
        let third = frame(&mut d, &mut ts, 15, base * 5, false);
        assert!(third.just_started, "a new event after the hold expired gets a new edge");
    }

    #[test]
    fn hold_expiry_drops_active() {
        let mut d = detector(15);
        let mut ts = 0u64;
        quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 90, 47);
        let base = d.baseline_bytes().unwrap() as usize;
        let hold = d.config().hold_ms;

        let burst = frame(&mut d, &mut ts, 15, base * 5, false);
        assert!(burst.active && burst.just_started);
        let event_start = burst.last_active_ms;

        // Quiet frames only: nothing here may trigger again. The level may last
        // the hold after the *last* time the score sat above the trigger (the
        // score decays over ~2 s, so it keeps refreshing for a moment), which is
        // why `ts - last_active_ms` — not `ts - burst` — is the number that must
        // stay under `hold`.
        let mut quiet = 0;
        let mut dropped = None;
        while quiet < 400 {
            let m = frame(&mut d, &mut ts, 15, jitter(base, 3), false);
            quiet += 1;
            assert!(!m.just_started, "a quiet frame raised an edge at +{} ms", ts - event_start);
            if m.active {
                assert!(
                    ts - m.last_active_ms < hold,
                    "active outlived the {} ms hold ({} ms since the last trigger)",
                    hold,
                    ts - m.last_active_ms
                );
            } else {
                assert!(ts - m.last_active_ms >= hold, "dropped before the hold expired");
                dropped = Some(ts);
                break;
            }
        }
        let dropped = dropped.expect("the level must fall once the evidence decays");
        assert!(
            dropped - event_start > hold,
            "the clip must run at least the hold ({} ms)",
            dropped - event_start
        );
        // Once it has dropped it stays dropped while the stream is quiet.
        for k in 0..100 {
            let m = frame(&mut d, &mut ts, 15, jitter(base, k), false);
            assert!(!m.active && !m.just_started);
        }
    }

    #[test]
    fn night_shift_is_adopted_not_latched() {
        // Day: 8.2 kB P-frames at 15 fps. Night with IR on: ~2.4× that (more
        // than the measured 1.65× shift, so it *is* evidence) at 10 fps, and the
        // ISP switch forces an IDR at the boundary.
        let mut d = detector(15);
        let mut ts = 0u64;
        let day = quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 450, 47); // 30 s
        assert!(day.iter().all(|m| !m.active));

        let night_p = 19_700;
        let mut edges_after_change = 0;
        let mut m = frame(&mut d, &mut ts, 10, 60_000, true); // forced IDR
        assert!(!m.active, "an IDR alone must never raise the alarm");
        for k in 0..1200 {
            // 120 s of night at 10 fps.
            let kf = k % 45 == 0;
            m = frame(&mut d, &mut ts, 10, if kf { 60_000 } else { jitter(night_p, k) }, kf);
            if k > 900 && m.just_started {
                edges_after_change += 1;
            }
        }
        assert_eq!(edges_after_change, 0, "night must not keep alarming after adoption");
        assert!(!m.active, "detector latched active through the night shift");
        let base = d.baseline_bytes().expect("baseline survives");
        assert!(
            (base as i64 - night_p as i64).abs() < night_p as i64 / 4,
            "baseline {base} should have followed the night level (~{night_p})"
        );
    }

    #[test]
    fn slow_lighting_shift_never_fires() {
        // The measured day→night bitrate step is 1.65×; that must be invisible.
        let mut d = detector(15);
        let mut ts = 0u64;
        let mut all = quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 300, 47);
        all.extend(quiet_run(&mut d, &mut ts, 10, 13_600, 44_000, 600, 45));
        assert!(all.iter().all(|m| !m.active), "a 1.65x bitrate step is not motion");
    }

    #[test]
    fn keyframes_alone_are_not_evidence_but_one_huge_idr_is_capped() {
        // GOP of 2 s at 15 fps, quiet P-frames, one IDR 10× the previous IDR.
        let mut d = detector(15);
        let mut ts = 0u64;
        quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 120, 30);
        let mut m = frame(&mut d, &mut ts, 15, 300_000, true);
        assert!(!m.active, "one huge IDR contributed {} to the score", m.score);
        assert!(m.score < TRIGGER_SCORE);
        // And the frames after it are not evidence either.
        for k in 0..10 {
            m = frame(&mut d, &mut ts, 15, jitter(8_200, k), false);
            assert!(!m.active);
        }
    }

    #[test]
    fn malformed_and_degenerate_inputs_do_not_panic() {
        // fps 0, zero dimensions: nothing may divide by zero.
        let odd = Arc::new(StreamInfo {
            name: String::new(),
            sps: Vec::new(),
            pps: Vec::new(),
            width: 0,
            height: 0,
            fps: 0,
            audio: None,
        });
        let mut d = Detector::with_config(odd, MotionConfig::default());
        assert_eq!(d.baseline_bytes(), None);
        // Empty stream, then one frame: no baseline, no alarm.
        let m = d.observe(0, 0, false);
        assert!(!m.active && !m.just_started);
        let m = d.observe(0, 12_000, true);
        assert!(!m.active);
        let m = d.observe(0, 12_000, false);
        assert!(!m.active);
        // Absurd size, backwards clock, multi-hour gap, then normal traffic.
        let m = d.observe(1, usize::MAX, false);
        assert!(!m.active);
        let mut ts = 1u64;
        for k in 0..40 {
            let r = frame(&mut d, &mut ts, 15, jitter(8_000, k), false);
            assert!(r.score.is_finite());
        }
        ts = 0; // clock jumps backwards (NTP sets the board's time)
        let m = d.observe(ts, 8_000, false);
        assert!(!m.just_started);
        ts += 6 * 60 * 60 * 1000; // six-hour gap
        let m = d.observe(ts, 8_000, false);
        assert!(m.score.is_finite());
        assert!(!m.just_started);
        // A config full of nonsense is clamped, not fatal.
        let weird = MotionConfig {
            window: 0,
            min_samples: 0,
            ratio_threshold: f32::NAN,
            min_delta_bytes: 0,
            score_half_life_ms: 0,
            hold_ms: 0,
            sustain_ms: u64::MAX,
            settle_frames_after_idr: 255,
            i_frame_weight: 9.0,
            gap_ms: 0,
        };
        let mut d = Detector::with_config(stream_info(15), weird);
        let mut ts = 0u64;
        for k in 0..50 {
            let r = frame(&mut d, &mut ts, 15, 8_000 * (k % 3 + 1), k % 7 == 0);
            assert!(r.score.is_finite());
        }
    }

    #[test]
    fn stall_clears_the_score_but_keeps_the_held_clip() {
        let mut d = detector(15);
        let mut ts = 0u64;
        quiet_run(&mut d, &mut ts, 15, 8_200, 30_000, 90, 47);
        let base = d.baseline_bytes().unwrap() as usize;
        assert!(frame(&mut d, &mut ts, 15, base * 5, false).active);
        // A 2-second stall, well past `gap_ms` but inside the hold: the score
        // must not bank the catch-up frame, and the running clip continues.
        ts += 2_000;
        let m = d.observe(ts, base * 8, false);
        assert!(m.score < TRIGGER_SCORE, "a stall must not bank a trigger");
        // Still inside the hold: the clip that was already running continues.
        assert!(m.active && !m.just_started);
        // After the hold, and past the gap, quiet traffic settles back to rest.
        for k in 0..200 {
            let m = frame(&mut d, &mut ts, 15, jitter(base, k), false);
            assert!(!m.just_started);
        }
        assert!(!d.is_active(ts));
    }
}
