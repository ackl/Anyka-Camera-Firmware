//! Tests that drive the real server over a real loopback socket with a
//! hand-rolled client, the same way `camrtsp/src/rtsp/tests.rs` drives the RTSP
//! server. Nothing here reaches into server internals to check a response: if
//! the bytes on the wire are wrong, a test fails.
//!
//! The client below matches `camrtsp/src/upload.rs` — same headers, same
//! `Connection: close` per request, same reading of verdicts out of headers —
//! because the point of this crate is to be byte-compatible with that file.
//! Where a test says "the camera does X", X is what `upload.rs` really does at
//! commit `cfdaeaf`.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::server::{self, Cfg, Server};
use crate::sha256;
use crate::store::{self, Op, ResolveErr};

const TOKEN: &str = "test-token-not-the-real-one";

// ---- harness --------------------------------------------------------------

struct Test {
    addr: SocketAddr,
    root: PathBuf,
    stop: server::StopHandle,
    prefix: String,
}

impl Test {
    fn rec(&self) -> PathBuf {
        self.root.join(store::REC_SUBDIR)
    }

    fn entries(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.rec())
            .expect("rec dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn client(&self) -> Cli {
        Cli {
            addr: self.addr,
            token: Some(TOKEN.to_string()),
            prefix: self.prefix.clone(),
        }
    }
}

impl Drop for Test {
    fn drop(&mut self) {
        self.stop.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn temp_root(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "camingest-test-{}-{tag}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp root");
    dir
}

fn start(tag: &str) -> Test {
    let root = temp_root(tag);
    let cfg = Cfg {
        listen: "127.0.0.1:0".to_string(),
        root: root.clone(),
        prefix: "/ingest".to_string(),
        token: TOKEN.as_bytes().to_vec(),
        max_conns: 8,
        // Short, so a client that never sends a line cannot stall the suite.
        io_timeout: Duration::from_secs(5),
        max_chunk: 1 << 20,
        max_json: 4096,
        quiet: true,
    };
    let srv = Server::bind(cfg).expect("bind");
    let addr = srv.local_addr().expect("local addr");
    let stop = srv.stop_handle();
    thread::spawn(move || {
        let _ = srv.serve_forever();
    });
    Test {
        addr,
        root,
        stop,
        prefix: "/ingest".to_string(),
    }
}

#[derive(Debug)]
struct Res {
    code: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Res {
    fn h(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn u(&self, name: &str) -> Option<u64> {
        self.h(name).and_then(|v| v.parse().ok())
    }

    fn ok(&self) -> bool {
        (200..300).contains(&self.code)
    }
}

struct Cli {
    addr: SocketAddr,
    token: Option<String>,
    prefix: String,
}

impl Cli {
    /// Send raw bytes, read whatever comes back (the server closes, so EOF ends
    /// the response). `None` means the server never answered.
    fn raw(&self, bytes: &[u8]) -> Option<Res> {
        let mut s = TcpStream::connect(self.addr).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
        s.set_write_timeout(Some(Duration::from_secs(20))).ok()?;
        s.write_all(bytes).ok()?;
        let _ = s.flush();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match s.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        parse(&buf)
    }

    fn must(&self, bytes: &[u8]) -> Res {
        match self.raw(bytes) {
            Some(r) => r,
            None => panic!("no response from the server"),
        }
    }

    fn build(
        &self,
        method: &str,
        target: &str,
        extra: &[(&str, &str)],
        body: &[u8],
        content_length: Option<usize>,
    ) -> Vec<u8> {
        let mut head = format!("{method} {target} HTTP/1.1\r\n");
        head.push_str("Host: 127.0.0.1\r\n");
        head.push_str("User-Agent: camrtsp/0.1.0\r\n");
        head.push_str("Connection: close\r\n");
        if let Some(t) = &self.token {
            head.push_str(&format!("X-Cam-Token: {t}\r\n"));
        }
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        match content_length {
            Some(n) => head.push_str(&format!("Content-Length: {n}\r\n")),
            None => {
                if !body.is_empty() || method == "PUT" || method == "POST" {
                    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
                }
            }
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn req(&self, method: &str, target: &str, extra: &[(&str, &str)], body: &[u8]) -> Res {
        self.must(&self.build(method, target, extra, body, None))
    }

    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.prefix)
    }

    fn head(&self, name: &str) -> Res {
        self.req("HEAD", &self.path(name), &[], b"")
    }

    fn head_target(&self, target: &str) -> Res {
        self.req("HEAD", target, &[], b"")
    }

    /// One `PUT PREFIX/NAME.part` exactly as `upload.rs` writes it.
    fn put_chunk(&self, name: &str, first: usize, data: &[u8], total: usize) -> Res {
        let last = first + data.len() - 1;
        let range = format!("bytes {first}-{last}/{total}");
        let sha = sha256::hex_digest(data);
        self.req(
            "PUT",
            &self.path(&format!("{name}.part")),
            &[("Content-Range", range.as_str()), ("Content-SHA256", sha.as_str())],
            data,
        )
    }

    fn finalize(&self, name: &str, total: usize, sha: &str) -> Res {
        self.req(
            "POST",
            &self.path(&format!("{name}.finalize")),
            &[("X-Upload-Total", total.to_string().as_str()), ("X-Upload-Sha256", sha)],
            b"",
        )
    }

    fn put_sidecar(&self, base: &str, body: &[u8]) -> Res {
        let sha = sha256::hex_digest(body);
        self.req(
            "PUT",
            &self.path(&format!("{base}.json")),
            &[("Content-SHA256", sha.as_str())],
            body,
        )
    }

    fn delete(&self, name: &str) -> Res {
        self.req("DELETE", &self.path(&format!("{name}.part")), &[], b"")
    }
}

fn parse(buf: &[u8]) -> Option<Res> {
    let pos = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next()?;
    let mut parts = status.splitn(3, ' ');
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/1.") {
        return None;
    }
    let code: u16 = parts.next()?.parse().ok()?;
    let reason = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for l in lines {
        if let Some(i) = l.find(':') {
            headers.push((l[..i].trim().to_string(), l[i + 1..].trim().to_string()));
        }
    }
    let declared = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let rest = &buf[pos + 4..];
    let body = rest[..rest.len().min(declared)].to_vec();
    Some(Res {
        code,
        reason,
        headers,
        body,
    })
}

/// Deterministic bytes, so a resumed or retried transfer can be compared
/// exactly and a truncated one is obvious.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) ^ 0x5a) as u8).collect()
}

const NAME: &str = "20261108-120000.h264";

// ---- the happy path -------------------------------------------------------

#[test]
fn video_and_sidecar_end_to_end() {
    let t = start("happy");
    let c = t.client();
    let data = payload(700);
    let sha = sha256::hex_digest(&data);

    // Handler 1 and 2 on a file nobody has seen.
    assert_eq!(c.head(NAME).code, 404);
    let r = c.head(&format!("{NAME}.part"));
    assert_eq!(r.code, 404);

    // Handler 3: three chunks, each answered with the offset it leaves behind.
    let r = c.put_chunk(NAME, 0, &data[..300], 700);
    assert_eq!(r.code, 200, "{r:?}");
    assert_eq!(r.u("x-upload-offset"), Some(300));
    let r = c.put_chunk(NAME, 300, &data[300..600], 700);
    assert_eq!(r.u("x-upload-offset"), Some(600));
    let r = c.put_chunk(NAME, 600, &data[600..], 700);
    assert_eq!(r.u("x-upload-offset"), Some(700));

    // Handler 2 now agrees about how much we hold.
    assert_eq!(c.head(&format!("{NAME}.part")).u("x-upload-offset"), Some(700));

    // Handler 4: commit, and the echo must be the sha of what we stored.
    let r = c.finalize(NAME, 700, &sha);
    assert_eq!(r.code, 200, "{r:?}");
    assert_eq!(r.h("x-stored-sha256"), Some(sha.as_str()));
    assert_eq!(r.u("x-stored-bytes"), Some(700));

    // The `.part` is gone and the segment is byte-exact.
    assert_eq!(t.entries(), vec![NAME.to_string()]);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);

