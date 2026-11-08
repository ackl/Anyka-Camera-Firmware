//! Batch uploader — motion segments off the card, over the private link.
//!
//! The cameras are deployed offsite behind an LTE hotspot with no operator and
//! no console. Recordings pile up on the SD card (see `rec.rs`); this module is
//! the only thing that gets them home. The rules that follow from "offsite, no
//! operator, flaky uplink":
//!
//! * **The card is the buffer and the local copy is the only copy** until the
//!   receiver has proven it holds the bytes. So nothing is deleted on a "200
//!   OK" alone: the receiver must echo the sha256 it computed over what it
//!   *stored*, and only an exact match against the local file releases the
//!   delete. A mismatch keeps the file and is reported, every pass, forever.
//!   Losing footage to a silent corruption is worse than keeping it.
//! * **Resume is per file, not per batch.** A 60 s segment is ~1 MB; an LTE
//!   link that drops every few minutes would otherwise re-send whole segments
//!   from zero and never finish a backlog. Chunks are 256 KiB and the receiver
//!   is the authority on how much it already holds — its number overrides
//!   anything this module remembers.
//! * **Bounded politeness.** One pass moves at most `max_bytes_per_run` bytes
//!   and sleeps `pause_ms` between chunks, so the uplink is never saturated
//!   for longer than a pass.
//! * **Never panics, never blocks forever.** Every connect, read and write has
//!   a timeout; every request is retried a bounded number of times; the first
//!   file-level network failure ends the pass instead of grinding through the
//!   queue against a dead link. A caller can log one line per pass and go back
//!   to streaming.
//! * **Pure std, no crates.** There is no TLS stack for this board and
//!   crates.io is not reachable from the build box, so the transport is plain
//!   HTTP over a WireGuard tunnel on the site router. SHA-256 is implemented
//!   below (it is a hundred lines) rather than pulled in.
//!
//! # Wire protocol (v1) — "cam-ingest"
//!
//! The receiver is any HTTP/1.1 server that implements the five handlers
//! below. **The homelab side has to implement exactly this**; nothing else is
//! sent. All bodies are opaque bytes. Verdicts live in **headers**: a JSON body
//! is allowed and ignored (a body is read only when the response carries
//! `Content-Length` ≤ 8 KiB, so a chunked reply also works).
//!
//! Base URL: `http://HOST[:PORT]/PREFIX`, where `NAME` below is the segment's
//! file name (`20261025-181500.h264`, no directory part) and every URL is
//! `PREFIX/NAME…`.
//!
//! Every request carries:
//!
//! ```text
//! Host: HOST[:PORT]
//! User-Agent: camrtsp/<version>          (set via UpConfig::user_agent)
//! Connection: close                       (one request per connection)
//! X-Cam-Token: <token>                    (omitted when the token is empty)
//! ```
//!
//! ## 1. Is it already there?
//!
//! ```text
//! HEAD PREFIX/NAME
//!   200 OK   X-Stored-Sha256: <64 hex>   X-Stored-Bytes: <n>
//!   404 Not Found
//! ```
//!
//! Asked once per file per pass, before anything is sent. If the echoed sha256
//! equals the local file's sha256 the transfer is skipped entirely and the
//! file is deleted — this is how a **lost finalize reply** (the object landed,
//! the answer did not) is recovered without re-sending a whole segment. A
//! receiver that does not implement this handler may answer 404/405 and the
//! camera will re-upload; that is correct but wasteful, so implement it.
//!
//! ## 2. How much do you already hold?
//!
//! ```text
//! HEAD PREFIX/NAME.part
//!   200 OK   X-Upload-Offset: <bytes stored so far>
//!   404 Not Found                     (no partial object: offset 0)
//! ```
//!
//! The receiver's offset wins over the camera's `upload.state`. A 200 without
//! `X-Upload-Offset` (or `X-Stored-Bytes`) is a protocol error, not an excuse
//! to start from zero. This request is skipped when the state file already
//! knows a non-zero offset: a wrong offset only costs one wasted chunk, because
//! the PUT that follows is rejected with 409 and the true offset.
//!
//! ## 3. Upload a chunk — repeated until offset == total
//!
//! ```text
//! PUT PREFIX/NAME.part
//!   Content-Length: <n>
//!   Content-Range: bytes <first>-<last>/<total>     (inclusive, HTTP style)
//!   Content-SHA256: <64 hex sha256 of this chunk's body>
//!   <body: n bytes, the file's bytes [first, last]>
//!
//!   200 OK              X-Upload-Offset: <first + n>
//!   201 / 204           treated exactly like 200
//!   409 Conflict        X-Upload-Offset: <offset the receiver actually has>
//!                       (the camera adopts it and continues — no truncation,
//!                       no restart, whether the receiver is ahead or behind)
//!   416                 same as 409
//!   400 (chunk sha)     retried as a whole chunk
//!   5xx                 retried with backoff, then the file is left for the
//!                       next pass
//!   4xx (anything else) the file is failed for this pass and kept
//! ```
//!
//! `n` is `chunk_bytes` except for the last chunk of a file and when
//! `max_bytes_per_run` clamps it. A receiver must accept chunks at the offset
//! it reported and must keep whatever prefix it has already written (a dropped
//! connection leaves a shorter `.part`, which is fine and expected).
//!
//! ## 4. Commit
//!
//! ```text
//! POST PREFIX/NAME.finalize
//!   Content-Length: 0
//!   X-Upload-Total: <total bytes>
//!   X-Upload-Sha256: <64 hex sha256 of the whole file>
//!
//!   200 OK   X-Stored-Sha256: <64 hex, computed over the bytes it stored>
//! ```
//!
//! The receiver hashes the object **it stored**, not the header, and answers
//! with that. Make it idempotent: same name, same total, same sha → 200 again
//! (the camera may legitimately repeat it after losing a reply). On success the
//! receiver should move `.part` to the final name so handler 1 answers from
//! then on. The camera deletes locally only when this header matches, so a
//! missing header is a hard failure, never a pass.
//!
//! ## 5. The sidecar
//!
//! ```text
//! PUT PREFIX/NAME.json
//!   Content-Length: <n>
//!   Content-SHA256: <64 hex>
//!   <body: the sidecar's bytes, unchanged>
//!
//!   200 OK   X-Stored-Sha256: <64 hex>
//! ```
//!
//! Small enough to send whole; idempotent by name, so a lost reply just means
//! it is sent again next pass (handler 1 already knows the video is there).
//! The sidecar is uploaded **after** the video commits and the local pair is
//! deleted only after both shas matched: metadata is half the value of the
//! footage.
//!
//! ## Optional: reset
//!
//! ```text
//! DELETE PREFIX/NAME.part  →  200 (or 404)
//! ```
//!
//! Sent only when the receiver claims an offset **beyond** the local file's
//! length (a stale `.part` from a file that was replaced: impossible with
//! `create_new` names, but the camera must not silently upload a truncated
//! object). Without this handler the camera fails that file and keeps it.
//!
//! # What is NOT protected, because there is no TLS
//!
//! Plain HTTP over a WireGuard tunnel is the v1 assumption. Everything below is
//! true **if the tunnel is the only path to the receiver**, which is why the
//! receiver must not be reachable from anywhere else:
//!
//! * **The token travels in cleartext.** Anyone who can see the path can read
//!   `X-Cam-Token` and replay it; knowing it is the whole authorisation model.
//!   There is no per-request signature (no HMAC), no nonce, no expiry.
//! * **The footage is cleartext**, as are file names and timestamps — an
//!   observer on the path sees the video. The tunnel is what hides it.
//! * **The server is not authenticated.** Base URLs are followed blindly: no
//!   certificate, no pinning, no host-key-like check. Whoever answers at
//!   `base_url` is trusted — including 3xx, which are *not* followed (a
//!   deliberate refusal: a redirect could point off-tunnel; a 3xx is a
//!   protocol failure here).
//! * **The sha256 check is an integrity check, not an authenticity check.** It
//!   catches corruption in transit, in the receiver's memory, and on the
//!   receiver's disk. A *malicious* peer that can rewrite the stream can also
//!   echo the right sha and make the camera delete its only copy. The delete
//!   decision rests entirely on an unauthenticated peer's word.
//! * **No forward secrecy**, no replay protection on the data itself: an
//!   attacker who can inject a `200` with a matching sha gets the inference
//!   "uploaded" and the file deleted.
//! * **DNS has no timeout** in `std`. Offsite with no resolver, use a literal
//!   address (`http://10.9.0.2:8088/ingest`); a hung resolver is the one way
//!   this module can still block for a while.
//!
//! # The local state file
//!
//! `<dir>/upload.state`, plain text, one line per segment, written by
//! temp-file-plus-rename so a power cut cannot leave half a line where a whole
//! one was:
//!
//! ```text
//! camrtsp-upload 1
//! 20261025-181500.h264 1048576 <64 hex sha256> <offset> <video:0|1> <sidecar:0|1> <attempts>
//! ```
//!
//! It is a **hint**, not the source of truth: it saves re-hashing a file and
//! one round trip per pass. The receiver's offset always wins, so a truncated,
//! empty, garbage or stale state file costs at most one wasted chunk. Lines
//! that do not parse (7 fields, sha of 64 lowercase hex, offset ≤ bytes) are
//! dropped silently; a missing state file is normal on a fresh card.
//!
//! # Handling a `.h264` with no sidecar
//!
//! `rec::Recorder` writes the sidecar only when the segment closes, so a
//! missing (or truncated) `.json` means one of two things, and the file name
//! tells them apart — it is the UTC stamp of the segment's first access unit:
//!
//! * **younger than `orphan_after_ms`** (default 600 s): it might still be the
//!   recorder's open file, so it is not provably finished. Skip, do not touch,
//!   count as `skipped_young` and look again next pass. This is the "leave
//!   anything younger than a minute alone" rule with a 10× margin over the 60 s
//!   maximum segment length — one threshold, not two, because both cases skip.
//! * **older than `orphan_after_ms`**: the write died — power cut, card yank.
//!   Upload the video
//!   anyway (footage is precious) and put an *inferred* sidecar beside it on
//!   the receiver, with `"sidecar": "inferred"`, `"reason": "orphan"`,
//!   `"cut_short": true`, `start_unix_ms` from the file name and
//!   `duration_ms: 0`. Then delete as usual once the video verified.
//! A local `*.part`, `*.tmp`, `*.error` or zero-byte `*.h264` is never touched:
//! `.part` is a *remote* name in this protocol, so anything by that name on the
//! card belongs to somebody else (or to a half-copied file) and is left alone.
//!
//! # Not wired yet
//!
//! `main.rs` does not `mod upload;` yet — the CLI wiring is deliberately
//! somebody else's commit. The file is self-contained (no `crate::` imports) so
//! it can be compiled and tested on its own, which is also how the tests below
//! run:
//!
//! ```text
//! rustc --test --edition 2021 src/upload.rs -o target/upload-tests
//! ./target/upload-tests
//! ```
//!
//! When it is wired in, pass `format!("camrtsp/{}", media::VERSION)` as
//! `UpConfig::user_agent` and drop the local default.

#![allow(dead_code)] // wired into main.rs later; see "Not wired yet" above.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---- naming and defaults --------------------------------------------------

/// Directory under the card root that holds the segments (`rec::SUBDIR`).
pub const REC_SUBDIR: &str = "rec";
/// Progress file, relative to the card root.
pub const STATE_FILE: &str = "upload.state";
const STATE_MAGIC: &str = "camrtsp-upload 1";

