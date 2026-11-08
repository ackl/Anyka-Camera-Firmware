//! The store: where the bytes land, and the only code that decides whether a
//! file is *complete*.
//!
//! Layout under `--root` (default `/srv/storage/cam`):
//!
//! ```text
//! <root>/rec/<NAME>.h264          a video segment, only ever created by a
//!                                 commit whose sha256 check passed
//! <root>/rec/<NAME>.json          its sidecar, written whole, then renamed
//! <root>/rec/<NAME>.h264.part     bytes in flight, appended chunk by chunk
//! <root>/rec/<NAME>.json.tmp      a sidecar mid-write (never served, never
//!                                 matched by the client's HEAD)
//! ```
//!
//! `<NAME>` is the camera's segment name, `YYYYMMDD-HHMMSS.h264`, taken from
//! `rec.rs` and validated to `[A-Za-z0-9._-]` with no leading dot and no `..`
//! before it ever touches a path. That is the whole traversal defence: an
//! acceptable name cannot contain a separator, so a path built from one cannot
//! leave the directory.
//!
//! A `.part` is never mistaken for a segment because the segment name never
//! ends in `.part`, and the client only ever asks for a finished file by its
//! own name.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::sha256;

/// Directory (under the root) that holds the segments — `rec::SUBDIR` on the
/// camera, so a copy of the tree looks the same on both ends.
pub const REC_SUBDIR: &str = "rec";
/// Video segments are the only thing with a `.part`: the camera's names end
/// here and its chunk uploads are `NAME.part`.
pub const VIDEO_SUFFIX: &str = ".h264";
const PART_SUFFIX: &str = ".part";
const SIDECAR_SUFFIX: &str = ".json";
const FINALIZE_SUFFIX: &str = ".finalize";
/// Long enough for any name the camera produces, short enough to be a name.
const MAX_NAME: usize = 200;

