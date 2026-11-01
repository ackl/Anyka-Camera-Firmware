//! RTSP/1.0 message framing (RFC 2326 §4, §6, §7): a request parser that
//! never panics on hostile bytes, response rendering, and the mapping from a
//! request URL to the stream/track it names.

use std::fmt::Write as _;
use std::net::SocketAddr;

/// Longest request head we buffer. Real clients send well under 1 KB.
pub const MAX_HEAD: usize = 8 * 1024;
/// Longest request body. Only GET_/SET_PARAMETER carry one, and we
/// understand no parameters, so anything big is an attack or a mistake.
pub const MAX_BODY: usize = 4 * 1024;

pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    /// Header lines after the request line, unparsed.
    head: &'a str,
    pub body: &'a [u8],
}

impl<'a> Request<'a> {
    /// First header named `name` (case-insensitive), value trimmed.
    pub fn header(&self, name: &str) -> Option<&'a str> {
        header_in(self.head, name)
    }
}

fn header_in<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    for line in head.split('\n') {
        if let Some(i) = line.find(':') {
            if line[..i].trim().eq_ignore_ascii_case(name) {
                return Some(line[i + 1..].trim());
            }
        }
    }
    None
}

pub enum Parsed<'a> {
    /// Need more bytes.
    More,
    /// A complete request occupying the first `usize` bytes.
    Req(Request<'a>, usize),
    /// Unusable request: answer `code`, then drop `len` bytes. `len == 0`
    /// means the framing is lost and the connection has to close.
    Bad {
        code: u16,
        cseq: Option<&'a str>,
        len: usize,
    },
}

/// Where the head ends: (index of the blank line's first byte, body start).
/// Accepts CRLF and bare LF line ends.
fn head_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lim = buf.len().min(MAX_HEAD + 4);
    let mut i = 0;
    while i < lim {
        if buf[i] == b'\n' {
            if buf.get(i + 1) == Some(&b'\n') {
                return Some((i, i + 2));
            }
            if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
                return Some((i, i + 3));
            }
        }
        i += 1;
    }
    None
}

pub fn parse(buf: &[u8]) -> Parsed<'_> {
    let (end, body_at) = match head_end(buf) {
        Some(x) => x,
        None if buf.len() > MAX_HEAD => return Parsed::Bad { code: 400, cseq: None, len: 0 },
        None => return Parsed::More,
    };
    if end > MAX_HEAD {
        return Parsed::Bad { code: 400, cseq: None, len: 0 };
    }
    let head = match std::str::from_utf8(&buf[..end]) {
        Ok(h) => h,
        // No usable Content-Length either; assume there is no body.
        Err(_) => return Parsed::Bad { code: 400, cseq: None, len: body_at },
    };
    let (line, rest) = match head.find('\n') {
        Some(i) => (&head[..i], &head[i + 1..]),
        None => (head, ""),
    };
    let line = line.trim_end_matches('\r');
    let cseq = header_in(rest, "CSeq");
    // Content-Length first, so even a rejected request is skipped whole.
    let clen = match header_in(rest, "Content-Length") {
        None => 0,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n <= MAX_BODY => n,
            _ => return Parsed::Bad { code: 400, cseq, len: 0 },
        },
    };
    let total = body_at + clen;
    if buf.len() < total {
        return Parsed::More;
    }
    let bad = |code| Parsed::Bad { code, cseq, len: total };

    let mut parts = line.split(' ').filter(|s| !s.is_empty());
    let (method, url, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(u), Some(v), None) => (m, u, v),
        _ => return bad(400),
    };
    if !method.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'-') {
        return bad(400);
    }
    // Nothing that could split a header we echo (Content-Base) is allowed.
    if url.bytes().any(|b| b < 0x21 || b == 0x7f) {
        return bad(400);
    }
    if version != "RTSP/1.0" {
        return bad(if version.starts_with("RTSP/") { 505 } else { 400 });
    }
    for l in rest.split('\n') {
        let l = l.trim_end_matches('\r');
        // Empty and folded continuation lines are tolerated.
        if l.is_empty() || l.starts_with(' ') || l.starts_with('\t') {
            continue;
        }
        if !l.contains(':') {
            return bad(400);
        }
    }
    Parsed::Req(
        Request { method, url, head: rest, body: &buf[body_at..total] },
        total,
    )
}

