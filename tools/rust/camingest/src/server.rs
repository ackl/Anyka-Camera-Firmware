//! The receiver: accept a socket, answer one request, close.
//!
//! Thread per connection with a hard cap, because the camera opens a fresh
//! connection per request and every request is short. Nothing here blocks
//! indefinitely: sockets carry read and write timeouts, bodies are capped
//! before they are read, and the whole handler is wrapped in `catch_unwind` so
//! that a bug in one request logs a line instead of taking the process down
//! with it.

use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::http::{self, HeadError, Request, Response};
use crate::sha256;
use crate::store::{self, Append, Commit, Op, ResolveErr, Store};

/// The biggest chunk a camera may send. `chunk_bytes` defaults to 256 KiB;
/// `max_bytes_per_run` only shrinks it. 8 MiB is a generous ceiling that still
/// keeps one request from being an allocation attack.
pub const DEFAULT_MAX_CHUNK: usize = 8 << 20;
/// A sidecar is a few hundred bytes of JSON. Anything past this is not one.
pub const DEFAULT_MAX_JSON: usize = 256 << 10;
/// One segment: 60 s at ~1 Mbit/s is ~1 MB. A gigabyte is far past "wrong
/// units" and still small enough to refuse before touching the disk.
pub const MAX_TOTAL: u64 = 1 << 30;
pub const DEFAULT_MAX_CONNS: usize = 32;
const DEFAULT_IO_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Debug)]
pub struct Cfg {
    pub listen: String,
    pub root: PathBuf,
    /// Normalised: `""` for the root, else starts with `/` and has no trailing
    /// `/`. Matches the path part of the camera's `base_url`.
    pub prefix: String,
    pub token: Vec<u8>,
    pub max_conns: usize,
    pub io_timeout: Duration,
    pub max_chunk: usize,
    pub max_json: usize,
    pub quiet: bool,
}

impl Default for Cfg {
    fn default() -> Cfg {
        Cfg {
            listen: "0.0.0.0:8080".to_string(),
            root: PathBuf::from("/srv/storage/cam"),
            prefix: "/ingest".to_string(),
            token: Vec::new(),
            max_conns: DEFAULT_MAX_CONNS,
            io_timeout: Duration::from_secs(DEFAULT_IO_TIMEOUT_SECS),
            max_chunk: DEFAULT_MAX_CHUNK,
            max_json: DEFAULT_MAX_JSON,
            quiet: false,
        }
    }
}

impl Cfg {
    /// One line per accepted upload and one per failure, and nothing per chunk.
    pub fn log(&self, line: &str) {
        if !self.quiet {
            println!("{} {}", utc_stamp(), line);
        }
    }

    pub fn log_fail(&self, line: &str) {
        self.log(&format!("fail {line}"));
    }
}

pub struct Server {
    cfg: Cfg,
    listener: TcpListener,
    store: Arc<Store>,
    gate: Arc<Gate>,
    stopping: Arc<AtomicBool>,
}