/// What a URL under the prefix is asking about. The method decides the verb
/// (see `server::dispatch`); this is the noun.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `NAME` — a complete segment.
    Segment(String),
    /// `NAME.part` — bytes in flight.
    Part(String),
    /// `BASE.json` — the sidecar.
    Sidecar(String),
    /// `NAME.finalize` — the commit.
    Finalize(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveErr {
    /// Not under our prefix (or not a shape we serve): 404, quietly.
    NotOurs,
    /// Under our prefix but malformed: 400, loudly, because it is a bug or an
    /// attack and both are worth a log line.
    Bad(&'static str),
}

/// Map a request target to an `Op`, accepting nothing that could escape the
/// store. `prefix` is normalised by the CLI: `""` (root) or `"/ingest"`.
pub fn resolve(prefix: &str, target: &str) -> Result<Op, ResolveErr> {
    if !target.starts_with('/') {
        // Reject the absolute form ("http://host/path") outright: it is a
        // different request syntax and no client here sends it.
        return Err(ResolveErr::Bad("request target must be an absolute path"));
    }
    if target.contains('?') || target.contains('#') {
        return Err(ResolveErr::Bad("query strings and fragments are not used"));
    }
    if target.contains("//") {
        return Err(ResolveErr::Bad("empty path segment"));
    }
    if target.contains('\\') {
        return Err(ResolveErr::Bad("backslash in path"));
    }

    let rest = match prefix {
        "" => {
            if target == "/" {
                return Err(ResolveErr::Bad("no file name"));
            }
            &target[1..]
        }
        p => match target.strip_prefix(p) {
            None => return Err(ResolveErr::NotOurs),
            Some("") => return Err(ResolveErr::Bad("no file name")),
            Some(r) => match r.strip_prefix('/') {
                Some(r) => r,
                // "/ingestfoo" is not inside "/ingest".
                None => return Err(ResolveErr::NotOurs),
            },
        },
    };

    // Percent-decode *before* validating, so "%2e%2e%2f" is judged as "../".
    let name = percent_decode(rest)?;
    if name.contains('/') || name.contains('\\') {
        return Err(ResolveErr::Bad("separators are not allowed in a file name"));
    }

    if let Some(video) = name.strip_suffix(PART_SUFFIX) {
        return match valid_video_name(video) {
            true => Ok(Op::Part(video.to_string())),
            false => Err(ResolveErr::Bad("bad segment name")),
        };
    }
    if let Some(video) = name.strip_suffix(FINALIZE_SUFFIX) {
        return match valid_video_name(video) {
            true => Ok(Op::Finalize(video.to_string())),
            false => Err(ResolveErr::Bad("bad segment name")),
        };
    }
    if let Some(base) = name.strip_suffix(SIDECAR_SUFFIX) {
        return match valid_name(base) {
            true => Ok(Op::Sidecar(base.to_string())),
            false => Err(ResolveErr::Bad("bad sidecar name")),
        };
    }
    match valid_video_name(&name) {
        true => Ok(Op::Segment(name)),
        false => Err(ResolveErr::NotOurs),
    }
}

/// A segment name: the camera's `YYYYMMDD-HHMMSS.h264`.
fn valid_video_name(name: &str) -> bool {
    valid_name(name) && name.ends_with(VIDEO_SUFFIX) && name.len() > VIDEO_SUFFIX.len()
}

fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    if name.starts_with('.') || name.contains("..") {
        return false;
    }
    name.bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_')
}

fn percent_decode(s: &str) -> Result<String, ResolveErr> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err(ResolveErr::Bad("truncated percent escape"));
            }
            let hi = hexval(b[i + 1]).ok_or(ResolveErr::Bad("bad percent escape"))?;
            let lo = hexval(b[i + 2]).ok_or(ResolveErr::Bad("bad percent escape"))?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // Anything that is not printable ASCII is not a name we make. This also
    // kills NUL, CR, LF and every encoding trick that follows them.
    if out.iter().any(|&c| !(0x20..0x7f).contains(&c)) {
        return Err(ResolveErr::Bad("non-printable byte in path"));
    }
    String::from_utf8(out).map_err(|_| ResolveErr::Bad("path is not ASCII"))
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// What a commit attempt found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Commit {
    /// Verified and renamed into place (or already verified from an earlier
    /// attempt — same answer either way, which is the point of idempotency).
    Done { bytes: u64, sha: String },
    /// We hold a different number of bytes than the client's total: tell it how
    /// many, and it carries on from there.
    OffsetMismatch { have: u64 },
    /// We hashed what we store and it is not what the client claims.
    ShaMismatch { stored: String },
    /// No `.part` and no final file: nothing to commit.
    Missing,
}

/// Result of appending a chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Append {
    Appended(u64),
    /// We already hold a different number of bytes; the client adopts `have`.
    Conflict(u64),
}

pub struct Store {
    rec: PathBuf,
    /// Serialises length-check-then-append and the commit rename. Two
    /// connections may be pushing the same name at once (a retried request
    /// racing its own timeout); neither may append over the other.
    lock: Mutex<()>,
}

impl Store {
    pub fn open(root: &Path) -> io::Result<Store> {
        let rec = root.join(REC_SUBDIR);
        fs::create_dir_all(&rec)?;
        Ok(Store {
            rec,
            lock: Mutex::new(()),
        })
    }

    pub fn rec_dir(&self) -> &Path {
        &self.rec
    }

    pub fn segment_path(&self, name: &str) -> PathBuf {
        self.rec.join(name)
    }

    pub fn part_path(&self, name: &str) -> PathBuf {
        self.rec.join(format!("{name}{PART_SUFFIX}"))
    }

    pub fn sidecar_path(&self, base: &str) -> PathBuf {
        self.rec.join(format!("{base}{SIDECAR_SUFFIX}"))
    }