    // Handler 1 answers from the file on disk now.
    let r = c.head(NAME);
    assert_eq!(r.code, 200);
    assert_eq!(r.h("x-stored-sha256"), Some(sha.as_str()));
    assert_eq!(r.u("x-stored-bytes"), Some(700));
    // A HEAD carries no body: the camera trusts Content-Length and would block.
    assert_eq!(r.h("content-length"), Some("0"));
    assert!(r.body.is_empty());

    // A repeated commit (the camera lost the reply) is a 200 with the same sha.
    let r = c.finalize(NAME, 700, &sha);
    assert_eq!(r.code, 200);
    assert_eq!(r.h("x-stored-sha256"), Some(sha.as_str()));

    // Handler 5: the sidecar, whole.
    let json = br#"{"file":"20261108-120000.h264","bytes":700,"cut_short":false}"#;
    let r = c.put_sidecar("20261108-120000", json);
    assert_eq!(r.code, 200, "{r:?}");
    assert_eq!(
        r.h("x-stored-sha256"),
        Some(sha256::hex_digest(json).as_str())
    );
    assert_eq!(fs::read(t.rec().join("20261108-120000.json")).unwrap(), json);
    // Idempotent: sent again next pass if a reply went missing.
    assert_eq!(c.put_sidecar("20261108-120000", json).code, 200);

