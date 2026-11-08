//! Just enough HTTP/1.1 to be the far end of the camera's uploader.
//!
//! Scope is deliberately the narrowest thing that can still be called a server:
//! one request per connection (the client sends `Connection: close` and opens a
//! fresh socket for every request), no keep-alive, no chunked bodies, no
//! pipelining, no `Expect: 100-continue`. Everything outside that is refused
//! with a status the camera understands rather than guessed at.
//!
//! Refusals are chosen so the *camera* does the right thing: it retries 5xx
//! with backoff, adopts `X-Upload-Offset` on 409/416, retries a bad chunk on
//! 400, and keeps the file on any other 4xx (see `camrtsp/src/upload.rs`).

use std::io::{self, BufRead, Read, Write};

use crate::VERSION;

/// A request head bigger than this is not ours (the camera's is ~250 bytes).
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// The camera sends 7 headers; anything past this is abuse.
pub const MAX_HEADERS: usize = 64;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    /// Parsed and only ever checked to be HTTP/1.x; kept for log lines.
    #[allow(dead_code)]
    pub version: String,
    /// Names lowercased, values trimmed.
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn header_u64(&self, name: &str) -> Option<u64> {
        self.header(name).and_then(|v| v.trim().parse::<u64>().ok())
    }

    /// `Content-Length` if it parses. Callers that need it treat its absence as
    /// a 411 rather than as zero, because a body we were not told about is a
    /// body we cannot trust.
    pub fn content_length(&self) -> Option<usize> {
        self.header("content-length")
            .and_then(|v| v.trim().parse::<usize>().ok())
    }
}