/// A response head bigger than this is not ours (or is a broken server).
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// We never need more than a few bytes of a reply body; all verdicts are in
/// headers. Keeps a chatty receiver from making us wait or allocate.
const MAX_BODY_BYTES: usize = 8 * 1024;
/// How many times one file may be pushed back to a different offset by 409/416
/// before we call it a protocol failure. Two would do; four tolerates a
/// receiver with a lagging commit.
const MAX_OFFSET_RESTARTS: u32 = 4;
/// Segments named before this are assumed to come from a camera whose clock was
/// never set (no RTC, no NTP yet), so their age is unknown and `keep_days` must
/// not expire them.
const PLAUSIBLE_EPOCH_MS: u64 = 1_577_836_800_000; // 2020-01-01T00:00:00Z
/// A stamp this far in the future is a clock jump, not a real segment.
const FUTURE_SLACK_MS: u64 = 86_400_000;

/// What the receiver must prove before we delete the local copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verify {
    /// The receiver echoes the sha256 it stored; only an exact match releases
    /// the delete. The default, and the only mode that deletes anything.
    Echo,
    /// Accept a bare `200` from finalize and **never delete**. Exists for a
    /// debug sink that cannot hash; the camera keeps every file, so the card
    /// fills up and `rec`'s quota evicts the oldest. Say so in the log.
    None,
}

impl Verify {
    pub fn as_str(self) -> &'static str {
        match self {
            Verify::Echo => "echo",
            Verify::None => "none",
        }
    }
}

/// Tunables. `Default` is what a camera in the field should use; the tests
/// shrink everything to milliseconds.
#[derive(Clone, Debug)]
pub struct UpConfig {
    /// `https://HOST[:PORT]/PREFIX` (the field case) or `http://…` for a LAN
    /// receiver. No redirects (see the module doc).
    pub base_url: String,
    /// Sent as `X-Cam-Token` on every request. Empty means "send no header",
    /// which is only sane on a wire you physically trust.
    pub token: String,
    /// Bytes this pass may put on the wire, chunks plus sidecars. One 60 s
    /// segment is ~1 MB; 8 MiB/pass every five minutes is ~0.2 Mbit/s of a
    /// typical LTE uplink, leaving room for the camera to keep streaming.
    pub max_bytes_per_run: u64,
    /// Do not bother uploading footage older than this many days (0 = no
    /// limit). Expired segments are **skipped, never deleted** — `rec`'s byte
    /// quota is what reclaims the card. Protects the uplink from a week-old
    /// backlog after an outage.
    pub keep_days: u32,
    pub verify: Verify,
    /// Chunk size. 256 KiB is ~2 s at 1 Mbit/s: a retry is worth it on LTE, and
    /// a lost chunk does not restart a segment.
    pub chunk_bytes: usize,
    /// Slept between chunks, so a pass is never a saturating blast.
    pub pause_ms: u64,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
    /// Attempts per request (1 = try once).
    pub retries: u32,
    pub retry_backoff_ms: u64,
    /// A sidecar-less `.h264` older than this is a power-cut orphan and is
    /// uploaded with inferred metadata. 10× the 60 s max segment length.
    pub orphan_after_ms: u64,
    /// After this many *non-network* failures, stop trying this file and report
    /// it as stuck (never delete it). Network failures do not count: an outage
    /// must not quarantine the queue.
    pub max_attempts_per_file: u32,
    pub user_agent: String,
}

impl Default for UpConfig {
    fn default() -> Self {
        UpConfig {
            base_url: String::new(),
            token: String::new(),
            max_bytes_per_run: 8 << 20,
            keep_days: 14,
            verify: Verify::Echo,
            chunk_bytes: 256 << 10,
            pause_ms: 200,
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(30),
            retries: 3,
            retry_backoff_ms: 500,
            orphan_after_ms: 600_000,
            max_attempts_per_file: 50,
            user_agent: "camrtsp/0.1.0".to_string(),
        }
    }
}

// ---- errors and the report ------------------------------------------------

/// The only failures that are not per-file: a missing card or a config mistake.
/// Everything about an individual segment goes into [`UploadReport::failed`],
/// because one bad file must not stop the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpError {
    /// `<dir>` does not exist or cannot be listed: the card is unplugged, or
    /// the mount is gone. Log it once, keep streaming, try again next pass.
    NoCard(String),
    /// `base_url` (or the user agent/token) is unusable.
    Config(String),
}

impl fmt::Display for UpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpError::NoCard(m) => write!(f, "card: {m}"),
            UpError::Config(m) => write!(f, "config: {m}"),
        }
    }
}

impl std::error::Error for UpError {}

/// Why one file did not upload. `Capped` and `Stuck` are bookkeeping, not
/// errors in themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// Connect/read/write failed with every retry spent: the link is down.
    Network,
    /// The receiver answered, but not 2xx after retries.
    Http,
    /// The receiver answered something this protocol does not allow.
    Protocol,
    /// The receiver's sha256 differs from ours. The file is kept.
    Sha256Mismatch,
    /// The card: file vanished, shrank, or could not be read.
    Local,
    /// `max_bytes_per_run` reached; the pass ended cleanly.
    Capped,
    /// `max_attempts_per_file` non-network failures: give up on this one.
    Stuck,
}

impl FailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailKind::Network => "network",
            FailKind::Http => "http",
            FailKind::Protocol => "protocol",
            FailKind::Sha256Mismatch => "sha256-mismatch",
            FailKind::Local => "local",
            FailKind::Capped => "capped",
            FailKind::Stuck => "stuck",
        }
    }
    /// Would the same thing have happened without this file being odd?
    fn worth_counting(self) -> bool {
        !matches!(self, FailKind::Network | FailKind::Capped)
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub name: String,
    pub kind: FailKind,
    pub message: String,
}

impl Failure {
    fn new(name: &str, kind: FailKind, message: impl Into<String>) -> Failure {
        Failure {
            name: name.to_string(),
            kind,
            message: message.into(),
        }
    }
}

/// One verified, deleted segment.
#[derive(Clone, Debug)]
pub struct Uploaded {
    pub name: String,
    pub bytes: u64,
    /// Lowercase hex of the whole file, as echoed by the receiver.
    pub sha256: String,
    /// Where the transfer picked up: `bytes_sent` is `bytes - resumed_from`
    /// plus anything a retry had to send twice.
    pub resumed_from: u64,
    pub elapsed_ms: u64,
    /// The `.h264` had no usable sidecar and the receiver got inferred
    /// metadata instead.
    pub orphan: bool,
    /// The receiver already held the file (a lost finalize reply last time).
    pub already_there: bool,
}

/// The result of one `upload_pending` call. Log one line; do not act on
/// individual fields.
#[derive(Clone, Debug, Default)]
pub struct UploadReport {
    /// Segments that were candidates at the start of the pass.
    pub considered: usize,
    pub uploaded: Vec<Uploaded>,
    /// Local pairs removed because the receiver proved it had them.
    pub deleted: usize,
    /// Body bytes actually written to the socket this pass.
    pub bytes_sent: u64,
    /// Files whose transfer started above zero.
    pub resumed: usize,
    /// Sidecar-less `.h264` too young to touch.
    pub skipped_young: usize,
    /// Sidecar-less `.h264` too old to be in progress (uploaded with inferred
    /// metadata).
    pub orphans: usize,
    /// Older than `keep_days`: skipped, kept.
    pub expired: usize,
    /// Empty files, ignored.
    pub empty: usize,
    pub failed: Vec<Failure>,
    /// The pass stopped because of `max_bytes_per_run`.
    pub capped: bool,
    /// The pass stopped after a file-level network failure: the link is down.
    pub aborted: bool,
    /// The state file was rewritten (false: nothing changed, or the card would
    /// not take it).
    pub state_saved: bool,
}

impl UploadReport {
    /// One line for the camera's log.
    pub fn summary(&self) -> String {
        format!(
            "upload: {} considered, {} uploaded ({} B, {} resumed), {} deleted, \
             {} skipped-young, {} orphan, {} expired, {} failed{}",
            self.considered,
            self.uploaded.len(),
            self.bytes_sent,
            self.resumed,
            self.deleted,
            self.skipped_young,
            self.orphans,
            self.expired,
            self.failed.len(),
            if self.aborted {
                ", link down"
            } else if self.capped {
                ", capped"
            } else {
                ""
            },
        )
    }
}

// ---- SHA-256 --------------------------------------------------------------
//
// Written out here because the board has no TLS stack, no cargo registry, and
// the only hash we need is this one. FIPS 180-4, straightforward
// implementation; the tests pin the standard vectors and the padding edges.

mod sha256 {
    const H0: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    const K: [u32; 64] = [
        0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4,
        0xab1c_5ed5, 0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe,
        0x9bdc_06a7, 0xc19b_f174, 0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f,
        0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da, 0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7,
        0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967, 0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc,
        0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85, 0xa2bf_e8a1, 0xa81a_664b,
        0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070, 0x19a4_c116,
        0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
        0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7,
        0xc671_78f2,
    ];

    /// Incremental SHA-256. `update` any number of times, then `finish`.
    pub struct Sha256 {
        h: [u32; 8],
        buf: [u8; 64],
        buf_len: usize,
        total: u64,
    }

    impl Sha256 {
        pub fn new() -> Sha256 {
            Sha256 {
                h: H0,
                buf: [0; 64],
                buf_len: 0,
                total: 0,
            }
        }

        pub fn update(&mut self, mut data: &[u8]) {
            self.total = self.total.wrapping_add(data.len() as u64);
            if self.buf_len > 0 {
                let take = (64 - self.buf_len).min(data.len());
                let (a, b) = (self.buf_len, self.buf_len + take);
                self.buf[a..b].copy_from_slice(&data[..take]);
                self.buf_len += take;
                data = &data[take..];
                if self.buf_len == 64 {
                    let block = self.buf;
                    self.compress(&block);
                    self.buf_len = 0;
                }
            }
            while data.len() >= 64 {
                let mut block = [0u8; 64];
                block.copy_from_slice(&data[..64]);
                self.compress(&block);
                data = &data[64..];
            }
            if !data.is_empty() {
                self.buf[..data.len()].copy_from_slice(data);
                self.buf_len = data.len();
            }
        }

        pub fn finish(mut self) -> [u8; 32] {
            let bits = self.total.wrapping_mul(8);
            // 0x80, then zeros, then the 64-bit big-endian bit length.
            let mut tail = [0u8; 128];
            tail[0] = 0x80;
            let pad = if self.buf_len < 56 {
                56 - self.buf_len
            } else {
                120 - self.buf_len
            };
            tail[pad..pad + 8].copy_from_slice(&bits.to_be_bytes());
            let n = pad + 8;
            // `update` would add to `total`, which no longer matters.
            let mut rest = &tail[..n];
            while !rest.is_empty() {
                let take = (64 - self.buf_len).min(rest.len());
                let (a, b) = (self.buf_len, self.buf_len + take);
                self.buf[a..b].copy_from_slice(&rest[..take]);
                self.buf_len += take;
                rest = &rest[take..];
                if self.buf_len == 64 {
                    let block = self.buf;
                    self.compress(&block);
                    self.buf_len = 0;
                }
            }
            let mut out = [0u8; 32];
            for (i, w) in self.h.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
            }
            out
        }