    // The reset handler, after the fact, is a harmless 200.
    let r = c.delete(NAME);
    assert_eq!(r.code, 200);
    assert_eq!(r.u("x-upload-offset"), Some(0));
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
}

#[test]
fn a_second_segment_does_not_disturb_the_first() {
    let t = start("two");
    let c = t.client();
    for (i, name) in ["20261108-120000.h264", "20261108-120500.h264"].iter().enumerate() {
        let data = payload(400 + i * 100);
        assert_eq!(c.put_chunk(name, 0, &data, data.len()).code, 200);
        let sha = sha256::hex_digest(&data);
        assert_eq!(c.finalize(name, data.len(), &sha).code, 200);
    }
    assert_eq!(t.entries().len(), 2);
}

// ---- resume and retries ---------------------------------------------------

#[test]
fn resume_after_a_dead_connection_continues_from_our_offset() {
    let t = start("resume");
    let c = t.client();
    let data = payload(1000);
    let sha = sha256::hex_digest(&data);

    assert_eq!(c.put_chunk(NAME, 0, &data[..400], 1000).code, 200);
    assert_eq!(c.put_chunk(NAME, 400, &data[400..800], 1000).code, 200);

    // The camera goes away here (LTE drop) and comes back in a new pass. It
    // asks handler 2 and adopts our answer.
    let r = c.head(&format!("{NAME}.part"));
    assert_eq!(r.u("x-upload-offset"), Some(800));
    assert_eq!(r.u("x-stored-bytes"), Some(800));

    assert_eq!(c.put_chunk(NAME, 800, &data[800..], 1000).code, 200);
    let r = c.finalize(NAME, 1000, &sha);
    assert_eq!(r.code, 200);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
}

#[test]
fn a_chunk_retried_at_the_same_offset_is_a_409_not_a_duplicate() {
    // The bug this exists for: appending a retried chunk twice, which produces
    // a file that hashes wrong and, worse, leaves the camera's offset and ours
    // disagreeing forever.
    let t = start("retry-chunk");
    let c = t.client();
    let data = payload(700);
    let sha = sha256::hex_digest(&data);

    assert_eq!(c.put_chunk(NAME, 0, &data[..300], 700).code, 200);
    let r = c.put_chunk(NAME, 0, &data[..300], 700);
    assert_eq!(r.code, 409, "{r:?}");
    assert_eq!(r.u("x-upload-offset"), Some(300));
    assert_eq!(c.head(&format!("{NAME}.part")).u("x-upload-offset"), Some(300));

    // A commit with the wrong total also reports the real offset, so the client
    // can carry on instead of restarting.
    let r = c.finalize(NAME, 700, &sha);
    assert_eq!(r.code, 409);
    assert_eq!(r.u("x-upload-offset"), Some(300));

    assert_eq!(c.put_chunk(NAME, 300, &data[300..600], 700).code, 200);
    assert_eq!(c.put_chunk(NAME, 600, &data[600..], 700).code, 200);
    assert_eq!(c.finalize(NAME, 700, &sha).code, 200);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
}

#[test]
fn an_out_of_order_chunk_is_told_where_to_start() {
    let t = start("outoforder");
    let c = t.client();
    let data = payload(900);
    let sha = sha256::hex_digest(&data);

    // Chunk 2 first: nothing held, so the answer is "offset 0", not a write.
    let r = c.put_chunk(NAME, 600, &data[600..], 900);
    assert_eq!(r.code, 409);
    assert_eq!(r.u("x-upload-offset"), Some(0));
    assert_eq!(c.head(&format!("{NAME}.part")).code, 404);

    assert_eq!(c.put_chunk(NAME, 0, &data[..300], 900).code, 200);
    // Now the same chunk 2 is still out of order (we hold 300).
    let r = c.put_chunk(NAME, 600, &data[600..], 900);
    assert_eq!(r.code, 409);
    assert_eq!(r.u("x-upload-offset"), Some(300));

    assert_eq!(c.put_chunk(NAME, 300, &data[300..600], 900).code, 200);
    assert_eq!(c.put_chunk(NAME, 600, &data[600..], 900).code, 200);
    assert_eq!(c.finalize(NAME, 900, &sha).code, 200);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
}