    fn tmp_path(&self, base: &str) -> PathBuf {
        self.rec.join(format!("{base}{SIDECAR_SUFFIX}.tmp"))
    }

    fn len_of(path: &Path) -> Option<u64> {
        match fs::metadata(path) {
            Ok(m) if m.is_file() => Some(m.len()),
            _ => None,
        }
    }

    /// A finished segment as the client's HEAD handler 1 wants it: the sha256
    /// of the bytes **on disk now**, never a cached number.
    pub fn segment_fingerprint(&self, name: &str) -> io::Result<Option<(u64, String)>> {
        sha256::digest_file(&self.segment_path(name))
    }

    pub fn part_len(&self, name: &str) -> Option<u64> {
        Self::len_of(&self.part_path(name))
    }

    /// Append a verified chunk, but only at the offset the store really holds.
    pub fn append(&self, name: &str, first: u64, data: &[u8]) -> io::Result<Append> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.part_path(name);
        let have = Self::len_of(&path).unwrap_or(0);
        if first != have {
            return Ok(Append::Conflict(have));
        }
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(data)?;
        f.sync_all()?;
        let now = f.metadata()?.len();
        Ok(Append::Appended(now))
    }

    /// Commit: hash what we hold, compare against the client's claim, and only
    /// then let the bytes become a segment.
    pub fn commit(&self, name: &str, total: u64, declared_sha: &str) -> io::Result<Commit> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let part = self.part_path(name);
        let segment = self.segment_path(name);

        match Self::len_of(&part) {
            Some(have) => {
                if have != total {
                    return Ok(Commit::OffsetMismatch { have });
                }
                let (bytes, stored) = match sha256::digest_file(&part)? {
                    Some(x) => x,
                    None => return Ok(Commit::Missing),
                };
                if !stored.eq_ignore_ascii_case(declared_sha) {
                    return Ok(Commit::ShaMismatch { stored });
                }
                // Same filesystem, so this is an atomic rename: a reader either
                // sees no segment or the whole one.
                fs::rename(&part, &segment)?;
                sync_dir(&self.rec);
                Ok(Commit::Done {
                    bytes,
                    sha: stored,
                })
            }
            None => {
                // No `.part`: either the segment is already here (a commit
                // whose reply was lost) or there is nothing to commit.
                let (have, stored) = match sha256::digest_file(&segment)? {
                    Some(x) => x,
                    None => return Ok(Commit::Missing),
                };
                if have == total && stored.eq_ignore_ascii_case(declared_sha) {
                    return Ok(Commit::Done {
                        bytes: have,
                        sha: stored,
                    });
                }
                if have != total {
                    return Ok(Commit::OffsetMismatch { have });
                }
                Ok(Commit::ShaMismatch { stored })
            }
        }
    }

    /// Store a sidecar whole. Written beside the target and renamed, so an
    /// interrupted write can never leave a truncated `.json` that looks real.
    pub fn put_sidecar(&self, base: &str, body: &[u8]) -> io::Result<(u64, String)> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = self.tmp_path(base);
        let dest = self.sidecar_path(base);
        let digest = sha256::hex_digest(body);
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(body)?;
            f.sync_all()?;
        }
        match fs::rename(&tmp, &dest) {
            Ok(()) => {}
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
        sync_dir(&self.rec);
        Ok((body.len() as u64, digest))
    }

    /// Drop a `.part`. Idempotent: the camera sends this when it thinks we hold
    /// more than its file has, and repeats it whenever it loses a reply.
    pub fn delete_part(&self, name: &str) -> io::Result<Option<u64>> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.part_path(name);
        let had = Self::len_of(&path);
        match fs::remove_file(&path) {
            Ok(()) => {
                sync_dir(&self.rec);
                Ok(had)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// fsync a directory so a rename survives a power cut. Best effort: a
/// filesystem that refuses this is not a reason to fail an upload.
fn sync_dir(dir: &Path) {
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}