pub fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        451 => "Parameter Not Understood",
        454 => "Session Not Found",
        455 => "Method Not Valid in This State",
        461 => "Unsupported Transport",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        505 => "RTSP Version Not Supported",
        _ => "Error",
    }
}

/// Render a response into `out` (cleared first). `extra` is zero or more
/// complete `Name: value\r\n` lines.
pub fn render(
    out: &mut Vec<u8>,
    code: u16,
    cseq: Option<&str>,
    server: &str,
    extra: &str,
    ctype: &str,
    body: &[u8],
) {
    let mut h = String::with_capacity(128 + extra.len());
    let _ = write!(h, "RTSP/1.0 {} {}\r\n", code, reason(code));
    // No CSeq in the request means nothing to echo; the stock camera answers
    // such requests too, so we do not turn them into 400s.
    if let Some(c) = cseq {
        let _ = write!(h, "CSeq: {}\r\n", c);
    }
    let _ = write!(h, "Server: {}\r\n", server);
    h.push_str(extra);
    if !body.is_empty() {
        let _ = write!(h, "Content-Type: {}\r\nContent-Length: {}\r\n", ctype, body.len());
    }
    h.push_str("\r\n");
    out.clear();
    out.extend_from_slice(h.as_bytes());
    out.extend_from_slice(body);
}

/// What a request URL points at.
#[derive(Debug, PartialEq)]
pub struct Target {
    /// `Some(i)` for a per-track control URL (`…/trackID=i`).
    pub track: Option<usize>,
    /// Aggregate URL with a trailing slash, for `Content-Base` and RTP-Info.
    pub base: String,
}

/// Map a request URL onto the one stream this process serves.
///
/// This server has exactly one stream, so *any* single-segment path names
/// it: `/vs0`, `/vs1`, `/video0.sdp`, and the bare `/` all get the same
/// stream (clients written for the stock camera ask for `/vs0`, ONVIF tools
/// for whatever the profile said, and none of them should need to know which
/// channel this process was started for). A second segment must be one of our
/// SDP control names (`trackID=N`). Anything deeper, or an unknown control,
/// is 404 (`None`).
pub fn target(url: &str, controls: &[&str], local: &SocketAddr) -> Option<Target> {
    let (auth, path): (String, &str) = match url.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("rtsp://") => {
            let rest = &url[7..];
            match rest.find('/') {
                Some(i) => (rest[..i].to_string(), &rest[i..]),
                None => (rest.to_string(), "/"),
            }
        }
        _ if url.starts_with('/') => (local.to_string(), url),
        _ => return None,
    };
    if auth.is_empty() {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let ctrl = |s: &str| controls.iter().position(|c| *c == s);
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    let (a, b) = (segs.next(), segs.next());
    if segs.next().is_some() {
        return None;
    }
    let (stream, track) = match (a, b) {
        (None, _) => ("", None),
        (Some(x), None) => match ctrl(x) {
            Some(i) => ("", Some(i)),
            None => (x, None),
        },
        (Some(x), Some(y)) => (x, Some(ctrl(y)?)),
    };
    let base = if stream.is_empty() {
        format!("rtsp://{}/", auth)
    } else {
        format!("rtsp://{}/{}/", auth, stream)
    };
    Some(Target { track, base })
}