#[test]
fn a_client_that_dies_mid_chunk_leaves_nothing_behind() {
    let t = start("midchunk");
    let c = t.client();
    let data = payload(900);

    // Head plus a third of the body, then an RST (the way a dead LTE link or a
    // killed process shows up).
    let mut head = format!(
        "PUT /ingest/{NAME}.part HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         X-Cam-Token: {TOKEN}\r\nContent-Range: bytes 0-299/900\r\nContent-SHA256: {}\r\n\
         Content-Length: 300\r\n\r\n",
        sha256::hex_digest(&data[..300])
    )
    .into_bytes();
    head.extend_from_slice(&data[..100]);
    {
        let mut s = TcpStream::connect(t.addr).expect("connect");
        let _ = s.write_all(&head);
        let _ = s.flush();
        // Half-sent body, then gone: the server sees a short read and must not
        // append. (`set_linger` is still unstable in this toolchain, so this is
        // a FIN rather than an RST; either way the body never completes.)
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    thread::sleep(Duration::from_millis(150));

    // Not one byte: a chunk is buffered and verified before it is appended, so
    // a half-sent chunk cannot poison the resume point.
    assert_eq!(c.head(&format!("{NAME}.part")).code, 404);
    assert!(t.entries().is_empty(), "{:?}", t.entries());

    // The retry then works from zero.
    assert_eq!(c.put_chunk(NAME, 0, &data[..300], 900).code, 200);
    assert_eq!(c.put_chunk(NAME, 300, &data[300..], 900).code, 200);
    assert_eq!(c.finalize(NAME, 900, &sha256::hex_digest(&data)).code, 200);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
}

#[test]
fn a_part_holding_more_than_the_file_can_be_reset() {
    // The story from `upload.rs`: the receiver claims an offset beyond the
    // local file's length. The camera sends DELETE and starts over; if we
    // refuse, it keeps the file forever.
    let t = start("reset");
    let c = t.client();
    let big = payload(900);
    let data = payload(700);

    assert_eq!(c.put_chunk(NAME, 0, &big[..900], 900).code, 200);
    assert_eq!(c.head(&format!("{NAME}.part")).u("x-upload-offset"), Some(900));

    let r = c.delete(NAME);
    assert_eq!(r.code, 200);
    assert_eq!(r.u("x-upload-offset"), Some(0));
    assert_eq!(c.head(&format!("{NAME}.part")).code, 404);
    // Nothing to commit after a reset.
    assert_eq!(c.finalize(NAME, 700, &sha256::hex_digest(&data)).code, 404);

    assert_eq!(c.put_chunk(NAME, 0, &data, 700).code, 200);
    assert_eq!(c.finalize(NAME, 700, &sha256::hex_digest(&data)).code, 200);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);

    // DELETE on a name with nothing held is still a 200 (idempotent).
    assert_eq!(c.delete("20990101-000000.h264").code, 200);
}

// ---- integrity ------------------------------------------------------------

#[test]
fn a_chunk_with_the_wrong_sha_is_refused_and_writes_nothing() {
    let t = start("chunk-sha");
    let c = t.client();
    let data = payload(300);
    let bogus = "0".repeat(64);
    let r = c.req(
        "PUT",
        &c.path(&format!("{NAME}.part")),
        &[("Content-Range", "bytes 0-299/300"), ("Content-SHA256", bogus.as_str())],
        &data,
    );
    assert_eq!(r.code, 400, "{r:?}");
    assert_eq!(c.head(&format!("{NAME}.part")).code, 404);
    assert!(t.entries().is_empty());

    // A chunk with no Content-SHA256 at all is a protocol violation: the camera
    // always sends one, and accepting an unhashed chunk would mean trusting a
    // link we cannot see.
    let r = c.req(
        "PUT",
        &c.path(&format!("{NAME}.part")),
        &[("Content-Range", "bytes 0-299/300")],
        &data,
    );
    assert_eq!(r.code, 400);
    assert!(t.entries().is_empty());
}

#[test]
fn the_commit_echoes_what_it_stored_not_what_was_declared() {
    // The whole reason the camera deletes: the receiver hashes its own bytes.
    // If this server echoed the header, a corrupted transfer would delete the
    // only copy of the footage.
    let t = start("commit-sha");
    let c = t.client();
    let data = payload(700);
    let real = sha256::hex_digest(&data);
    let wrong = "a".repeat(64);

    assert_eq!(c.put_chunk(NAME, 0, &data, 700).code, 200);
    let r = c.finalize(NAME, 700, &wrong);
    assert_eq!(r.code, 422, "{r:?}");
    assert_eq!(r.h("x-stored-sha256"), Some(real.as_str()));

    // The bytes we hold are kept: the camera retries, it does not restart.
    assert_eq!(c.head(&format!("{NAME}.part")).u("x-upload-offset"), Some(700));
    assert!(!t.rec().join(NAME).exists());

    // And the honest commit then works.
    let r = c.finalize(NAME, 700, &real);
    assert_eq!(r.code, 200);
    assert_eq!(r.h("x-stored-sha256"), Some(real.as_str()));
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
}