#[derive(Debug)]
pub enum HeadError {
    /// The peer said nothing (or went away). No reply is possible or wanted.
    Eof,
    /// Head exceeded `MAX_HEAD_BYTES`; answer 431.
    TooLarge,
    /// Malformed; answer 400 with this as `X-Error`.
    Bad(&'static str),
}

/// Read one request head. The body (if any) is left in `r` for the caller.
pub fn read_request<R: BufRead>(r: &mut R) -> Result<Request, HeadError> {
    let mut budget = MAX_HEAD_BYTES;
    let mut line = Vec::new();

    // Request line. A leading blank line is tolerated once (some clients send
    // a stray CRLF after a pipelined response); more than that is junk.
    let mut first = String::new();
    for attempt in 0..2 {
        if !next_line(r, &mut line, &mut budget)? {
            return Err(HeadError::Eof);
        }
        first = String::from_utf8_lossy(&line).into_owned();
        if !first.is_empty() {
            break;
        }
        if attempt == 1 {
            return Err(HeadError::Bad("empty request line"));
        }
    }

    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err(HeadError::Bad("request line has too many fields"));
    }
    if method.is_empty() || !method.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err(HeadError::Bad("bad method token"));
    }
    if target.is_empty() {
        return Err(HeadError::Bad("no request target"));
    }
    if !version.starts_with("HTTP/1.") {
        return Err(HeadError::Bad("only HTTP/1.x is spoken here"));
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_lengths: Vec<String> = Vec::new();
    loop {
        if !next_line(r, &mut line, &mut budget)? {
            return Err(HeadError::Bad("head ended without a blank line"));
        }
        if line.is_empty() {
            break;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            // Obsolete line folding: a second way to write the same header, and
            // therefore a way to smuggle one past a check. Refused.
            return Err(HeadError::Bad("folded header lines are not accepted"));
        }
        if headers.len() >= MAX_HEADERS {
            return Err(HeadError::TooLarge);
        }
        let colon = match line.iter().position(|&b| b == b':') {
            Some(i) => i,
            None => return Err(HeadError::Bad("header without a colon")),
        };
        let name = String::from_utf8_lossy(&line[..colon]).into_owned();
        if name.is_empty() || name.bytes().any(|c| c <= b' ' || c == 0x7f) {
            // Also catches "Name : value", the space-before-colon smuggling
            // trick, because the name would end in a space.
            return Err(HeadError::Bad("bad header name"));
        }
        let value = String::from_utf8_lossy(&line[colon + 1..])
            .trim()
            .to_string();
        if value.len() > 4096 {
            return Err(HeadError::TooLarge);
        }
        let lower = name.to_ascii_lowercase();
        if lower == "content-length" {
            content_lengths.push(value.clone());
        }
        headers.push((lower, value));
    }

    // Two different Content-Length headers is a request nobody honest sends.
    if content_lengths.len() > 1 {
        let first = content_lengths.first().map(|s| s.as_str()).unwrap_or("");
        if content_lengths.iter().any(|v| v != first) {
            return Err(HeadError::Bad("conflicting Content-Length headers"));
        }
    }

    Ok(Request {
        method: method.to_ascii_uppercase(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
    })
}

/// One CRLF (or LF) terminated line, with the terminator removed. `false` = EOF.
fn next_line<R: BufRead>(
    r: &mut R,
    buf: &mut Vec<u8>,
    budget: &mut usize,
) -> Result<bool, HeadError> {
    buf.clear();
    let n = match r.read_until(b'\n', buf) {
        Ok(n) => n,
        // A reset, or the read timeout on a socket that never sent a line.
        Err(_) => return Err(HeadError::Eof),
    };
    if n == 0 {
        return Ok(false);
    }
    if n > *budget {
        return Err(HeadError::TooLarge);
    }
    *budget -= n;
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    Ok(true)
}

/// `Expect: 100-continue`: the peer is waiting for permission before it sends
/// the body. Answering is cheaper than the second of silence every client
/// (curl, some proxies) otherwise waits for. Anything else in `Expect` is
/// ignored, which is what RFC 9110 asks a server that has nothing to promise.
pub fn maybe_continue<W: Write>(req: &Request, w: &mut W) -> io::Result<()> {
    if let Some(v) = req.header("expect") {
        if v.eq_ignore_ascii_case("100-continue") {
            w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            w.flush()?;
        }
    }
    Ok(())
}

/// Read exactly `len` bytes of body. The caller has already capped `len`.
pub fn read_body<R: Read>(r: &mut R, len: usize) -> io::Result<Vec<u8>> {
    let mut body = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        match r.read(&mut body[got..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed mid-body",
                ))
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(body)
}

/// A response. `code == 0` means "say nothing, just close": used when the peer
/// vanished mid-body and a reply would go nowhere.
#[derive(Clone, Debug)]
pub struct Response {
    pub code: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(code: u16, reason: &'static str) -> Response {
        Response {
            code,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn silent() -> Response {
        Response::new(0, "")
    }

    pub fn is_silent(&self) -> bool {
        self.code == 0
    }

    pub fn ok() -> Response {
        Response::new(200, "OK")
    }

    /// A refusal with a human-readable reason in `X-Error` (which the camera
    /// logs) and in the body (which is allowed, and ignored by the camera).
    pub fn err(code: u16, reason: &'static str, msg: &str) -> Response {
        let mut r = Response::new(code, reason);
        r.headers.push(("X-Error".to_string(), clip(msg)));
        r.body = format!("{msg}\n").into_bytes();
        r
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.to_string(), clip(&value.into())));
        self
    }

    /// `head_only` forces `Content-Length: 0` with no body, because the camera's
    /// reader takes a declared length at face value and would block waiting for
    /// a body that HEAD responses never carry.
    pub fn write_to<W: Write>(&self, w: &mut W, head_only: bool) -> io::Result<()> {
        let mut head = String::with_capacity(256);
        head.push_str("HTTP/1.1 ");
        head.push_str(&self.code.to_string());
        head.push(' ');
        head.push_str(self.reason);
        head.push_str("\r\nServer: camingest/");
        head.push_str(VERSION);
        head.push_str("\r\nConnection: close\r\n");
        let body_len = if head_only { 0 } else { self.body.len() };
        head.push_str("Content-Length: ");
        head.push_str(&body_len.to_string());
        head.push_str("\r\n");
        for (k, v) in &self.headers {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        w.write_all(head.as_bytes())?;
        if !head_only && !self.body.is_empty() {
            w.write_all(&self.body)?;
        }
        Ok(())
    }
}

/// Header values are ours, but a panic-free server does not rely on that: a
/// stray CRLF in a value would be header injection.
fn clip(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(200));
    for c in s.chars() {
        if c == '\r' || c == '\n' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out.truncate(200);
    out
}