/// The id part of a `Session:` header (`"abc;timeout=60"` → `"abc"`).
pub fn session_id(h: &str) -> &str {
    h.split(';').next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(buf: &[u8]) -> (String, String, usize) {
        match parse(buf) {
            Parsed::Req(r, n) => (r.method.to_string(), r.url.to_string(), n),
            Parsed::More => panic!("more"),
            Parsed::Bad { code, .. } => panic!("bad {}", code),
        }
    }

    #[test]
    fn parses_request_and_headers() {
        let b = b"OPTIONS rtsp://h/vs0 RTSP/1.0\r\nCSeq: 2\r\ncseq: 9\r\n\r\nNEXT";
        let (m, u, n) = req(b);
        assert_eq!((m.as_str(), u.as_str()), ("OPTIONS", "rtsp://h/vs0"));
        assert_eq!(&b[n..], b"NEXT");
        if let Parsed::Req(r, _) = parse(b) {
            assert_eq!(r.header("cseq"), Some("2"));
        }
        // bare LF and a body
        let b = b"SET_PARAMETER * RTSP/1.0\nCSeq: 1\nContent-Length: 3\n\nabcX";
        let (_, _, n) = req(b);
        assert_eq!(n, b.len() - 1);
    }

    #[test]
    fn partial_and_bad() {
        assert!(matches!(parse(b"OPTIONS rtsp://h/ RTSP/1.0\r\nCSe"), Parsed::More));
        assert!(matches!(
            parse(b"SET_PARAMETER * RTSP/1.0\r\nContent-Length: 10\r\n\r\nab"),
            Parsed::More
        ));
        match parse(b"DESCRIBE rtsp://h/vs0\r\nCSeq: 3\r\n\r\n") {
            Parsed::Bad { code: 400, cseq: Some("3"), len } => assert!(len > 0),
            _ => panic!("missing version must be 400"),
        }
        assert!(matches!(
            parse(b"PLAY rtsp://h/ RTSP/2.0\r\n\r\n"),
            Parsed::Bad { code: 505, .. }
        ));
        assert!(matches!(
            parse(b"GET / HTTP/1.1\r\n\r\n"),
            Parsed::Bad { code: 400, .. }
        ));
        assert!(matches!(
            parse(b"PLAY rtsp://h/ RTSP/1.0\r\nContent-Length: 99999\r\n\r\n"),
            Parsed::Bad { code: 400, len: 0, .. }
        ));
        let big = vec![b'A'; MAX_HEAD + 10];
        assert!(matches!(parse(&big), Parsed::Bad { len: 0, .. }));
    }

    #[test]
    fn parser_never_panics_on_noise() {
        let mut x: u32 = 0x1234_5678;
        let alphabet = b"OPTIONS DESCRIBE rtsp:// RTSP/1.0\r\n:CSeq Content-Length 0123456789\xff\x00$";
        for _ in 0..3000 {
            let mut v = Vec::new();
            let n = (x % 200) as usize;
            for _ in 0..n {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(alphabet[(x as usize) % alphabet.len()]);
            }
            let _ = parse(&v);
        }
    }

    #[test]
    fn targets() {
        let local: SocketAddr = "10.0.0.2:554".parse().unwrap();
        let c = ["trackID=0", "trackID=1"];
        let t = |u| target(u, &c, &local);
        assert_eq!(t("rtsp://h:1/vs0").unwrap(), Target { track: None, base: "rtsp://h:1/vs0/".into() });
        assert_eq!(t("rtsp://h:1/vs0/").unwrap().base, "rtsp://h:1/vs0/");
        assert_eq!(t("rtsp://h:1/video0.sdp?x=1").unwrap().base, "rtsp://h:1/video0.sdp/");
        assert_eq!(t("rtsp://h:1").unwrap().base, "rtsp://h:1/");
        assert_eq!(t("rtsp://h:1/").unwrap().track, None);
        assert_eq!(t("rtsp://h/vs0/trackID=1").unwrap().track, Some(1));
        assert_eq!(t("rtsp://h/trackID=0").unwrap(), Target { track: Some(0), base: "rtsp://h/".into() });
        assert_eq!(t("/vs1").unwrap().base, "rtsp://10.0.0.2:554/vs1/");
        assert!(t("rtsp://h/vs0/trackID=7").is_none());
        assert!(t("rtsp://h/a/b/c").is_none());
        assert!(t("rtsp://h/vs0/camera").is_none());
        assert!(t("*").is_none());
        assert!(t("http://h/vs0").is_none());
        assert!(t("rtsp:///vs0").is_none());
        assert!(t("rtsp://\u{e9}\u{e9}").is_some());
        assert!(t("rts\u{e9}").is_none());
    }
}