#[test]
fn a_sidecar_with_the_wrong_sha_is_refused_and_writes_nothing() {
    let t = start("json-sha");
    let c = t.client();
    let json = br#"{"bytes":700}"#;
    let r = c.req(
        "PUT",
        &c.path("20261108-120000.json"),
        &[("Content-SHA256", "b".repeat(64).as_str())],
        json,
    );
    assert_eq!(r.code, 400);
    assert!(t.entries().is_empty());
    assert_eq!(c.put_sidecar("20261108-120000", json).code, 200);
    assert_eq!(t.entries(), vec!["20261108-120000.json".to_string()]);
}

// ---- auth and path safety -------------------------------------------------

#[test]
fn every_request_needs_the_token() {
    let t = start("auth");
    let mut c = t.client();

    c.token = None;
    let r = c.head_target("/ingest/20261108-120000.h264");
    assert_eq!(r.code, 401, "{r:?}");
    assert!(r.h("www-authenticate").is_some());
    // A PUT without the token must not create anything, even though its body
    // is well formed.
    let data = payload(300);
    let r = c.put_chunk(NAME, 0, &data, 300);
    assert_eq!(r.code, 401);
    assert!(t.entries().is_empty());

    c.token = Some("not-the-token".to_string());
    assert_eq!(c.head_target("/ingest/20261108-120000.h264").code, 403);
    assert_eq!(c.put_chunk(NAME, 0, &data, 300).code, 403);
    assert!(t.entries().is_empty());

    c.token = Some(TOKEN.to_string());
    assert_eq!(c.head_target("/ingest/20261108-120000.h264").code, 404);
}

#[test]
fn an_empty_presented_token_is_not_a_match() {
    // An empty X-Cam-Token header must not equal an empty configured token even
    // if one somehow got past the CLI: the server refuses to start without a
    // token, and this pins the comparison down.
    let t = start("empty-token");
    let mut c = t.client();
    c.token = Some(String::new());
    assert_eq!(c.head("x.h264").code, 403);
}

#[test]
fn traversal_and_paths_outside_the_prefix_are_refused() {
    let t = start("traversal");
    let c = t.client();

    let cases = [
        "/ingest/../../etc/passwd",
        "/ingest/..%2f..%2fetc%2fpasswd",
        "/ingest/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/ingest/%2E%2E/secret.h264",
        "/ingest/sub/dir/20261108-120000.h264",
        "/ingest//20261108-120000.h264",
        "/ingest/",
        "/ingest",
        "/ingest/20261108-120000.h264%00",
        "/ingest/20261108-120000.h264?x=1",
        "/ingest/20261108-120000.h264#frag",
        "/ingest/.hidden/../20261108-120000.h264",
        "/ingest/.hidden.h264.part",
        "/etc/passwd",
        "//ingest/20261108-120000.h264",
        "/ingest/..\\..\\etc\\passwd",
        "/ingestfoo/20261108-120000.h264",
        "/ingest/20261108%2D120000.h264%2Fx",
    ];
    for target in cases {
        let r = c.head_target(target);
        assert!(
            r.code == 400 || r.code == 404,
            "{target} → {} {}",
            r.code,
            r.reason
        );
    }
    // Nothing was created, and nothing appeared next to the root either.
    assert!(t.entries().is_empty(), "{:?}", t.entries());
    assert!(!t.root.join("..").join("etc").exists());
    // A percent-encoded *dash* is still a legitimate name and must not be
    // conflated with the refusals above.
    assert_eq!(c.head_target("/ingest/20261108%2D120000.h264").code, 404);
    assert_eq!(c.head_target("/ingest/20261108-120000.h264").code, 404);
}

#[test]
fn the_root_prefix_also_works() {
    // `base_url` without a path makes the camera send "/NAME".
    let root = temp_root("rootprefix");
    let cfg = Cfg {
        listen: "127.0.0.1:0".to_string(),
        root: root.clone(),
        prefix: String::new(),
        token: TOKEN.as_bytes().to_vec(),
        max_conns: 4,
        io_timeout: Duration::from_secs(5),
        max_chunk: 1 << 20,
        max_json: 4096,
        quiet: true,
    };
    let srv = Server::bind(cfg).expect("bind");
    let addr = srv.local_addr().unwrap();
    let stop = srv.stop_handle();
    thread::spawn(move || {
        let _ = srv.serve_forever();
    });
    let t = Test {
        addr,
        root,
        stop,
        prefix: String::new(),
    };
    let c = t.client();
    let data = payload(500);
    assert_eq!(c.put_chunk(NAME, 0, &data, 500).code, 200);
    assert_eq!(c.finalize(NAME, 500, &sha256::hex_digest(&data)).code, 200);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
    // And a multi-segment path is not a file name under a root prefix, so the
    // two shapes cannot be confused.
    assert_eq!(c.head_target("/ingest/20261108-120000.h264").code, 400);
    assert_eq!(c.head_target("/20261108-120000.h264").code, 200);
}

// ---- abuse ----------------------------------------------------------------