        fn compress(&mut self, block: &[u8; 64]) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    block[i * 4],
                    block[i * 4 + 1],
                    block[i * 4 + 2],
                    block[i * 4 + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let mut v = self.h;
            for i in 0..64 {
                let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
                let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
                let t1 = v[7]
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
                let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                let t2 = s0.wrapping_add(maj);
                v[7] = v[6];
                v[6] = v[5];
                v[5] = v[4];
                v[4] = v[3].wrapping_add(t1);
                v[3] = v[2];
                v[2] = v[1];
                v[1] = v[0];
                v[0] = t1.wrapping_add(t2);
            }
            for i in 0..8 {
                self.h[i] = self.h[i].wrapping_add(v[i]);
            }
        }
    }

    /// Lowercase hex, the only representation this protocol uses.
    pub fn hex_digest(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        let d = h.finish();
        let mut s = String::with_capacity(64);
        for b in d {
            s.push(hex_nibble(b >> 4));
            s.push(hex_nibble(b & 0xf));
        }
        s
    }

    pub fn hex_nibble(v: u8) -> char {
        match v {
            0..=9 => (b'0' + v) as char,
            _ => (b'a' + (v - 10)) as char,
        }
    }

    pub fn is_hex_sha256(s: &str) -> bool {
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

// ---- small helpers --------------------------------------------------------

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_interrupted(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Interrupted
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `YYYYMMDD-HHMMSS` at the front of a segment file name, as unix ms.
fn parse_stamp(name: &str) -> Option<u64> {
    let b = name.as_bytes();
    if b.len() < 15 || b[8] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let mut v = 0i64;
        for &c in &b[r] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    let (y, mo, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (hh, mi, ss) = (num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || hh > 23 || mi > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    let secs = days * 86_400 + hh * 3600 + mi * 60 + ss;
    Some((secs as u64).saturating_mul(1000))
}

/// Howard Hinnant's `days_from_civil`, the inverse of `rec::days_to_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Age in ms, or `None` when the stamp cannot be trusted (bogus camera clock).
fn age_ms(now: u64, stamp: u64) -> Option<u64> {
    if stamp < PLAUSIBLE_EPOCH_MS || stamp > now.saturating_add(FUTURE_SLACK_MS) {
        return None;
    }
    Some(now.saturating_sub(stamp))
}

/// Percent-encode anything that is not unreserved. Segment names never need it;
/// it is here so a name from an older recorder cannot inject a request line.
fn url_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if ok {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(sha256::hex_nibble(b >> 4).to_ascii_uppercase());
            out.push(sha256::hex_nibble(b & 0xf).to_ascii_uppercase());
        }
    }
    out
}

// ---- HTTP ----------------------------------------------------------------

/// A parsed `http://…` base URL. Everything we need to speak to the receiver.
#[derive(Clone, Debug)]
struct Endpoint {
    host: String,
    port: u16,
    /// `https://` — wrap the socket in TLS (crate::tls) before speaking HTTP.
    tls: bool,
    /// No trailing slash; empty when the URL had no path.
    prefix: String,
    token: String,
    user_agent: String,
    connect_timeout: Duration,
    io_timeout: Duration,
    retries: u32,
    backoff: Duration,
}

impl Endpoint {
    fn from_config(cfg: &UpConfig) -> Result<Endpoint, String> {
        let url = cfg.base_url.trim();
        if url.is_empty() {
            return Err("base_url is empty".to_string());
        }
        // https:// is what the field configuration uses: the only public path to
        // the house is Cloudflare's edge, and it does not speak plain HTTP to a
        // camera. http:// stays supported for a LAN receiver or a tunnel.
        let (rest, tls) = match url.strip_prefix("http://") {
            Some(r) => (r, false),
            None => match url.strip_prefix("https://") {
                Some(r) => (r, true),
                None => {
                    return Err(format!("base_url must start with http:// or https:// (got {url})"))
                }
            },
        };
        let default_port = if tls { 443 } else { 80 };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(format!("base_url has no host: {url}"));
        }
        let (host, port) = if let Some(close) = authority.strip_prefix('[').and_then(|r| r.find(']'))
        {
            // [::1] or [::1]:8080
            let host = &authority[1..close + 1];
            let after = &authority[close + 2..];
            let port = match after.strip_prefix(':') {
                Some(p) => p
                    .parse::<u16>()
                    .map_err(|_| format!("base_url port is not a number: {url}"))?,
                None => default_port,
            };
            (host.to_string(), port)
        } else {
            match authority.rfind(':') {
                Some(i) => (
                    authority[..i].to_string(),
                    authority[i + 1..]
                        .parse::<u16>()
                        .map_err(|_| format!("base_url port is not a number: {url}"))?,
                ),
                None => (authority.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return Err(format!("base_url has no host: {url}"));
        }
        let prefix = path.trim_end_matches('/').to_string();
        if prefix.contains('?') || prefix.contains('#') {
            return Err(format!("base_url must not carry a query or fragment: {url}"));
        }
        Ok(Endpoint {
            host,
            port,
            tls,
            prefix,
            token: cfg.token.clone(),
            user_agent: cfg.user_agent.clone(),
            connect_timeout: cfg.connect_timeout,
            io_timeout: cfg.io_timeout,
            retries: cfg.retries.max(1),
            backoff: Duration::from_millis(cfg.retry_backoff_ms),
        })
    }

    fn target(&self, suffix: &str) -> String {
        format!("{}/{}", self.prefix, suffix)
    }

    fn user_agent_or_default(&self) -> &str {
        if self.user_agent.is_empty() {
            "camrtsp/0.1.0"
        } else {
            &self.user_agent
        }
    }

    /// One request, one connection, `Connection: close`. Retries transport
    /// errors and 5xx; returns the last response whatever its code, so the
    /// caller decides what a 4xx means.
    fn call(
        &self,
        method: &str,
        target: &str,
        extra: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Resp, io::Error> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let r = self.once(method, target, extra, body);
            let retryable = match &r {
                Ok(resp) => resp.code >= 500,
                Err(_) => true,
            };
            if !retryable || attempt >= self.retries {
                return r;
            }
            std::thread::sleep(self.backoff * attempt);
        }
    }

    fn once(
        &self,
        method: &str,
        target: &str,
        extra: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Resp, io::Error> {
        let mut sock = None;
        let mut last: Option<io::Error> = None;
        let addrs = (self.host.as_str(), self.port).to_socket_addrs()?;
        for a in addrs {
            match TcpStream::connect_timeout(&a, self.connect_timeout) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        // Not `mut`: the timeouts take &self, and `s` is shadowed by the boxed
        // stream below.
        let s = match sock {
            Some(s) => s,
            None => {
                return Err(last.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "address resolved to nothing")
                }))
            }
        };
        s.set_read_timeout(Some(self.io_timeout))?;
        s.set_write_timeout(Some(self.io_timeout))?;
        let _ = s.set_nodelay(true);

        // TLS (if any) is established before the first byte of HTTP, and a failed
        // handshake is a transport error: call() retries it like a refused
        // connect, which is what a flaky LTE link usually looks like.
        let mut s: Box<dyn Stream> = if self.tls {
            Box::new(crate::tls::TlsStream::connect(&self.host, s)?)
        } else {
            Box::new(s)
        };

        let mut head = String::with_capacity(256);
        head.push_str(&format!("{method} {target} HTTP/1.1\r\n"));
        head.push_str(&format!("Host: {}:{}\r\n", self.host, self.port));
        head.push_str(&format!("User-Agent: {}\r\n", self.user_agent_or_default()));
        head.push_str("Connection: close\r\n");
        if !self.token.is_empty() {
            head.push_str(&format!("X-Cam-Token: {}\r\n", self.token));
        }
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        let body_len = body.map(|b| b.len()).unwrap_or(0);
        if body.is_some() || method == "POST" || method == "PUT" {
            head.push_str(&format!("Content-Length: {body_len}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes())?;
        if let Some(b) = body {
            s.write_all(b)?;
        }
        let _ = s.flush();

        read_response(&mut s)
    }
}

#[derive(Clone, Debug)]
struct Resp {
    code: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    }

    fn header_u64(&self, name: &str) -> Option<u64> {
        self.header(name).and_then(|v| v.parse::<u64>().ok())
    }

    fn ok(&self) -> bool {
        (200..300).contains(&self.code)
    }

    fn described(&self) -> String {
        let mut s = format!("HTTP {} {}", self.code, self.reason);
        if let Some(e) = self.header("X-Error") {
            s.push_str(&format!(" ({e})"));
        }
        s
    }
}

/// Anything HTTP can be read from: a TcpStream, or a TlsStream on top of one.
trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

fn read_response(s: &mut dyn Read) -> io::Result<Resp> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let head_end = loop {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "receiver closed before sending a status line",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response head too large",
            ));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let mut parts = status.splitn(3, ' ');
    let proto = parts.next().unwrap_or("");
    let code = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad status line: {status:?}"),
            )
        })?;
    if !proto.starts_with("HTTP/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad status line: {status:?}"),
        ));
    }
    let reason = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(i) = line.find(':') {
            headers.push((line[..i].trim().to_string(), line[i + 1..].trim().to_string()));
        }
    }

    // All verdicts live in the headers; a body is read only if the receiver
    // says how long it is, and only up to our cap, so a chatty JSON reply
    // cannot make us wait for a socket that never closes.
    let mut body = Vec::new();
    if let Some(rest) = buf.get(head_end + 4..) {
        body.extend_from_slice(rest);
    }
    if let Some(len) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
    {
        let want = len.min(MAX_BODY_BYTES);
        while body.len() < want {
            let n = s.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
    }
    body.truncate(MAX_BODY_BYTES);
    Ok(Resp {
        code,
        reason,
        headers,
        body,
    })
}

// ---- the card: segments and their sidecars -------------------------------

/// One upload candidate, from the card's `rec/` directory.
#[derive(Clone, Debug)]
struct Candidate {
    /// File name with extension, e.g. `20261025-181500.h264`.
    name: String,
    path: PathBuf,
    bytes: u64,
    start_unix_ms: Option<u64>,
}

/// The bits of the sidecar this module uses. Everything else stays on the card
/// and is uploaded as-is.
#[derive(Clone, Debug)]
struct Sidecar {
    start_unix_ms: Option<u64>,
    bytes: Option<u64>,
    duration_ms: u64,
    cut_short: bool,
}

/// Pull `"key": <integer>` out of the hand-written sidecar JSON. Deliberately
/// not a JSON parser: the shape is fixed by `rec.rs`, and anything unexpected
/// must degrade to "no metadata", never to an error.
fn json_u64(text: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\"");
    let i = text.find(&pat)? + pat.len();
    let rest = &text[i..];
    let colon = rest.find(':')?;
    let tail = rest[colon + 1..].trim_start();
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    if end == 0 {
        return None;
    }
    tail[..end].parse::<u64>().ok()
}

fn json_bool(text: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\"");
    let i = text.find(&pat)? + pat.len();
    let rest = &text[i..];
    let colon = rest.find(':')?;
    let tail = rest[colon + 1..].trim_start();
    if tail.starts_with("true") {
        Some(true)
    } else if tail.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

impl Sidecar {
    /// `None` when the file is missing, unreadable, empty or truncated — a
    /// power cut mid-write leaves exactly that, and the caller treats it as
    /// "no metadata yet".
    fn read(path: &Path) -> Option<Sidecar> {
        let text = fs::read_to_string(path).ok()?;
        // The sidecar always has a "file" key first; if it is there but the
        // close brace is not, the write was torn.
        if !text.contains("\"file\"") || !text.trim_end().ends_with('}') {
            return None;
        }
        Some(Sidecar {
            start_unix_ms: json_u64(&text, "start_unix_ms"),
            bytes: json_u64(&text, "bytes"),
            duration_ms: json_u64(&text, "duration_ms").unwrap_or(0),
            cut_short: json_bool(&text, "cut_short").unwrap_or(false),
        })
    }
}