impl Server {
    pub fn bind(cfg: Cfg) -> io::Result<Server> {
        let store = Arc::new(Store::open(&cfg.root)?);
        let addrs: Vec<SocketAddr> = cfg.listen.to_socket_addrs()?.collect();
        if addrs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{:?} resolved to no address", cfg.listen),
            ));
        }
        let mut last: Option<io::Error> = None;
        let mut bound: Option<TcpListener> = None;
        for a in addrs {
            match TcpListener::bind(a) {
                Ok(l) => {
                    bound = Some(l);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let listener = match bound {
            Some(l) => l,
            None => {
                return Err(last.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::AddrNotAvailable, "cannot bind")
                }))
            }
        };
        Ok(Server {
            gate: Arc::new(Gate::new(cfg.max_conns)),
            cfg,
            listener,
            store,
            stopping: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// A handle that stops the accept loop. Used by the tests, and by any
    /// future caller that wants a clean shutdown instead of a kill.
    #[allow(dead_code)]
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            flag: self.stopping.clone(),
        }
    }

    /// Accept until `stop()` or an unrecoverable error. Per-connection errors
    /// never end the loop: a peer that resets is normal.
    pub fn serve_forever(self) -> io::Result<()> {
        let Server {
            cfg,
            listener,
            store,
            gate,
            stopping,
        } = self;
        loop {
            if stopping.load(Ordering::Relaxed) {
                return Ok(());
            }
            let (stream, peer) = match listener.accept() {
                Ok(x) => x,
                Err(e) => match e.kind() {
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => continue,
                    // A dead listener is the one error worth ending on.
                    io::ErrorKind::InvalidInput | io::ErrorKind::NotConnected => {
                        return Err(e);
                    }
                    // EMFILE and friends: back off rather than spin.
                    _ => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                },
            };
            let guard = match gate.enter() {
                Some(g) => g,
                None => {
                    // Over the cap: answer rather than queue, so the camera's
                    // bounded retry decides what happens next.
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let mut s = stream;
                    let r = Response::err(
                        503,
                        "Service Unavailable",
                        "too many connections at once; retry with backoff",
                    );
                    let _ = r.write_to(&mut s, false);
                    let _ = s.flush();
                    let _ = s.shutdown(Shutdown::Both);
                    cfg.log_fail(&format!("busy: 503 for {peer} (cap {})", cfg.max_conns));
                    continue;
                }
            };

            let cfg2 = cfg.clone();
            let store2 = store.clone();
            let spawned = std::thread::Builder::new()
                .name("camingest-conn".to_string())
                .stack_size(512 * 1024)
                .spawn(move || {
                    let _guard = guard;
                    let peer = peer.to_string();
                    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        handle_conn(&cfg2, &store2, stream, &peer);
                    }));
                    if r.is_err() {
                        // The process is the archive's only server; a panic in
                        // one request must not become an outage.
                        cfg2.log_fail(&format!("internal: handler panicked for {peer}; kept running"));
                    }
                });
            if let Err(e) = spawned {
                cfg.log_fail(&format!("internal: cannot spawn a thread ({e}); dropping a connection"));
            }
        }
    }
}

#[allow(dead_code)]
pub struct StopHandle {
    flag: Arc<AtomicBool>,
}