#[test]
fn oversized_bodies_are_refused_before_they_are_read() {
    let t = start("oversize");
    let c = t.client();

    // 100 MB claimed in the head; the body is never sent. The answer has to
    // come from the header, or one request becomes an allocation attack.
    let sha = "c".repeat(64);
    let r = c.must(&c.build(
        "PUT",
        &c.path(&format!("{NAME}.part")),
        &[
            ("Content-Range", "bytes 0-99999999/100000000"),
            ("Content-SHA256", sha.as_str()),
        ],
        b"",
        Some(100_000_000),
    ));
    assert_eq!(r.code, 413, "{r:?}");
    assert!(t.entries().is_empty());

    // A sidecar that is not a sidecar.
    let r = c.must(&c.build(
        "PUT",
        &c.path("20261108-120000.json"),
        &[("Content-SHA256", sha.as_str())],
        b"",
        Some(1_000_000),
    ));
    assert_eq!(r.code, 413);

    // A total that is not a video segment.
    let r = c.finalize(NAME, 4 << 30, &sha256::hex_digest(b""));
    assert_eq!(r.code, 413);
}

#[test]
fn a_lying_content_length_is_refused() {
    let t = start("liar");
    let c = t.client();
    let data = payload(300);
    let sha = sha256::hex_digest(&data);

    // Content-Length disagrees with Content-Range.
    let r = c.must(&c.build(
        "PUT",
        &c.path(&format!("{NAME}.part")),
        &[("Content-Range", "bytes 0-299/300"), ("Content-SHA256", sha.as_str())],
        &data,
        Some(400),
    ));
    assert_eq!(r.code, 400, "{r:?}");
    assert!(t.entries().is_empty());

    // A range that ends past its own total.
    let r = c.req(
        "PUT",
        &c.path(&format!("{NAME}.part")),
        &[("Content-Range", "bytes 0-299/200"), ("Content-SHA256", sha.as_str())],
        &data,
    );
    assert_eq!(r.code, 400);

    // A range that does not parse at all.
    for range in ["bytes */300", "0-299/300", "bytes 299-0/300", "bytes +0-299/300", "bytes 0-299"] {
        let r = c.req(
            "PUT",
            &c.path(&format!("{NAME}.part")),
            &[("Content-Range", range), ("Content-SHA256", sha.as_str())],
            &data,
        );
        assert_eq!(r.code, 400, "{range} → {}", r.code);
    }
    assert!(t.entries().is_empty());
}

#[test]
fn junk_on_the_socket_does_not_take_the_server_down() {
    let t = start("junk");
    let c = t.client();

    // Not a request line at all.
    let r = c.must(b"GARBAGE\r\n\r\n");
    assert_eq!(r.code, 400);
    // A head that never ends.
    let mut huge = vec![b'A'; 20 * 1024];
    huge.extend_from_slice(b"\r\n\r\n");
    let r = c.must(&huge);
    assert_eq!(r.code, 431);
    // Binary noise that happens to contain a CRLF.
    let r = c.must(b"\x00\x01\x02\r\n\r\n");
    assert_eq!(r.code, 400);
    // Two Content-Length headers that disagree.
    let r = c.must(
        format!(
            "HEAD /ingest/{NAME} HTTP/1.1\r\nX-Cam-Token: {TOKEN}\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"
        )
        .as_bytes(),
    );
    assert_eq!(r.code, 400);

    // An unknown method is a 405 with the verb list, not a dropped connection.
    let r = c.req("GET", &c.path(NAME), &[], b"");
    assert_eq!(r.code, 405);
    assert!(r.h("allow").is_some());
    let r = c.req("PUT", &c.path(NAME), &[("Content-SHA256", "d".repeat(64).as_str())], b"abc");
    assert_eq!(r.code, 405);
    let r = c.req("POST", &c.path(&format!("{NAME}.part")), &[], b"");
    assert_eq!(r.code, 405);

    // And it is still serving.
    assert_eq!(c.head(NAME).code, 404);
    let data = payload(300);
    assert_eq!(c.put_chunk(NAME, 0, &data, 300).code, 200);
}

#[test]
fn simultaneous_retries_of_one_chunk_exactly_one_wins() {
    // A slow chunk whose reply is lost gets retried *while the first request is
    // still being handled*. Both must not append.
    let t = start("race");
    let data = payload(1000);
    let sha = sha256::hex_digest(&data[..500]);
    let body = t.client().build(
        "PUT",
        &t.client().path(&format!("{NAME}.part")),
        &[("Content-Range", "bytes 0-499/1000"), ("Content-SHA256", sha.as_str())],
        &data[..500],
        None,
    );

    let addr = t.addr;
    let mut handles = Vec::new();
    for _ in 0..2 {
        let b = body.clone();
        handles.push(thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(20))).ok();
            s.write_all(&b).unwrap();
            let mut out = Vec::new();
            let _ = s.read_to_end(&mut out);
            parse(&out).map(|r| r.code).unwrap_or(0)
        }));
    }
    let mut codes: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    codes.sort();
    assert_eq!(codes, vec![200, 409], "both requests must not append");

    let c = t.client();
    assert_eq!(c.head(&format!("{NAME}.part")).u("x-upload-offset"), Some(500));
    assert_eq!(c.put_chunk(NAME, 500, &data[500..], 1000).code, 200);
    assert_eq!(c.finalize(NAME, 1000, &sha256::hex_digest(&data)).code, 200);
    assert_eq!(fs::read(t.rec().join(NAME)).unwrap(), data);
}