/// Metadata for a `.h264` whose sidecar never got written. Honest about it:
/// `sidecar: "inferred"` and `reason: "orphan"` so the homelab can tell these
/// from a normal close.
fn inferred_sidecar(name: &str, bytes: u64, start_unix_ms: u64) -> String {
    format!(
        "{{\n  \"file\": \"{name}\",\n  \"start_unix_ms\": {start_unix_ms},\n  \
         \"duration_ms\": 0,\n  \"bytes\": {bytes},\n  \"au_count\": 0,\n  \
         \"keyframe_count\": 0,\n  \"cut_short\": true,\n  \"reason\": \"orphan\",\n  \
         \"sidecar\": \"inferred\"\n}}\n"
    )
}

// ---- progress state ------------------------------------------------------

#[derive(Clone, Debug)]
struct FileState {
    bytes: u64,
    sha: String,
    offset: u64,
    video_done: bool,
    sidecar_done: bool,
    attempts: u32,
}

impl FileState {
    fn new(bytes: u64) -> FileState {
        FileState {
            bytes,
            sha: String::new(),
            offset: 0,
            video_done: false,
            sidecar_done: false,
            attempts: 0,
        }
    }
}

// ---- the uploader --------------------------------------------------------

/// One file's outcome inside a pass.
struct Transfer {
    resumed_from: u64,
    orphan: bool,
    already_there: bool,
}

/// Motion-segment uploader. Own the queue in the caller's thread: every method
/// takes `&mut self`, there are no locks and no background threads, so a
/// dropped connection can never leave a worker spinning.
pub struct Uploader {
    root: PathBuf,
    rec_dir: PathBuf,
    cfg: UpConfig,
    ep: Result<Endpoint, String>,
    state_path: PathBuf,
    state: HashMap<String, FileState>,
    /// Serialised state as loaded, so an unchanged pass writes nothing.
    state_text: String,
    queue: Vec<Candidate>,
    said_no_card: bool,
    said_state: bool,
}

impl Uploader {
    /// `dir` is the card root (`/mnt` on the camera); segments live in
    /// `<dir>/rec`. Never touches the network and never fails: a missing card
    /// or a broken `base_url` surfaces from [`Uploader::upload_pending`], where
    /// the caller can log it once and carry on streaming.
    pub fn new(dir: &Path, cfg: UpConfig) -> Uploader {
        let ep = Endpoint::from_config(&cfg);
        let rec_dir = dir.join(REC_SUBDIR);
        let state_path = dir.join(STATE_FILE);
        let mut u = Uploader {
            root: dir.to_path_buf(),
            rec_dir,
            cfg,
            ep,
            state_path,
            state: HashMap::new(),
            state_text: String::new(),
            queue: Vec::new(),
            said_no_card: false,
            said_state: false,
        };
        if let Ok(list) = u.scan() {
            u.queue = list;
        }
        u
    }

    /// Validate the configuration without touching the card or the network.
    /// `serve` calls this at startup so a typo in `--upload` fails immediately
    /// instead of at the first pass, hours later.
    pub fn check_endpoint(&self) -> Result<(), String> {
        self.ep.clone().map(|_| ())
    }

    /// How many segments the last scan found still waiting. Does not touch the
    /// disk or the network.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Names still waiting, oldest first.
    pub fn queue_names(&self) -> Vec<String> {
        self.queue.iter().map(|c| c.name.clone()).collect()
    }