impl StopHandle {
    #[allow(dead_code)]
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

/// Connection cap. Thread per connection is fine for a camera; the cap is what
/// keeps a hostile LAN from turning it into a thread bomb.
struct Gate {
    cap: usize,
    live: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new(cap: usize) -> Gate {
        Gate {
            cap: cap.max(1),
            live: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn enter(self: &Arc<Self>) -> Option<Guard> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if *live >= self.cap {
            return None;
        }
        *live += 1;
        Some(Guard {
            gate: self.clone(),
        })
    }
}

struct Guard {
    gate: Arc<Gate>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut live = self.gate.live.lock().unwrap_or_else(|e| e.into_inner());
        *live = live.saturating_sub(1);
        self.gate.cv.notify_one();
    }
}

// ---- one connection -------------------------------------------------------

fn handle_conn(cfg: &Cfg, store: &Store, stream: TcpStream, peer: &str) {
    let _ = stream.set_read_timeout(Some(cfg.io_timeout));
    let _ = stream.set_write_timeout(Some(cfg.io_timeout));
    let _ = stream.set_nodelay(true);
    let mut br = BufReader::with_capacity(16 * 1024, stream);

    let req = match http::read_request(&mut br) {
        Ok(r) => r,
        // Nothing was said (or the peer left). No reply is possible.
        Err(HeadError::Eof) => return,
        Err(HeadError::TooLarge) => {
            cfg.log_fail(&format!("request: head too large from {peer}"));
            finish(&mut br, &Response::err(431, "Request Header Fields Too Large", "request head too large"), false);
            return;
        }
        Err(HeadError::Bad(why)) => {
            cfg.log_fail(&format!("request: {why} from {peer}"));
            finish(&mut br, &Response::err(400, "Bad Request", why), false);
            return;
        }
    };

    let head_only = req.method == "HEAD";
    let resp = route(cfg, store, &req, &mut br, peer);
    finish(&mut br, &resp, head_only);
}

fn finish(br: &mut BufReader<TcpStream>, resp: &Response, head_only: bool) {
    let s = br.get_mut();
    if resp.is_silent() {
        let _ = s.shutdown(Shutdown::Both);
        return;
    }
    let _ = resp.write_to(s, head_only);
    let _ = s.flush();
    // Half-close first, then swallow whatever the peer is still sending: a
    // client that was mid-body when we refused would otherwise get an RST
    // instead of reading the answer, and "connection reset" hides the reason.
    let _ = s.shutdown(Shutdown::Write);
    let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
    let mut sink = [0u8; 2048];
    let mut drained = 0usize;
    while drained < 256 * 1024 {
        match s.read(&mut sink) {
            Ok(0) => break,
            Ok(n) => drained += n,
            Err(_) => break,
        }
    }
}

fn route(
    cfg: &Cfg,
    store: &Store,
    req: &Request,
    br: &mut BufReader<TcpStream>,
    peer: &str,
) -> Response {
    if let Err(resp) = authorize(cfg, req, peer) {
        return resp;
    }

    // Every body we accept is length-delimited, because that is the only kind
    // we can refuse before reading it. A chunked body would have to be read to
    // be measured, which is the one thing a public endpoint must not do.
    if req.header("transfer-encoding").is_some() {
        cfg.log_fail(&format!(
            "request: chunked body for {} {} from {peer}",
            req.method,
            clip_target(&req.target)
        ));
        return Response::err(
            411,
            "Length Required",
            "send Content-Length; chunked request bodies are not supported",
        );
    }

    let op = match store::resolve(&cfg.prefix, &req.target) {
        Ok(op) => op,
        Err(ResolveErr::NotOurs) => {
            cfg.log_fail(&format!(
                "path: refused {} {} from {peer}",
                req.method,
                clip_target(&req.target)
            ));
            return Response::err(404, "Not Found", "not a path this receiver serves");
        }
        Err(ResolveErr::Bad(why)) => {
            cfg.log_fail(&format!(
                "path: {why}: {} {} from {peer}",
                req.method,
                clip_target(&req.target)
            ));
            return Response::err(400, "Bad Request", why);
        }
    };

    let allow = match &op {
        Op::Segment(_) => "HEAD",
        Op::Part(_) => "HEAD, PUT, DELETE",
        Op::Sidecar(_) => "PUT",
        Op::Finalize(_) => "POST",
    };

    match (req.method.as_str(), op) {
        ("HEAD", Op::Segment(name)) => head_segment(store, &name, peer, cfg),
        ("HEAD", Op::Part(name)) => head_part(store, &name),
        ("PUT", Op::Part(name)) => put_part(cfg, store, req, br, &name, peer),
        ("PUT", Op::Sidecar(base)) => put_sidecar(cfg, store, req, br, &base, peer),
        ("POST", Op::Finalize(name)) => finalize(cfg, store, req, br, &name, peer),
        ("DELETE", Op::Part(name)) => delete_part(cfg, store, &name, peer),
        (method, _) => {
            cfg.log_fail(&format!(
                "verb: {method} {} from {peer} is not allowed",
                clip_target(&req.target)
            ));
            Response::err(
                405,
                "Method Not Allowed",
                &format!("{method} is not allowed on this resource"),
            )
            .header("Allow", allow)
        }
    }
}

/// The token is the whole authorisation model (see `upload.rs`), so: constant
/// time compare, no echo of the presented value, 401 when there is nothing to
/// compare and 403 when there is and it is wrong.
fn authorize(cfg: &Cfg, req: &Request, peer: &str) -> Result<(), Response> {
    let presented = match req.header("x-cam-token") {
        Some(v) => Some(v.trim().to_string()),
        None => req.header("authorization").and_then(|v| {
            let v = v.trim();
            match v.split_once(' ') {
                Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => {
                    Some(rest.trim().to_string())
                }
                _ => None,
            }
        }),
    };
    match presented {
        None => {
            cfg.log_fail(&format!("auth: no token from {peer}"));
            Err(Response::err(
                401,
                "Unauthorized",
                "X-Cam-Token (or Authorization: Bearer) is required",
            )
            .header("WWW-Authenticate", "X-Cam-Token realm=\"camingest\""))
        }
        Some(t) => {
            if ct_eq(t.as_bytes(), &cfg.token) {
                Ok(())
            } else {
                cfg.log_fail(&format!("auth: wrong token from {peer}"));
                Err(Response::err(403, "Forbidden", "token rejected"))
            }
        }
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    // Length is not secret; contents are. Fold the lengths in so equal-prefix
    // tokens cost the same as different ones.
    let mut diff = if a.len() == b.len() { 0u8 } else { 0xff };
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

// ---- handlers -------------------------------------------------------------

/// Handler 1: is the finished file here, and what does it hash to?
fn head_segment(store: &Store, name: &str, peer: &str, cfg: &Cfg) -> Response {
    match store.segment_fingerprint(name) {
        Ok(Some((bytes, sha))) => Response::ok()
            .header("X-Stored-Sha256", sha)
            .header("X-Stored-Bytes", bytes.to_string()),
        Ok(None) => Response::err(404, "Not Found", "no such segment"),
        Err(e) => {
            cfg.log_fail(&format!("head {name}: cannot hash: {e} from {peer}"));
            Response::err(500, "Internal Server Error", "cannot read the stored segment")
        }
    }
}

/// Handler 2: how much of it do we already hold?
fn head_part(store: &Store, name: &str) -> Response {
    match store.part_len(name) {
        Some(n) => Response::ok()
            .header("X-Upload-Offset", n.to_string())
            // The camera accepts either name for the same number.
            .header("X-Stored-Bytes", n.to_string()),
        None => Response::err(404, "Not Found", "no partial object"),
    }
}

/// Handler 3: a chunk at the offset we reported.
fn put_part(
    cfg: &Cfg,
    store: &Store,
    req: &Request,
    br: &mut BufReader<TcpStream>,
    name: &str,
    peer: &str,
) -> Response {
    let range = match req.header("content-range") {
        Some(v) => v,
        None => {
            cfg.log_fail(&format!("put {name}: missing Content-Range from {peer}"));
            return Response::err(411, "Length Required", "Content-Range: bytes FIRST-LAST/TOTAL is required");
        }
    };
    let (first, last, total) = match parse_content_range(range) {
        Some(x) => x,
        None => {
            cfg.log_fail(&format!("put {name}: bad Content-Range {range:?} from {peer}"));
            return Response::err(400, "Bad Request", "Content-Range must be `bytes FIRST-LAST/TOTAL`");
        }
    };
    if last < first {
        cfg.log_fail(&format!("put {name}: inverted range {range:?} from {peer}"));
        return Response::err(400, "Bad Request", "Content-Range ends before it starts");
    }
    if last >= total {
        cfg.log_fail(&format!("put {name}: range past total {range:?} from {peer}"));
        return Response::err(400, "Bad Request", "Content-Range ends at or past the declared total");
    }
    let want = last - first + 1;
    let len = match req.content_length() {
        Some(l) => l,
        None => {
            cfg.log_fail(&format!("put {name}: no Content-Length from {peer}"));
            return Response::err(411, "Length Required", "Content-Length is required on a chunk");
        }
    };
    if len as u64 != want {
        cfg.log_fail(&format!(
            "put {name}: Content-Length {len} disagrees with range {range:?} from {peer}"
        ));
        return Response::err(400, "Bad Request", "Content-Length disagrees with Content-Range");
    }
    if len == 0 {
        return Response::err(400, "Bad Request", "a chunk must carry at least one byte");
    }
    if len > cfg.max_chunk {
        cfg.log_fail(&format!(
            "put {name}: chunk of {len} bytes exceeds the {} byte cap from {peer}",
            cfg.max_chunk
        ));
        return Response::err(
            413,
            "Payload Too Large",
            &format!("chunks are capped at {} bytes", cfg.max_chunk),
        );
    }
    let declared = match req.header("content-sha256") {
        Some(v) if sha256::is_hex_sha256(v.trim()) => v.trim().to_ascii_lowercase(),
        Some(_) => {
            cfg.log_fail(&format!("put {name}: malformed Content-SHA256 from {peer}"));
            return Response::err(400, "Bad Request", "Content-SHA256 must be 64 hex characters");
        }
        None => {
            cfg.log_fail(&format!("put {name}: missing Content-SHA256 from {peer}"));
            return Response::err(400, "Bad Request", "Content-SHA256 is required on a chunk");
        }
    };

    // The body is buffered, not streamed into the `.part`: a chunk that is
    // short, truncated or corrupt must leave the partial object exactly as it
    // was, so the client's resume point is still honest.
    let _ = http::maybe_continue(req, br.get_mut());
    let body = match http::read_body(br, len) {
        Ok(b) => b,
        Err(e) => {
            cfg.log_fail(&format!("put {name}: body cut short at {len} bytes ({e}) from {peer}"));
            return Response::silent();
        }
    };
    let got = sha256::hex_digest(&body);
    if got != declared {
        cfg.log_fail(&format!(
            "put {name}: chunk sha256 {got} != declared {declared} from {peer}"
        ));
        return Response::err(400, "Bad Request", "chunk sha256 does not match Content-SHA256");
    }
    if first.saturating_add(len as u64) > total {
        cfg.log_fail(&format!("put {name}: chunk runs past total {total} from {peer}"));
        return Response::err(400, "Bad Request", "chunk ends past the declared total");
    }

    match store.append(name, first, &body) {
        Ok(Append::Appended(next)) => {
            Response::ok().header("X-Upload-Offset", next.to_string())
        }
        Ok(Append::Conflict(have)) => {
            // Not a failure: this is how a retried chunk converges. The client
            // adopts `have` and continues from there.
            Response::err(
                409,
                "Conflict",
                &format!("we hold {have} bytes, not {first}; continue from X-Upload-Offset"),
            )
            .header("X-Upload-Offset", have.to_string())
        }
        Err(e) => {
            cfg.log_fail(&format!("put {name}: cannot append at {first}: {e}"));
            Response::err(500, "Internal Server Error", "cannot write the partial object")
        }
    }
}

/// Handler 4: commit — the only place a file becomes a segment.
fn finalize(
    cfg: &Cfg,
    store: &Store,
    req: &Request,
    br: &mut BufReader<TcpStream>,
    name: &str,
    peer: &str,
) -> Response {
    let total = match req.header_u64("x-upload-total") {
        Some(t) => t,
        None => {
            cfg.log_fail(&format!("finalize {name}: no X-Upload-Total from {peer}"));
            return Response::err(400, "Bad Request", "X-Upload-Total is required");
        }
    };
    let declared = match req.header("x-upload-sha256") {
        Some(v) if sha256::is_hex_sha256(v.trim()) => v.trim().to_ascii_lowercase(),
        Some(_) => {
            cfg.log_fail(&format!("finalize {name}: malformed X-Upload-Sha256 from {peer}"));
            return Response::err(400, "Bad Request", "X-Upload-Sha256 must be 64 hex characters");
        }
        None => {
            cfg.log_fail(&format!("finalize {name}: no X-Upload-Sha256 from {peer}"));
            return Response::err(400, "Bad Request", "X-Upload-Sha256 is required");
        }
    };
    if total > MAX_TOTAL {
        cfg.log_fail(&format!(
            "finalize {name}: total {total} exceeds the {MAX_TOTAL} byte cap from {peer}"
        ));
        return Response::err(413, "Payload Too Large", "that total is not a video segment");
    }
    // A commit carries no body; swallow a tiny one so the socket is not left
    // with unread bytes, refuse a big one.
    match req.content_length() {
        None | Some(0) => {}
        Some(n) if n <= 8192 => {
            let _ = http::read_body(br, n);
        }
        Some(_) => {
            return Response::err(400, "Bad Request", "finalize must not carry a body");
        }
    }

    match store.commit(name, total, &declared) {
        Ok(Commit::Done { bytes, sha }) => {
            cfg.log(&format!("upload {name} video {bytes} bytes sha256 {sha}"));
            Response::ok()
                .header("X-Stored-Sha256", sha)
                .header("X-Stored-Bytes", bytes.to_string())
        }
        Ok(Commit::OffsetMismatch { have }) => {
            cfg.log_fail(&format!(
                "finalize {name}: {have} of {total} bytes held from {peer}"
            ));
            Response::err(
                409,
                "Conflict",
                &format!("we hold {have} of {total} bytes; continue from X-Upload-Offset"),
            )
            .header("X-Upload-Offset", have.to_string())
        }
        Ok(Commit::ShaMismatch { stored }) => {
            cfg.log_fail(&format!(
                "finalize {name}: stored sha256 {stored} != declared {declared} from {peer}; .part kept"
            ));
            Response::err(
                422,
                "Unprocessable Entity",
                &format!("we stored sha256 {stored}, which is not what was declared; retry the file"),
            )
            .header("X-Stored-Sha256", stored)
        }
        Ok(Commit::Missing) => {
            cfg.log_fail(&format!("finalize {name}: no bytes to commit from {peer}"));
            Response::err(404, "Not Found", "no partial object to commit")
        }
        Err(e) => {
            cfg.log_fail(&format!("finalize {name}: {e}"));
            Response::err(500, "Internal Server Error", "cannot commit the segment")
        }
    }
}

/// Handler 5: the sidecar, whole.
fn put_sidecar(
    cfg: &Cfg,
    store: &Store,
    req: &Request,
    br: &mut BufReader<TcpStream>,
    base: &str,
    peer: &str,
) -> Response {
    let len = match req.content_length() {
        Some(l) => l,
        None => {
            cfg.log_fail(&format!("sidecar {base}: no Content-Length from {peer}"));
            return Response::err(411, "Length Required", "Content-Length is required");
        }
    };
    if len > cfg.max_json {
        cfg.log_fail(&format!(
            "sidecar {base}: {len} bytes exceeds the {} byte cap from {peer}",
            cfg.max_json
        ));
        return Response::err(
            413,
            "Payload Too Large",
            &format!("sidecars are capped at {} bytes", cfg.max_json),
        );
    }
    let declared = match req.header("content-sha256") {
        Some(v) if sha256::is_hex_sha256(v.trim()) => v.trim().to_ascii_lowercase(),
        Some(_) => {
            return Response::err(400, "Bad Request", "Content-SHA256 must be 64 hex characters")
        }
        None => {
            cfg.log_fail(&format!("sidecar {base}: missing Content-SHA256 from {peer}"));
            return Response::err(400, "Bad Request", "Content-SHA256 is required");
        }
    };
    let _ = http::maybe_continue(req, br.get_mut());
    let body = match http::read_body(br, len) {
        Ok(b) => b,
        Err(e) => {
            cfg.log_fail(&format!("sidecar {base}: body cut short ({e}) from {peer}"));
            return Response::silent();
        }
    };
    let got = sha256::hex_digest(&body);
    if got != declared {
        cfg.log_fail(&format!(
            "sidecar {base}: sha256 {got} != declared {declared} from {peer}"
        ));
        return Response::err(400, "Bad Request", "sha256 does not match Content-SHA256");
    }
    match store.put_sidecar(base, &body) {
        Ok((bytes, sha)) => {
            cfg.log(&format!("upload {base}.json sidecar {bytes} bytes sha256 {sha}"));
            Response::ok()
                .header("X-Stored-Sha256", sha)
                .header("X-Stored-Bytes", bytes.to_string())
        }
        Err(e) => {
            cfg.log_fail(&format!("sidecar {base}: cannot write: {e}"));
            Response::err(500, "Internal Server Error", "cannot write the sidecar")
        }
    }
}

/// Optional handler: drop a stale `.part`.
fn delete_part(cfg: &Cfg, store: &Store, name: &str, peer: &str) -> Response {
    match store.delete_part(name) {
        Ok(had) => {
            cfg.log(&format!(
                "reset {name}.part {}",
                match had {
                    Some(n) => format!("dropped {n} bytes"),
                    None => "nothing held".to_string(),
                }
            ));
            Response::ok().header("X-Upload-Offset", "0")
        }
        Err(e) => {
            cfg.log_fail(&format!("delete {name}.part from {peer}: {e}"));
            Response::err(500, "Internal Server Error", "cannot remove the partial object")
        }
    }
}

// ---- helpers --------------------------------------------------------------

/// `bytes 1024-2047/3000` → `(1024, 2047, 3000)`. Digits only: `+5`, `0x10`
/// and whitespace tricks are not a range.
pub fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim();
    let rest = v.strip_prefix("bytes ")?.trim();
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let first = digits(a)?;
    let last = digits(b)?;
    let total = digits(total)?;
    Some((first, last, total))
}

fn digits(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s.len() > 19 {
        return None;
    }
    if !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

fn clip_target(t: &str) -> String {
    let mut out = String::with_capacity(t.len().min(120));
    for c in t.chars() {
        if out.len() >= 120 {
            out.push('…');
            break;
        }
        out.push(if c.is_control() { '?' } else { c });
    }
    out
}

/// `2026-11-08T22:53:29Z`, no crate. Log lines are read by a human at 3 a.m.;
/// unix seconds are not.
pub fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Normalise `--prefix`: `""`, `"/"` → root; otherwise `/path` with no trailing
/// slash. The camera's `base_url` path is used verbatim, so this has to match
/// what an operator typed there.
pub fn normalise_prefix(p: &str) -> Result<String, String> {
    let p = p.trim();
    if p.is_empty() || p == "/" {
        return Ok(String::new());
    }
    if p.contains('?') || p.contains('#') || p.contains("//") {
        return Err(format!("{p:?} is not a plain path"));
    }
    let p = if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    };
    Ok(p.trim_end_matches('/').to_string())
}