#[test]
fn two_names_upload_at_once() {
    let t = start("parallel");
    let addr = t.addr;
    let mut handles = Vec::new();
    for i in 0..2u8 {
        let name = format!("20261108-12{:02}00.h264", i);
        let data: Vec<u8> = (0..2000u32).map(|j| (j.wrapping_mul(7) ^ i as u32) as u8).collect();
        handles.push(thread::spawn(move || {
            let mut ok = true;
            for (n, chunk) in data.chunks(700).enumerate() {
                let first = n * 700;
                let head = format!(
                    "PUT /ingest/{name}.part HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
                     X-Cam-Token: {TOKEN}\r\nContent-Range: bytes {first}-{}/{}\r\n\
                     Content-SHA256: {}\r\nContent-Length: {}\r\n\r\n",
                    first + chunk.len() - 1,
                    data.len(),
                    sha256::hex_digest(chunk),
                    chunk.len()
                );
                let mut bytes = head.into_bytes();
                bytes.extend_from_slice(chunk);
                let mut sock = TcpStream::connect(addr).unwrap();
                sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
                sock.write_all(&bytes).unwrap();
                let mut out = Vec::new();
                let _ = sock.read_to_end(&mut out);
                ok &= parse(&out).map(|r| r.ok()).unwrap_or(false);
            }
            let head = format!(
                "POST /ingest/{name}.finalize HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
                 X-Cam-Token: {TOKEN}\r\nX-Upload-Total: {}\r\nX-Upload-Sha256: {}\r\n\
                 Content-Length: 0\r\n\r\n",
                data.len(),
                sha256::hex_digest(&data)
            );
            let mut sock = TcpStream::connect(addr).unwrap();
            sock.write_all(head.as_bytes()).unwrap();
            let mut out = Vec::new();
            let _ = sock.read_to_end(&mut out);
            let committed = ok && parse(&out).map(|r| r.ok()).unwrap_or(false);
            assert!(committed, "{name} did not commit");
            (name, data)
        }));
    }
    for h in handles {
        let (name, data) = h.join().unwrap();
        assert_eq!(fs::read(t.rec().join(&name)).unwrap(), data, "{name}");
    }
}

#[test]
fn expect_100_continue_is_answered_before_the_body() {
    // curl sends `Expect: 100-continue` for anything bigger than a kilobyte and
    // then waits up to a second for permission. A server that keeps quiet still
    // works, one second at a time; a server that answers lets the body flow.
    let t = start("continue");
    let data = payload(300);
    let mut s = TcpStream::connect(t.addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let head = format!(
        "PUT /ingest/{NAME}.part HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         X-Cam-Token: {TOKEN}\r\nExpect: 100-continue\r\n\
         Content-Range: bytes 0-299/300\r\nContent-SHA256: {}\r\nContent-Length: 300\r\n\r\n",
        sha256::hex_digest(&data)
    );
    s.write_all(head.as_bytes()).unwrap();
    // The interim response must arrive before a byte of body has been sent.
    let mut interim = Vec::new();
    let mut one = [0u8; 1];
    while !interim.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut one).expect("read interim");
        assert_eq!(n, 1, "server closed before answering the expectation");
        interim.push(one[0]);
        assert!(interim.len() < 256);
    }
    assert!(
        interim.starts_with(b"HTTP/1.1 100"),
        "{:?}",
        String::from_utf8_lossy(&interim)
    );
    // Now the body, then the real answer.
    s.write_all(&data).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let r = parse(&out).expect("final response");
    assert_eq!(r.code, 200, "{r:?}");
    assert_eq!(r.u("x-upload-offset"), Some(300));
}

#[test]
fn a_chunked_request_body_is_refused_rather_than_buffered() {
    let t = start("chunked");
    let c = t.client();
    for (method, target) in [
        ("PUT", format!("/ingest/{NAME}.part")),
        ("PUT", "/ingest/20261108-120000.json".to_string()),
    ] {
        let req = format!(
            "{method} {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Cam-Token: {TOKEN}\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"
        );
        let r = c.must(req.as_bytes());
        assert_eq!(r.code, 411, "{method} {target} → {r:?}");
    }
    assert!(t.entries().is_empty());
}