    pub fn rec_dir(&self) -> &Path {
        &self.rec_dir
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Re-scan without uploading, so `queue_len` is fresh.
    pub fn refresh(&mut self) -> Result<usize, UpError> {
        self.queue = self.scan()?;
        Ok(self.queue.len())
    }

    /// One pass: oldest segment first, bounded by `max_bytes_per_run`, ending
    /// early on the first link failure. Returns `Err` only for card-level
    /// trouble (missing card, unreadable mount) and config mistakes; per-file
    /// problems are in `report.failed`.
    pub fn upload_pending(&mut self) -> Result<UploadReport, UpError> {
        let ep = self.ep.clone().map_err(UpError::Config)?;
        let cands = self.scan()?;
        self.load_state();

        let now = unix_now_ms();
        let mut rep = UploadReport::default();
        let mut sent = 0u64;
        let mut stop = false;

        for c in &cands {
            rep.considered += 1;
            if stop {
                continue;
            }
            if sent >= self.cfg.max_bytes_per_run {
                rep.capped = true;
                stop = true;
                continue;
            }
            if c.bytes == 0 {
                rep.empty += 1;
                continue;
            }

            // Age: from the sidecar when it exists, else from the file name,
            // which is the segment's first AU in UTC.
            let sc = Sidecar::read(&c.path.with_extension("json"));
            let stamp = sc
                .as_ref()
                .and_then(|s| s.start_unix_ms)
                .or(c.start_unix_ms);
            let age = stamp.and_then(|s| age_ms(now, s));

            if let (Some(age), Some(keep)) = (age, keep_days_ms(self.cfg.keep_days)) {
                if age > keep {
                    rep.expired += 1;
                    continue;
                }
            }
            // No sidecar: either the recorder's open segment or a power-cut
            // orphan, and only the age can tell them apart. Below
            // `orphan_after_ms` it is not provably closed, so leave it alone.
            let orphan = sc.is_none();
            if orphan {
                if let Some(a) = age {
                    if a < self.cfg.orphan_after_ms {
                        rep.skipped_young += 1;
                        continue;
                    }
                }
            }

            let mut st = self
                .state
                .get(&c.name)
                .cloned()
                .unwrap_or_else(|| FileState::new(c.bytes));
            if st.bytes != c.bytes {
                // The file changed under the state (or the name was reused):
                // forget everything we thought we knew.
                st = FileState::new(c.bytes);
            }
            if st.attempts >= self.cfg.max_attempts_per_file {
                rep.failed.push(Failure::new(
                    &c.name,
                    FailKind::Stuck,
                    format!("{} failed attempts, not retrying", st.attempts),
                ));
                continue;
            }

            let t0 = Instant::now();
            match self.transfer(&ep, c, sc.as_ref(), &mut st, &mut sent) {
                Ok(t) => {
                    st.offset = c.bytes;
                    st.video_done = true;
                    st.attempts = 0;
                    if orphan {
                        rep.orphans += 1;
                    }
                    if t.resumed_from > 0 {
                        rep.resumed += 1;
                    }
                    rep.uploaded.push(Uploaded {
                        name: c.name.clone(),
                        bytes: c.bytes,
                        sha256: st.sha.clone(),
                        resumed_from: t.resumed_from,
                        elapsed_ms: t0.elapsed().as_millis() as u64,
                        orphan: t.orphan,
                        already_there: t.already_there,
                    });
                    // Remember it even when we are about to delete, so a
                    // receiver that cannot be verified (Verify::None) does not
                    // get the same megabytes a second time.
                    self.state.insert(c.name.clone(), st.clone());

                    // Delete only on a proven echo.
                    let can_delete = self.cfg.verify == Verify::Echo;
                    if can_delete {
                        match remove_pair(&c.path) {
                            Ok(()) => {
                                rep.deleted += 1;
                                self.state.remove(&c.name);
                            }
                            Err(e) => rep.failed.push(Failure::new(
                                &c.name,
                                FailKind::Local,
                                format!("uploaded but cannot delete: {e}"),
                            )),
                        }
                    }
                }
                Err(f) if f.kind == FailKind::Capped => {
                    rep.capped = true;
                    stop = true;
                    self.state.insert(c.name.clone(), st);
                }
                Err(f) => {
                    if f.kind.worth_counting() {
                        st.attempts = st.attempts.saturating_add(1);
                    }
                    let link_down = f.kind == FailKind::Network;
                    self.state.insert(c.name.clone(), st);
                    rep.failed.push(f);
                    if link_down {
                        // Everything after this would fail the same way, slowly.
                        rep.aborted = true;
                        stop = true;
                    }
                }
            }
        }

        rep.bytes_sent = sent;

        // Prune state for names that are no longer on the card.
        let live: Vec<String> = cands.iter().map(|c| c.name.clone()).collect();
        self.state.retain(|k, _| live.iter().any(|n| n == k));
        rep.state_saved = self.save_state_if_changed();

        match self.scan() {
            Ok(list) => self.queue = list,
            Err(_) => {}
        }
        Ok(rep)
    }

    /// Everything for one segment: HEAD, resume, chunks, finalize, sidecar.
    fn transfer(
        &self,
        ep: &Endpoint,
        c: &Candidate,
        sc: Option<&Sidecar>,
        st: &mut FileState,
        sent: &mut u64,
    ) -> Result<Transfer, Failure> {
        let name = c.name.as_str();

        // The whole-file hash: from the state when the size matches, else one
        // read of the file (1 MB, a few ms on the card).
        if st.sha.is_empty() || !sha256::is_hex_sha256(&st.sha) {
            st.sha = sha256_file(&c.path)
                .map_err(|e| Failure::new(name, FailKind::Local, format!("hash: {e}")))?;
        }

        // 1. Already there? (A lost finalize reply costs us nothing here.)
        let mut already_there = false;
        match ep.call("HEAD", &ep.target(&url_escape(name)), &[], None) {
            Ok(r) if r.ok() => {
                if let Some(remote) = r.header("X-Stored-Sha256") {
                    if remote.eq_ignore_ascii_case(&st.sha) {
                        already_there = true;
                        st.video_done = true;
                    }
                }
            }
            // 404 (or anything the receiver prefers to 404) just means "not
            // there"; a 4xx we do recognise as an error (403 token, 401) is
            // worth reporting as http, not as a protocol violation.
            Ok(r) if r.code == 404 || r.code == 405 || r.code == 501 => {}
            Ok(r) if r.code >= 400 && r.code < 500 => {
                return Err(Failure::new(
                    name,
                    FailKind::Http,
                    format!("HEAD {name}: {}", r.described()),
                ))
            }
            Ok(r) => {
                return Err(Failure::new(
                    name,
                    FailKind::Protocol,
                    format!("HEAD {name}: {}", r.described()),
                ))
            }
            Err(e) => {
                return Err(Failure::new(
                    name,
                    FailKind::Network,
                    format!("HEAD {name}: {e}"),
                ))
            }
        }

        let mut resumed_from = 0u64;
        if !already_there {
            // 2. How much does the receiver hold? Its answer is the only truth
            //    about the offset; the state file only saves the round trip.
            let hint = if st.offset > 0 && st.offset < c.bytes && st.bytes == c.bytes {
                st.offset
            } else {
                0
            };
            let probe = if hint > 0 {
                hint
            } else {
                match ep.call("HEAD", &ep.target(&format!("{}.part", url_escape(name))), &[], None) {
                    Ok(r) if r.ok() => match r
                        .header_u64("X-Upload-Offset")
                        .or_else(|| r.header_u64("X-Stored-Bytes"))
                    {
                        Some(o) => o,
                        None => {
                            return Err(Failure::new(
                                name,
                                FailKind::Protocol,
                                format!(
                                    "HEAD {name}.part: 200 without X-Upload-Offset, \
                                     refusing to guess the offset"
                                ),
                            ))
                        }
                    },
                    Ok(r) if r.code == 404 => 0,
                    Ok(r) if r.code >= 400 && r.code < 500 => {
                        return Err(Failure::new(
                            name,
                            FailKind::Http,
                            format!("HEAD {name}.part: {}", r.described()),
                        ))
                    }
                    Ok(r) => {
                        return Err(Failure::new(
                            name,
                            FailKind::Protocol,
                            format!("HEAD {name}.part: {}", r.described()),
                        ))
                    }
                    Err(e) => {
                        return Err(Failure::new(
                            name,
                            FailKind::Network,
                            format!("HEAD {name}.part: {e}"),
                        ))
                    }
                }
            };

            let mut offset = probe;
            if offset > c.bytes {
                // The receiver holds bytes this file does not have. Never
                // upload a truncated object: ask for a reset, and give up on
                // this file if the receiver will not take one.
                let r = ep
                    .call("DELETE", &ep.target(&format!("{}.part", url_escape(name))), &[], None)
                    .map_err(|e| Failure::new(name, FailKind::Network, format!("DELETE .part: {e}")))?;
                if !r.ok() && r.code != 404 {
                    return Err(Failure::new(
                        name,
                        FailKind::Protocol,
                        format!("receiver has {offset} bytes for a {} byte file: {}", c.bytes, r.described()),
                    ));
                }
                offset = 0;
                st.offset = 0;
            }
            resumed_from = offset;
            st.offset = offset;

            // 3. Chunks, until the receiver has the whole file.
            let mut file = File::open(&c.path)
                .map_err(|e| Failure::new(name, FailKind::Local, format!("open: {e}")))?;
            let mut restarts = 0u32;
            while offset < c.bytes {
                let room = self.cfg.max_bytes_per_run.saturating_sub(*sent);
                if room == 0 {
                    return Err(Failure::new(name, FailKind::Capped, "pass budget spent"));
                }
                let want = self
                    .cfg
                    .chunk_bytes
                    .max(1)
                    .min((c.bytes - offset) as usize)
                    .min(room.min(usize::MAX as u64) as usize);
                let data = read_at(&mut file, offset, want).map_err(|e| {
                    Failure::new(name, FailKind::Local, format!("read at {offset}: {e}"))
                })?;
                if data.is_empty() {
                    return Err(Failure::new(
                        name,
                        FailKind::Local,
                        format!("file is shorter than its {} bytes (shrank mid-transfer?)", c.bytes),
                    ));
                }
                let last = offset + data.len() as u64 - 1;
                let range = format!("bytes {}-{}/{}", offset, last, c.bytes);
                let chunk_sha = sha256::hex_digest(&data);
                let r = ep
                    .call(
                        "PUT",
                        &ep.target(&format!("{}.part", url_escape(name))),
                        &[("Content-Range", range.as_str()), ("Content-SHA256", chunk_sha.as_str())],
                        Some(&data),
                    )
                    .map_err(|e| Failure::new(name, FailKind::Network, format!("PUT {range}: {e}")))?;

                if r.ok() {
                    let next = r
                        .header_u64("X-Upload-Offset")
                        .unwrap_or(offset + data.len() as u64);
                    *sent += data.len() as u64;
                    if next <= offset {
                        return Err(Failure::new(
                            name,
                            FailKind::Protocol,
                            format!("receiver accepted {} bytes but reported offset {next}", data.len()),
                        ));
                    }
                    offset = next.min(c.bytes);
                    st.offset = offset;
                    if self.cfg.pause_ms > 0 {
                        std::thread::sleep(Duration::from_millis(self.cfg.pause_ms));
                    }
                    continue;
                }
                if r.code == 409 || r.code == 416 {
                    match r.header_u64("X-Upload-Offset") {
                        Some(o) => {
                            restarts += 1;
                            *sent += data.len() as u64;
                            if o > c.bytes {
                                return Err(Failure::new(
                                    name,
                                    FailKind::Protocol,
                                    format!("receiver wants offset {o} for a {} byte file", c.bytes),
                                ));
                            }
                            if o == offset {
                                return Err(Failure::new(
                                    name,
                                    FailKind::Protocol,
                                    format!("receiver rejected offset {offset} and reported the same"),
                                ));
                            }
                            if restarts > MAX_OFFSET_RESTARTS {
                                return Err(Failure::new(
                                    name,
                                    FailKind::Protocol,
                                    format!("{restarts} offset restarts, giving up on this file"),
                                ));
                            }
                            offset = o;
                            st.offset = offset;
                            continue;
                        }
                        None => {
                            return Err(Failure::new(
                                name,
                                FailKind::Protocol,
                                format!("{} without X-Upload-Offset", r.described()),
                            ))
                        }
                    }
                }
                if r.code == 400 {
                    // The receiver says our chunk hash is wrong (it is not) or
                    // the range is unacceptable. One retry at most: `call`
                    // already retried 5xx, not 4xx.
                    *sent += data.len() as u64;
                    return Err(Failure::new(
                        name,
                        FailKind::Http,
                        format!("chunk {range}: {}", r.described()),
                    ));
                }
                *sent += data.len() as u64;
                return Err(Failure::new(
                    name,
                    FailKind::Http,
                    format!("chunk {range}: {}", r.described()),
                ));
            }

            // 4. Commit.
            let total = c.bytes.to_string();
            let sha = st.sha.clone();
            let r = ep
                .call(
                    "POST",
                    &ep.target(&format!("{}.finalize", url_escape(name))),
                    &[("X-Upload-Total", total.as_str()), ("X-Upload-Sha256", sha.as_str())],
                    None,
                )
                .map_err(|e| Failure::new(name, FailKind::Network, format!("finalize: {e}")))?;
            if !r.ok() {
                return Err(Failure::new(
                    name,
                    FailKind::Http,
                    format!("finalize: {}", r.described()),
                ));
            }
            if self.cfg.verify == Verify::Echo {
                match r.header("X-Stored-Sha256") {
                    Some(echo) if echo.eq_ignore_ascii_case(&st.sha) => {}
                    Some(echo) => {
                        return Err(Failure::new(
                            name,
                            FailKind::Sha256Mismatch,
                            format!("receiver stored {echo}, we sent {}", st.sha),
                        ))
                    }
                    None => {
                        return Err(Failure::new(
                            name,
                            FailKind::Protocol,
                            "finalize: 200 without X-Stored-Sha256; not deleting unverified footage"
                                .to_string(),
                        ))
                    }
                }
            }
        }

        // 5. The sidecar. Uploaded after the video, verified the same way.
        let side_bytes = match sc {
            Some(_) => match fs::read(c.path.with_extension("json")) {
                Ok(b) => b,
                Err(e) => {
                    return Err(Failure::new(
                        name,
                        FailKind::Local,
                        format!("sidecar: {e} (video is up; keeping both)"),
                    ))
                }
            },
            None => {
                let start = c.start_unix_ms.unwrap_or(0);
                let json_name = format!("{}.json", name.trim_end_matches(".h264"));
                inferred_sidecar(&json_name, c.bytes, start).into_bytes()
            }
        };
        let side_sha = sha256::hex_digest(&side_bytes);
        let json_target = format!(
            "{}.json",
            url_escape(name.trim_end_matches(".h264"))
        );
        let r = ep
            .call(
                "PUT",
                &ep.target(&json_target),
                &[("Content-SHA256", side_sha.as_str())],
                Some(&side_bytes),
            )
            .map_err(|e| Failure::new(name, FailKind::Network, format!("PUT sidecar: {e}")))?;
        if !r.ok() {
            return Err(Failure::new(
                name,
                FailKind::Http,
                format!("PUT sidecar: {}", r.described()),
            ));
        }
        if self.cfg.verify == Verify::Echo {
            match r.header("X-Stored-Sha256") {
                Some(echo) if echo.eq_ignore_ascii_case(&side_sha) => {}
                Some(echo) => {
                    return Err(Failure::new(
                        name,
                        FailKind::Sha256Mismatch,
                        format!("sidecar: receiver stored {echo}, we sent {side_sha}"),
                    ))
                }
                None => {
                    return Err(Failure::new(
                        name,
                        FailKind::Protocol,
                        "sidecar: 200 without X-Stored-Sha256; keeping both files".to_string(),
                    ))
                }
            }
        }
        st.sidecar_done = true;
        *sent += side_bytes.len() as u64;

        Ok(Transfer {
            resumed_from,
            orphan: sc.is_none(),
            already_there,
        })
    }

    // ---- card scanning ---------------------------------------------------

    fn scan(&self) -> Result<Vec<Candidate>, UpError> {
        if !self.root.is_dir() {
            return Err(UpError::NoCard(format!(
                "{} is not there (card unplugged?)",
                self.root.display()
            )));
        }
        let rd = match fs::read_dir(&self.rec_dir) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(UpError::NoCard(format!(
                    "cannot list {}: {e}",
                    self.rec_dir.display()
                )))
            }
        };
        let mut out = Vec::new();
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.ends_with(".h264") {
                // `.json` sidecars are chosen by name, `.part`/`.tmp`/`.error`
                // belong to somebody else.
                continue;
            }
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(Candidate {
                start_unix_ms: parse_stamp(&name),
                name,
                path,
                bytes,
            });
        }
        // Fixed-width stamps: lexicographic on the name is chronological. (The
        // `-2` collision suffix sorts before the bare name inside a second;
        // `rec` sorts the same way, and one second's order does not matter.)
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    // ---- state -----------------------------------------------------------

    fn load_state(&mut self) {
        self.state.clear();
        self.state_text = fs::read_to_string(&self.state_path).unwrap_or_default();
        for line in self.state_text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line == STATE_MAGIC {
                continue;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() != 7 {
                continue;
            }
            let (bytes, sha, offset, vdone, sdone, attempts) = (
                f[1].parse::<u64>().ok(),
                f[2],
                f[3].parse::<u64>().ok(),
                f[4].parse::<u8>().ok(),
                f[5].parse::<u8>().ok(),
                f[6].parse::<u32>().ok(),
            );
            let (Some(bytes), Some(offset), Some(vdone), Some(sdone), Some(attempts)) =
                (bytes, offset, vdone, sdone, attempts)
            else {
                continue;
            };
            if !sha256::is_hex_sha256(sha) || offset > bytes || vdone > 1 || sdone > 1 {
                continue;
            }
            if f[0].is_empty() || f[0].contains('/') {
                continue;
            }
            self.state.insert(
                f[0].to_string(),
                FileState {
                    bytes,
                    sha: sha.to_string(),
                    offset,
                    video_done: vdone == 1,
                    sidecar_done: sdone == 1,
                    attempts,
                },
            );
        }
    }

    fn render_state(&self) -> String {
        let mut names: Vec<&String> = self.state.keys().collect();
        names.sort();
        let mut s = String::with_capacity(64 + names.len() * 96);
        s.push_str(STATE_MAGIC);
        s.push('\n');
        for n in names {
            let f = &self.state[n];
            s.push_str(&format!(
                "{n} {} {} {} {} {} {}\n",
                f.bytes,
                f.sha,
                f.offset,
                f.video_done as u8,
                f.sidecar_done as u8,
                f.attempts
            ));
        }
        s
    }

    /// Write temp + rename, so a power cut cannot tear a line in half. Returns
    /// whether anything was written; a card that will not take it is logged
    /// once and otherwise ignored (state is a hint, not the truth).
    fn save_state_if_changed(&mut self) -> bool {
        let text = self.render_state();
        if text == self.state_text && self.state_path.exists() {
            return false;
        }
        let tmp = self.state_path.with_extension("state.tmp");
        let res = (|| -> io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &self.state_path)
        })();
        match res {
            Ok(()) => {
                self.state_text = text;
                true
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                if !self.said_state {
                    self.said_state = true;
                    eprintln!(
                        "camrtsp upload: cannot write {}: {e} (continuing without resume hints)",
                        self.state_path.display()
                    );
                }
                false
            }
        }
    }
}

fn keep_days_ms(days: u32) -> Option<u64> {
    if days == 0 {
        None
    } else {
        Some((days as u64).saturating_mul(86_400_000))
    }
}

/// Delete the `.h264` and its sidecar. Missing files are fine (someone else
/// may have cleaned up); the pair is what matters, and only the caller's
/// verified-upload check may call this.
fn remove_pair(h264: &Path) -> io::Result<()> {
    match fs::remove_file(h264) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match fs::remove_file(h264.with_extension("json")) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = sha256::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(e) if is_interrupted(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    let d = h.finish();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push(sha256::hex_nibble(b >> 4));
        s.push(sha256::hex_nibble(b & 0xf));
    }
    Ok(s)
}

fn read_at(f: &mut File, offset: u64, n: usize) -> io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(e) if is_interrupted(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(got);
    Ok(buf)
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    // ---- temp dirs -------------------------------------------------------

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    struct Tmp(PathBuf);

    impl Tmp {
        fn new() -> Tmp {
            let p = std::env::temp_dir().join(format!(
                "camrtsp-upload-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(p.join(REC_SUBDIR)).unwrap();
            Tmp(p)
        }
        fn rec(&self) -> PathBuf {
            self.0.join(REC_SUBDIR)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // ---- mock receiver ---------------------------------------------------

    /// The whole server side of the protocol, in memory. Every request is
    /// recorded so a test can assert what actually went over the wire.
    #[derive(Default)]
    struct MockState {
        parts: HashMap<String, Vec<u8>>,
        finals: HashMap<String, (u64, String)>,
        sidecars: HashMap<String, Vec<u8>>,
        /// `PUT target first-last/total`
        puts: Vec<String>,
        /// `HEAD target` / `POST target` etc, in order.
        verbs: Vec<String>,
        /// Echo a different sha from finalize (corruption simulation).
        wrong_sha: Option<String>,
        /// (start offset, bytes to read) then close the socket: a dropped LTE
        /// connection with a partial chunk on the receiver's disk.
        drop_after: Option<(u64, usize)>,
        dropped: bool,
        /// Only accept requests carrying this token.
        require_token: Option<String>,
        /// Pretend HEAD final knows nothing.
        hide_finals: bool,
    }

    struct Mock {
        addr: SocketAddr,
        st: Arc<Mutex<MockState>>,
        stop: Arc<AtomicBool>,
        h: Option<JoinHandle<()>>,
    }

    impl Mock {
        fn start() -> Mock {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = l.local_addr().unwrap();
            l.set_nonblocking(true).unwrap();
            let st = Arc::new(Mutex::new(MockState::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let (s2, stop2) = (st.clone(), stop.clone());
            let h = std::thread::spawn(move || loop {
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                match l.accept() {
                    Ok((mut s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                        let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
                        handle(&mut s, &s2);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            });
            Mock {
                addr,
                st,
                stop,
                h: Some(h),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/ingest/", self.addr)
        }

        fn with(&self, f: impl FnOnce(&mut MockState)) {
            f(&mut self.st.lock().unwrap());
        }

        fn count(&self, prefix: &str) -> usize {
            self.st
                .lock()
                .unwrap()
                .verbs
                .iter()
                .filter(|v| v.starts_with(prefix))
                .count()
        }

        /// `HEAD …part` requests: the offset probe we try to avoid.
        fn probes(&self) -> usize {
            self.st
                .lock()
                .unwrap()
                .verbs
                .iter()
                .filter(|v| v.starts_with("HEAD ") && v.contains(".part"))
                .count()
        }

        fn final_sha(&self, name: &str) -> Option<String> {
            self.st.lock().unwrap().finals.get(name).map(|(_, s)| s.clone())
        }

        fn stored(&self, name: &str) -> Option<Vec<u8>> {
            self.st.lock().unwrap().sidecars.get(name).cloned()
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.h.take() {
                let _ = h.join();
            }
        }
    }

    fn handle(s: &mut TcpStream, st: &Arc<Mutex<MockState>>) {
        let (head, mut leftover) = match read_head(s) {
            Some(x) => x,
            None => return,
        };
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or("");
        let mut it = first.split(' ');
        let method = it.next().unwrap_or("").to_string();
        let target = it.next().unwrap_or("").to_string();
        let mut headers: Vec<(String, String)> = Vec::new();
        for l in lines {
            if let Some(i) = l.find(':') {
                headers.push((
                    l[..i].trim().to_ascii_lowercase(),
                    l[i + 1..].trim().to_string(),
                ));
            }
        }
        let hdr = |n: &str| -> Option<String> {
            headers
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.clone())
        };
        let path = target.strip_prefix("/ingest/").unwrap_or(&target).to_string();
        let len: usize = hdr("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut g = st.lock().unwrap();
        g.verbs.push(format!("{method} {target}"));

        if let Some(tok) = &g.require_token {
            if hdr("x-cam-token").as_deref() != Some(tok.as_str()) {
                g.verbs.push("403".to_string());
                drop(g);
                respond(s, 403, "Forbidden", &[]);
                return;
            }
        }

        match method.as_str() {
            "HEAD" => {
                if let Some(name) = path.strip_suffix(".part") {
                    let n = g.parts.get(name).map(|v| v.len()).unwrap_or(0);
                    let found = g.parts.contains_key(name);
                    drop(g);
                    if found {
                        respond(
                            s,
                            200,
                            "OK",
                            &[("X-Upload-Offset", n.to_string().as_str())],
                        );
                    } else {
                        respond(s, 404, "Not Found", &[]);
                    }
                } else if g.hide_finals || !g.finals.contains_key(&path) {
                    drop(g);
                    respond(s, 404, "Not Found", &[]);
                } else {
                    let (bytes, sha) = g.finals[&path].clone();
                    drop(g);
                    respond(
                        s,
                        200,
                        "OK",
                        &[
                            ("X-Stored-Sha256", sha.as_str()),
                            ("X-Stored-Bytes", bytes.to_string().as_str()),
                        ],
                    );
                }
            }
            "DELETE" => {
                if let Some(name) = path.strip_suffix(".part") {
                    g.parts.remove(name);
                    drop(g);
                    respond(s, 200, "OK", &[]);
                } else {
                    drop(g);
                    respond(s, 404, "Not Found", &[]);
                }
            }
            "PUT" => {
                // Content-Range first: the drop hook needs the offset, and both
                // arrive in the head.
                let range = hdr("content-range").unwrap_or_default();
                let is_part = path.ends_with(".part");
                let parsed = if is_part {
                    match parse_range(&range) {
                        Some(x) => Some(x),
                        None => {
                            drop(g);
                            respond(s, 400, "Bad Request", &[("X-Error", "bad Content-Range")]);
                            return;
                        }
                    }
                } else {
                    None
                };
                let first = parsed.map(|(f, _)| f).unwrap_or(0);
                let total = parsed.map(|(_, t)| t).unwrap_or(0);
                let cut_at = match (g.drop_after, g.dropped) {
                    (Some((off, cut)), false) if off == first => Some(cut),
                    _ => None,
                };

                // Body: whatever came with the head first, then the socket.
                let mut body = std::mem::take(&mut leftover);
                body.truncate(len.min(body.len()));
                let mut budget = len - body.len();
                let mut dropped = false;
                while budget > 0 {
                    let mut tmp = [0u8; 4096];
                    let want = budget.min(tmp.len());
                    match s.read(&mut tmp[..want]) {
                        Ok(0) => break,
                        Ok(n) => {
                            body.extend_from_slice(&tmp[..n]);
                            budget -= n;
                            if let Some(cut) = cut_at {
                                if body.len() >= cut {
                                    dropped = true;
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }

                if is_part {
                    let name = path.trim_end_matches(".part").to_string();
                    let cur = g.parts.get(&name).map(|v| v.len()).unwrap_or(0) as u64;
                    if first != cur {
                        drop(g);
                        respond(
                            s,
                            409,
                            "Conflict",
                            &[("X-Upload-Offset", cur.to_string().as_str())],
                        );
                        return;
                    }
                    let v = g.parts.entry(name.clone()).or_default();
                    v.extend_from_slice(&body);
                    let next = v.len() as u64;
                    g.puts.push(format!(
                        "PUT {name} {}-{} of {total}{}",
                        first,
                        first + (body.len() as u64).saturating_sub(1),
                        if dropped { " (dropped)" } else { "" }
                    ));
                    if dropped {
                        g.dropped = true;
                        drop(g);
                        let _ = s.shutdown(std::net::Shutdown::Both);
                        return;
                    }
                    drop(g);
                    respond(
                        s,
                        200,
                        "OK",
                        &[("X-Upload-Offset", next.to_string().as_str())],
                    );
                } else {
                    let name = path.clone();
                    let sha = sha256::hex_digest(&body);
                    g.sidecars.insert(name.clone(), body);
                    let wrong = g.wrong_sha.clone();
                    g.puts.push(format!("PUT {name} sidecar {} B", len));
                    drop(g);
                    match wrong {
                        Some(w) => respond(s, 200, "OK", &[("X-Stored-Sha256", w.as_str())]),
                        None => respond(s, 200, "OK", &[("X-Stored-Sha256", sha.as_str())]),
                    }
                }
            }
            "POST" => {
                if let Some(name) = path.strip_suffix(".finalize") {
                    let total = hdr("x-upload-total")
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                    let sha = hdr("x-upload-sha256").unwrap_or_default();
                    let stored = g.parts.get(name).map(|v| sha256::hex_digest(v));
                    let echo = match g.wrong_sha.clone() {
                        Some(w) => w,
                        None => match stored {
                            // Echo what we actually stored (the protocol's point).
                            Some(s) => s,
                            None => sha.clone(),
                        },
                    };
                    g.finals.insert(name.to_string(), (total, echo.clone()));
                    g.parts.remove(name);
                    drop(g);
                    respond(s, 200, "OK", &[("X-Stored-Sha256", echo.as_str())]);
                } else {
                    drop(g);
                    respond(s, 404, "Not Found", &[]);
                }
            }
            _ => {
                drop(g);
                respond(s, 405, "Method Not Allowed", &[]);
            }
        }
    }

    fn read_head(s: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = s.read(&mut tmp).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..p]).into_owned();
                return Some((head, buf[p + 4..].to_vec()));
            }
            if buf.len() > 32 * 1024 {
                return None;
            }
        }
    }

    /// `bytes 1024-2047/3000` → (1024, 3000).
    fn parse_range(v: &str) -> Option<(u64, u64)> {
        let rest = v.trim().strip_prefix("bytes ")?;
        let (range, total) = rest.split_once('/')?;
        let (a, _b) = range.split_once('-')?;
        Some((a.parse().ok()?, total.parse().ok()?))
    }

    fn respond(s: &mut TcpStream, code: u16, reason: &str, extra: &[(&str, &str)]) {
        let mut head = format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n"
        );
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let _ = s.write_all(head.as_bytes());
        let _ = s.flush();
    }

    // ---- card fixtures ---------------------------------------------------

    /// Deterministic bytes, so a resumed transfer can be compared exactly.
    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) ^ 0x5a) as u8).collect()
    }

    /// Write a segment the way `rec` would: `name.h264` + `name.json`.
    fn write_segment(rec: &Path, stamp: &str, len: usize) -> String {
        let name = format!("{stamp}.h264");
        let body = payload(len);
        fs::write(rec.join(&name), &body).unwrap();
        let start = parse_stamp(&name).unwrap();
        let json = format!(
            "{{\n  \"file\": \"{name}\",\n  \"start_unix_ms\": {start},\n  \
             \"start_ts_ms\": 0,\n  \"trigger_unix_ms\": {start},\n  \"trigger_ts_ms\": 0,\n  \
             \"duration_ms\": 4000,\n  \"bytes\": {len},\n  \"au_count\": 60,\n  \
             \"keyframe_count\": 1,\n  \"width\": 1280,\n  \"height\": 720,\n  \
             \"cut_short\": false,\n  \"reason\": \"post_roll\",\n  \
             \"recorder\": \"camrtsp 0.1.0\"\n}}\n"
        );
        fs::write(rec.join(name.replace(".h264", ".json")), json).unwrap();
        name
    }

    /// A stamp `age_ms` in the past (UTC, second precision).
    fn stamp_ago(age_ms: u64) -> String {
        let now = unix_now_ms().saturating_sub(age_ms);
        let secs = (now / 1000) as i64;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // days_to_civil, the same table rec.rs uses (only for tests).
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
            rem / 3600,
            (rem / 60) % 60,
            rem % 60
        )
    }

    fn cfg_for(base_url: String) -> UpConfig {
        UpConfig {
            base_url,
            token: "s3cret".to_string(),
            chunk_bytes: 1024,
            pause_ms: 0,
            retries: 3,
            retry_backoff_ms: 1,
            connect_timeout: Duration::from_millis(500),
            io_timeout: Duration::from_secs(2),
            ..UpConfig::default()
        }
    }

    fn cfg(mock: &Mock) -> UpConfig {
        cfg_for(mock.url())
    }

    /// Every byte currently on the card under `rec/`: what a perfect pass must
    /// put on the wire, no more.
    fn dir_bytes(dir: &Path) -> u64 {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    fn names(u: &Uploader) -> Vec<String> {
        u.queue_names()
    }

    // ---- SHA-256 ---------------------------------------------------------

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256::hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256::hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256::hex_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256::hex_digest(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_incremental_matches_one_shot_at_every_padding_edge() {
        let data = payload(300);
        let one = sha256::hex_digest(&data);
        for split in [0usize, 1, 55, 56, 63, 64, 65, 119, 120, 128, 299, 300] {
            let mut h = sha256::Sha256::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            let d = h.finish();
            let mut got = String::new();
            for b in d {
                got.push(sha256::hex_nibble(b >> 4));
                got.push(sha256::hex_nibble(b & 0xf));
            }
            assert_eq!(got, one, "split at {split}");
        }
        // And three-way, out of order of the loop above.
        let mut h = sha256::Sha256::new();
        h.update(&data[..7]);
        h.update(&data[7..200]);
        h.update(&data[200..]);
        assert_ne!(sha256::hex_digest(&data[..0]), one);
        let d = h.finish();
        assert_eq!(d.len(), 32);
    }

    #[test]
    fn stamp_parsing_round_trips() {
        let s = "20261025-181500.h264";
        let ms = parse_stamp(s).unwrap();
        assert_eq!(ms % 1000, 0);
        assert_eq!(parse_stamp("garbage.h264"), None);
        assert_eq!(parse_stamp("20261325-181500.h264"), None); // month 13
        assert_eq!(parse_stamp("20261025x181500.h264"), None);
        assert_eq!(stamp_ago(0).len(), 15);
        let now = unix_now_ms();
        let parsed = parse_stamp(&stamp_ago(60_000)).unwrap() as i64;
        assert!((parsed - (now as i64 - 60_000)).abs() < 5_000, "{parsed} vs {now}");
    }

    // ---- the tests the task asks for -------------------------------------

    #[test]
    fn happy_path_uploads_and_deletes() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 3000);
        let b = write_segment(&card.rec(), &stamp_ago(60_000), 2000);

        let mut u = Uploader::new(&card.0, cfg(&mock));
        assert_eq!(u.queue_len(), 2);
        let expect_bytes = dir_bytes(&card.rec());
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.considered, 2);
        assert_eq!(rep.uploaded.len(), 2, "{:?}", rep.failed);
        assert_eq!(rep.deleted, 2);
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(!rep.capped && !rep.aborted);
        // Video plus both sidecars, nothing twice.
        assert_eq!(rep.bytes_sent, expect_bytes);

        // Both pairs are gone from the card, queue empty, state empty.
        assert!(!card.rec().join(&a).exists());
        assert!(!card.rec().join(a.replace(".h264", ".json")).exists());
        assert!(!card.rec().join(&b).exists());
        assert_eq!(u.queue_len(), 0);
        let state = fs::read_to_string(card.0.join(STATE_FILE)).unwrap();
        assert_eq!(state.trim(), STATE_MAGIC);

        // The receiver holds the exact bytes and the sidecar.
        let m = mock.st.lock().unwrap();
        assert_eq!(m.finals.len(), 2);
        assert_eq!(m.finals[&a].1, sha256::hex_digest(&payload(3000)));
        assert_eq!(m.finals[&b].0, 2000);
        assert!(m.parts.is_empty(), "parts left behind: {:?}", m.parts);
        let sc = &m.sidecars[&format!("{}.json", b.trim_end_matches(".h264"))];
        let text = String::from_utf8_lossy(sc);
        assert!(text.contains("\"reason\": \"post_roll\""), "{text}");
        assert!(text.contains(&format!("\"file\": \"{b}\"")), "{text}");
        drop(m);

        // Chunks were sent as contiguous, inclusive ranges from zero.
        let puts: Vec<String> = mock.st.lock().unwrap().puts.clone();
        let mut expect: std::collections::HashMap<&str, u64> = Default::default();
        let mut chunks = 0;
        for p in puts.iter() {
            let f: Vec<&str> = p.split_whitespace().collect();
            if f.len() != 5 || !f[1].ends_with(".h264") {
                continue; // a sidecar put, not a chunk
            }
            let (first, last) = f[2].split_once('-').unwrap();
            let total = if f[1] == a { 3000 } else { 2000 };
            let want = expect.entry(f[1]).or_insert(0);
            assert_eq!(first.parse::<u64>().unwrap(), *want, "{p}");
            assert_eq!(f[4].parse::<u64>().unwrap(), total, "{p}");
            *want = last.parse::<u64>().unwrap() + 1;
            chunks += 1;
        }
        assert_eq!(chunks, 3 + 2, "3 chunks for 3000 B, 2 for 2000 B: {:?}", puts);
        assert_eq!(expect[&a.as_str()], 3000);
        assert_eq!(expect[&b.as_str()], 2000);
    }

    #[test]
    fn receiver_down_keeps_files_and_aborts_the_pass() {
        let card = Tmp::new();
        // A port with nothing behind it.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = dead.local_addr().unwrap();
        drop(dead);

        let a = write_segment(&card.rec(), &stamp_ago(120_000), 3000);
        let b = write_segment(&card.rec(), &stamp_ago(60_000), 2000);
        let mut u = Uploader::new(&card.0, cfg_for(format!("http://{addr}/ingest/")));
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.uploaded.len(), 0);
        assert_eq!(rep.deleted, 0);
        assert!(rep.aborted, "a dead link must end the pass");
        assert_eq!(rep.failed.len(), 1, "one failure, not one per file");
        assert_eq!(rep.failed[0].kind, FailKind::Network);
        assert!(card.rec().join(&a).exists());
        assert!(card.rec().join(&b).exists());
        assert_eq!(u.queue_len(), 2);
    }

    #[test]
    fn sha_mismatch_keeps_files() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 3000);
        mock.with(|m| {
            m.wrong_sha = Some(
                "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            )
        });

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.uploaded.len(), 0);
        assert_eq!(rep.deleted, 0);
        assert_eq!(rep.failed.len(), 1);
        assert_eq!(rep.failed[0].kind, FailKind::Sha256Mismatch);
        assert!(card.rec().join(&a).exists(), "corrupted upload must keep the card copy");
        assert!(card.rec().join(a.replace(".h264", ".json")).exists());

        // The file is retried next pass, not quarantined as a network error.
        let rep2 = u.upload_pending().unwrap();
        assert_eq!(rep2.failed.len(), 1);
        assert_eq!(rep2.failed[0].kind, FailKind::Sha256Mismatch);
        assert!(card.rec().join(&a).exists());
    }

    #[test]
    fn resume_from_the_receivers_offset() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 5000);
        let expect_bytes = dir_bytes(&card.rec());
        // A previous pass died after 2048 bytes; the receiver kept them.
        mock.with(|m| {
            m.parts.insert(a.clone(), payload(5000)[..2048].to_vec());
        });

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.uploaded.len(), 1, "{:?}", rep.failed);
        assert_eq!(rep.resumed, 1);
        assert_eq!(rep.uploaded[0].resumed_from, 2048);
        assert_eq!(
            rep.bytes_sent,
            expect_bytes - 2048,
            "only the tail went over the wire"
        );
        assert_eq!(mock.final_sha(&a).unwrap(), sha256::hex_digest(&payload(5000)));
        assert!(!card.rec().join(&a).exists());
    }

    #[test]
    fn resume_after_a_dropped_connection() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 4096);
        // Drop the second chunk after 300 of its bytes: the receiver keeps
        // them, so the retry must adopt offset 1324 rather than restart.
        mock.with(|m| m.drop_after = Some((1024, 300)));

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.uploaded.len(), 1, "{:?}", rep.failed);
        assert_eq!(rep.deleted, 1);
        assert_eq!(mock.final_sha(&a).unwrap(), sha256::hex_digest(&payload(4096)));
        let m = mock.st.lock().unwrap();
        assert!(m.dropped);
        // Someone tried to PUT at 1024 after the receiver already had 1324;
        // the 409 that came back was adopted (no full restart from zero).
        assert!(m.puts.iter().any(|p| p.contains("(dropped)")), "{:?}", m.puts);
    }

    #[test]
    fn two_pass_cap_is_honoured_and_resumes_where_it_stopped() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(180_000), 3000);
        let b = write_segment(&card.rec(), &stamp_ago(120_000), 3000);

        let mut c = cfg(&mock);
        c.max_bytes_per_run = 1500;
        let mut u = Uploader::new(&card.0, c);
        let rep1 = u.upload_pending().unwrap();
        assert!(rep1.capped);
        assert_eq!(rep1.bytes_sent, 1500, "the cap is a hard cap, chunk-clamped");
        assert!(rep1.uploaded.is_empty());
        assert!(card.rec().join(&a).exists());
        assert!(card.rec().join(&b).exists());

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep2 = u.upload_pending().unwrap();
        assert!(rep2.uploaded.len() >= 1);
        assert_eq!(rep2.uploaded[0].name, a);
        assert_eq!(rep2.uploaded[0].resumed_from, 1500);
        assert_eq!(mock.final_sha(&a).unwrap(), sha256::hex_digest(&payload(3000)));
        assert!(!card.rec().join(&a).exists());
    }

    #[test]
    fn state_file_saves_a_round_trip_across_a_restart() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 3000);

        let mut c = cfg(&mock);
        c.max_bytes_per_run = 1024;
        let mut u = Uploader::new(&card.0, c);
        let rep1 = u.upload_pending().unwrap();
        assert!(rep1.capped);
        let state = fs::read_to_string(card.0.join(STATE_FILE)).unwrap();
        assert!(state.contains(&a), "state lists the file: {state}");
        assert!(state.contains(&sha256::hex_digest(&payload(3000))), "{state}");
        assert_eq!(mock.probes(), 1, "pass 1 asks the receiver for its offset");

        // A fresh process (reboot, new Uploader) picks up from the state: no
        // second probe, and the bytes already stored are not re-sent.
        let mut u2 = Uploader::new(&card.0, cfg(&mock));
        let rep2 = u2.upload_pending().unwrap();
        assert_eq!(rep2.uploaded.len(), 1, "{:?}", rep2.failed);
        assert_eq!(rep2.uploaded[0].resumed_from, 1024);
        assert_eq!(
            mock.probes(),
            1,
            "pass 2 trusts the state file: no second offset probe"
        );
        assert_eq!(mock.final_sha(&a).unwrap(), sha256::hex_digest(&payload(3000)));
    }

    #[test]
    fn a_garbage_state_file_is_ignored() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 3000);
        fs::write(
            card.0.join(STATE_FILE),
            b"\x00\xff not a state file\x00\n20261025-181500.h264 9 zz 1 0 0 0\n\
              20261025-181500.h264 3000 00 999999 0 0 0\nhalf a line",
        )
        .unwrap();

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();
        assert_eq!(rep.uploaded.len(), 1, "{:?}", rep.failed);
        assert_eq!(mock.final_sha(&a).unwrap(), sha256::hex_digest(&payload(3000)));
        assert!(!card.rec().join(&a).exists());
    }

    #[test]
    fn orphan_without_a_sidecar_is_uploaded_once_it_is_old_enough() {
        let card = Tmp::new();
        let mock = Mock::start();
        // 2 h old, no sidecar: a power cut between close and sidecar write.
        let old = format!("{}.h264", stamp_ago(2 * 3600_000));
        fs::write(card.rec().join(&old), payload(1234)).unwrap();
        // 5 s old, no sidecar: the recorder may still be writing it.
        let young = format!("{}.h264", stamp_ago(5_000));
        fs::write(card.rec().join(&young), payload(77)).unwrap();

        let mut u = Uploader::new(&card.0, cfg(&mock));
        assert_eq!(u.queue_len(), 2);
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.skipped_young, 1);
        assert_eq!(rep.orphans, 1);
        assert_eq!(rep.uploaded.len(), 1);
        assert_eq!(rep.uploaded[0].name, old);
        assert!(rep.uploaded[0].orphan);
        assert!(!card.rec().join(&old).exists(), "orphan uploaded and cleared");
        assert!(card.rec().join(&young).exists(), "young file untouched");

        // The receiver got inferred metadata, clearly labelled.
        let json = mock
            .stored(&format!("{}.json", old.trim_end_matches(".h264")))
            .expect("inferred sidecar");
        let text = String::from_utf8_lossy(&json);
        assert!(text.contains("\"sidecar\": \"inferred\""), "{text}");
        assert!(text.contains("\"reason\": \"orphan\""), "{text}");
        assert!(text.contains("\"bytes\": 1234"), "{text}");
    }

    #[test]
    fn expired_segments_are_skipped_and_kept() {
        let card = Tmp::new();
        let mock = Mock::start();
        let stale = write_segment(&card.rec(), &stamp_ago(5 * 86_400_000), 1000);
        let fresh = write_segment(&card.rec(), &stamp_ago(120_000), 1000);

        let mut c = cfg(&mock);
        c.keep_days = 1;
        let mut u = Uploader::new(&card.0, c);
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.expired, 1);
        assert_eq!(rep.uploaded.len(), 1);
        assert_eq!(rep.uploaded[0].name, fresh);
        assert!(card.rec().join(&stale).exists(), "expired footage is never deleted");
    }

    #[test]
    fn a_file_the_receiver_already_holds_is_not_re_sent() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 3000);
        let expect_bytes = dir_bytes(&card.rec());
        // The last pass uploaded and committed it, then the reply was lost.
        mock.with(|m| {
            m.finals
                .insert(a.clone(), (3000, sha256::hex_digest(&payload(3000))));
        });

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();

        assert_eq!(rep.uploaded.len(), 1, "{:?}", rep.failed);
        assert!(rep.uploaded[0].already_there);
        assert_eq!(rep.deleted, 1);
        assert_eq!(
            rep.bytes_sent,
            expect_bytes - 3000,
            "no video bytes re-sent"
        );
        assert!(!card.rec().join(&a).exists());
    }

    #[test]
    fn a_missing_card_is_a_card_error_not_a_panic() {
        let cfg = UpConfig {
            base_url: "http://127.0.0.1:1/ingest/".to_string(),
            ..UpConfig::default()
        };
        let mut u = Uploader::new(Path::new("/nonexistent/camrtsp-not-a-card"), cfg);
        match u.upload_pending() {
            Err(UpError::NoCard(m)) => assert!(m.contains("not there"), "{m}"),
            other => panic!("expected NoCard, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_rec_dir_is_an_empty_pass() {
        let card = Tmp::new();
        let mock = Mock::start();
        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();
        assert_eq!(rep.considered, 0);
        assert!(rep.uploaded.is_empty());
        assert!(!rep.aborted);
        // No rec/ yet is also fine (a card that has never recorded).
        fs::remove_dir_all(card.rec()).unwrap();
        assert_eq!(u.refresh().unwrap(), 0);
    }

    #[test]
    fn base_url_parsing_picks_scheme_and_port() {
        let base = UpConfig::default();
        assert!(Endpoint::from_config(&base).is_err());

        // https default port 443, tls on.
        let https = Endpoint::from_config(&UpConfig {
            base_url: "https://cam.ackl.me/ingest/".to_string(),
            ..UpConfig::default()
        })
        .unwrap();
        assert_eq!((https.host.as_str(), https.port, https.tls), ("cam.ackl.me", 443, true));
        assert_eq!(https.target("a.h264"), "/ingest/a.h264");

        // explicit port wins, and http still means no TLS.
        let explicit = Endpoint::from_config(&UpConfig {
            base_url: "https://cam.ackl.me:8443/ingest".to_string(),
            ..UpConfig::default()
        })
        .unwrap();
        assert_eq!((explicit.port, explicit.tls), (8443, true));

        let junk = Endpoint::from_config(&UpConfig {
            base_url: "ftp://example.com/x".to_string(),
            ..UpConfig::default()
        })
        .unwrap_err();
        assert!(junk.contains("http://"), "{junk}");

        let ep = Endpoint::from_config(&UpConfig {
            base_url: "http://192.168.50.20:8088/ingest/".to_string(),
            ..UpConfig::default()
        })
        .unwrap();
        assert_eq!((ep.host.as_str(), ep.port, ep.prefix.as_str()), ("192.168.50.20", 8088, "/ingest"));
        assert_eq!(ep.target("a.h264.part"), "/ingest/a.h264.part");

        let bare = Endpoint::from_config(&UpConfig {
            base_url: "http://cam".to_string(),
            ..UpConfig::default()
        })
        .unwrap();
        assert_eq!((bare.port, bare.prefix.as_str()), (80, ""));
        assert_eq!(bare.target("x"), "/x");

        let v6 = Endpoint::from_config(&UpConfig {
            base_url: "http://[::1]:9000/i".to_string(),
            ..UpConfig::default()
        })
        .unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 9000));
    }

    #[test]
    fn token_is_sent_and_a_receiver_can_require_it() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 900);
        mock.with(|m| m.require_token = Some("s3cret".to_string()));

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();
        assert_eq!(rep.uploaded.len(), 1, "{:?}", rep.failed);
        assert!(!card.rec().join(&a).exists());

        // And a wrong token fails loudly instead of silently uploading.
        let card2 = Tmp::new();
        let b = write_segment(&card2.rec(), &stamp_ago(120_000), 900);
        let mut c = cfg(&mock);
        c.token = "wrong".to_string();
        let mut u2 = Uploader::new(&card2.0, c);
        let rep2 = u2.upload_pending().unwrap();
        assert!(rep2.uploaded.is_empty());
        assert!(card2.rec().join(&b).exists());
        assert!(
            rep2.failed.iter().any(|f| f.kind == FailKind::Http),
            "{:?}",
            rep2.failed
        );
    }

    #[test]
    fn a_receiver_that_never_echoes_its_sha_cannot_make_us_delete() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 900);
        // Verify::None keeps everything, even on a clean 200.
        let mut c = cfg(&mock);
        c.verify = Verify::None;
        let mut u = Uploader::new(&card.0, c);
        let rep = u.upload_pending().unwrap();
        assert_eq!(rep.uploaded.len(), 1);
        assert_eq!(rep.deleted, 0);
        assert!(card.rec().join(&a).exists(), "no verify, no delete");
    }

    #[test]
    fn queue_is_oldest_first() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(300_000), 100);
        let b = write_segment(&card.rec(), &stamp_ago(200_000), 100);
        let c = write_segment(&card.rec(), &stamp_ago(100_000), 100);
        // A stray `.part` is somebody else's and must never be picked up.
        fs::write(card.rec().join("20260101-000000.h264.part"), b"junk").unwrap();

        let u = Uploader::new(&card.0, cfg(&mock));
        assert_eq!(names(&u), vec![a, b, c]);
    }

    #[test]
    fn an_offset_beyond_the_file_asks_for_a_reset_instead_of_truncating() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 1000);
        mock.with(|m| {
            m.parts.insert(a.clone(), payload(4000));
        });

        let mut u = Uploader::new(&card.0, cfg(&mock));
        let rep = u.upload_pending().unwrap();
        assert_eq!(rep.uploaded.len(), 1, "{:?}", rep.failed);
        assert!(mock.count("DELETE ") >= 1, "a reset was requested");
        assert_eq!(mock.final_sha(&a).unwrap(), sha256::hex_digest(&payload(1000)));
    }

    #[test]
    fn a_stuck_file_is_reported_and_kept_but_not_retried_forever() {
        let card = Tmp::new();
        let mock = Mock::start();
        let a = write_segment(&card.rec(), &stamp_ago(120_000), 900);
        mock.with(|m| m.require_token = Some("never-matches".to_string()));
        let mut c = cfg(&mock);
        c.max_attempts_per_file = 2;
        let mut u = Uploader::new(&card.0, c);
        let _ = u.upload_pending().unwrap();
        let rep2 = u.upload_pending().unwrap();
        assert_eq!(rep2.failed[0].kind, FailKind::Http);
        let rep3 = u.upload_pending().unwrap();
        assert_eq!(rep3.failed[0].kind, FailKind::Stuck, "{:?}", rep3.failed);
        assert!(card.rec().join(&a).exists(), "stuck files are never deleted");
    }

    #[test]
    fn report_summary_is_one_line() {
        let rep = UploadReport {
            considered: 3,
            deleted: 2,
            bytes_sent: 1024,
            aborted: true,
            ..UploadReport::default()
        };
        let s = rep.summary();
        assert!(s.starts_with("upload: 3 considered"), "{s}");
        assert!(s.contains("link down"), "{s}");
        assert!(!s.contains('\n'));
    }
}