// ---- unit tests -----------------------------------------------------------

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
    // Incremental == one-shot across every awkward block boundary.
    let data = payload(1000);
    for split in [1usize, 55, 56, 63, 64, 65, 127, 128, 500] {
        let mut h = sha256::Sha256::new();
        h.update(&data[..split]);
        h.update(&data[split..]);
        assert_eq!(sha256::to_hex(&h.finish()), sha256::hex_digest(&data));
    }
}

#[test]
fn resolve_maps_the_protocol_paths_and_nothing_else() {
    let name = "20261108-120000.h264";
    assert_eq!(
        store::resolve("/ingest", &format!("/ingest/{name}")),
        Ok(Op::Segment(name.to_string()))
    );
    assert_eq!(
        store::resolve("/ingest", &format!("/ingest/{name}.part")),
        Ok(Op::Part(name.to_string()))
    );
    assert_eq!(
        store::resolve("/ingest", &format!("/ingest/{name}.finalize")),
        Ok(Op::Finalize(name.to_string()))
    );
    assert_eq!(
        store::resolve("/ingest", "/ingest/20261108-120000.json"),
        Ok(Op::Sidecar("20261108-120000".to_string()))
    );
    // No prefix at all.
    assert_eq!(
        store::resolve("", &format!("/{name}")),
        Ok(Op::Segment(name.to_string()))
    );
    // Outside the prefix, and the near-miss that a naive starts_with accepts.
    assert_eq!(store::resolve("/ingest", &format!("/ingestfoo/{name}")), Err(ResolveErr::NotOurs));
    assert_eq!(store::resolve("/ingest", &format!("/{name}")), Err(ResolveErr::NotOurs));
    // Not a shape we serve.
    assert_eq!(store::resolve("/ingest", "/ingest/notes.txt"), Err(ResolveErr::NotOurs));
    assert_eq!(store::resolve("/ingest", "/ingest/x.json.part"), Err(ResolveErr::Bad("bad segment name")));
    // Bad names never reach a path.
    for bad in ["/ingest/../x.h264", "/ingest/.x.h264", "/ingest/x%2Fy.h264", "/ingest/x.h264%00"] {
        assert!(store::resolve("/ingest", bad).is_err(), "{bad} should be refused");
    }
    // Percent-encoding is decoded, then judged.
    assert_eq!(
        store::resolve("/ingest", "/ingest/20261108%2D120000.h264"),
        Ok(Op::Segment(name.to_string()))
    );
}

#[test]
fn a_name_can_never_contain_a_separator() {
    // The traversal defence, stated as a property: whatever survives
    // `resolve` must be a single path component.
    for target in [
        "/ingest/a/b.h264",
        "/ingest/a%2Fb.h264",
        "/ingest/..%5C..%5Cetc%5Cpasswd.h264",
        "/ingest/%2e%2e%2f%2e%2e%2fx.h264",
    ] {
        match store::resolve("/ingest", target) {
            Err(_) => {}
            Ok(op) => {
                let name = match op {
                    Op::Segment(n) | Op::Part(n) | Op::Finalize(n) | Op::Sidecar(n) => n,
                };
                assert!(!name.contains('/') && !name.contains('\\'), "{target} → {name}");
                assert!(!name.contains(".."), "{target} → {name}");
            }
        }
    }
}

#[test]
fn prefixes_are_normalised_the_way_an_operator_types_them() {
    assert_eq!(server::normalise_prefix("/ingest"), Ok("/ingest".to_string()));
    assert_eq!(server::normalise_prefix("ingest"), Ok("/ingest".to_string()));
    assert_eq!(server::normalise_prefix("/ingest/"), Ok("/ingest".to_string()));
    assert_eq!(server::normalise_prefix(""), Ok(String::new()));
    assert_eq!(server::normalise_prefix("/"), Ok(String::new()));
    assert!(server::normalise_prefix("/a//b").is_err());
}

#[test]
fn content_range_parsing_is_strict() {
    use server::parse_content_range as p;
    assert_eq!(p("bytes 0-299/300"), Some((0, 299, 300)));
    assert_eq!(p("bytes 1024-2047/3000"), Some((1024, 2047, 3000)));
    assert_eq!(p("  bytes 0-0/1  "), Some((0, 0, 1)));
    assert_eq!(p("bytes */300"), None);
    assert_eq!(p("0-299/300"), None);
    assert_eq!(p("bytes 0-299"), None);
    assert_eq!(p("bytes +0-299/300"), None);
    assert_eq!(p("bytes 0-299/1e3"), None);
    assert_eq!(p("bytes 0x10-0x20/64"), None);
    assert_eq!(p("bytes 10-20/20"), Some((10, 20, 20)));
}

