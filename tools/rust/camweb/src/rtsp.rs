//! Minimal RTSP / RTP-over-TCP client for the Anyka AK3918 `ak_rtsp_demo`
//! server (YI CB211 stock firmware), written against **std only**.
//!
//! No crates, no async, no threads of its own: one `Session` owns one TCP
//! connection and is driven by the caller (poll `nal()` from a video thread,
//! call `keepalive()` every ~20 s from the same thread).
//!
//! Shape of the wire conversation with this particular server (all measured,
//! see `work/rtsp-rust-notes.md`):
//!
//! ```text
//! C: OPTIONS  rtsp://host:554/vs0                 -> 200, Public: SETUP,OPTIONS,...
//! C: DESCRIBE rtsp://host:554/vs0                 -> 200 + SDP (Content-Base .../vs0/)
//! C: SETUP    <base>/camera   Transport: ...interleaved=0-1  -> 200, Session: 00000006; timeout=60
//! C: SETUP    <base>/micphone Transport: ...interleaved=2-3  -> 200, same Session
//! C: PLAY     <base>                              -> 200, then '$' interleaved frames
//! ```
//!
//! Quirks this module deliberately tolerates:
//!
//! * `Session:` value is `"<id>; timeout=<secs>"` — the id has to be split off
//!   before it is echoed back.
//! * A trailing `/` on the request URL makes the server emit `Content-Base`
//!   with a doubled slash (`.../vs0//`); therefore the base for control URLs is
//!   built locally from the URL as given, never from `Content-Base`.
//! * The server answers `PLAY` even without `Range` or `Session` headers, but
//!   both are sent anyway because they are what the spec wants.
//! * Replies to a mid-stream `OPTIONS` arrive as text between `'$'` frames, so
//!   text and binary are demultiplexed on the same byte stream.
//! * RTCP arrives on the odd interleaved channel (`1`, `3`) and must be
//!   dropped without being parsed as RTP.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Refuse to assemble a single NAL larger than this (drop it instead).
const MAX_NAL: usize = 2 * 1024 * 1024;
/// Hard ceiling for the inbound byte buffer. Interleaved frames are <= 64 KiB,
/// so anything near this means the stream is not what we think it is.
const MAX_BUF: usize = 4 * 1024 * 1024;
/// After skipping this many non-video interleaved frames in one `nal()` call,
/// return `Ok(None)` so the caller keeps control of its loop.
const MAX_SKIPS: u32 = 32;
/// Read cap per syscall.
const CHUNK: usize = 16 * 1024;
/// How long `keepalive()` waits for its reply before giving up (non-fatal).
const KEEPALIVE_WAIT: Duration = Duration::from_millis(400);

const UA: &str = "camweb-rtsp/1.0";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One media section of the SDP, plus the interleaved channel pair assigned to
/// it by `SETUP` (`None` when it was not set up).
#[derive(Clone, Debug)]
pub struct Track {
    /// "video" | "audio"
    pub media: String,
    /// "H264", "PCMA", ...
    pub encoding: String,
    /// Clock rate from `a=rtpmap` (90000 for H.264, 8000 for PCMA).
    pub clock: u32,
    /// The SDP `a=control` value as advertised ("camera", "micphone", "*", or a
    /// full URL).
    pub control: String,
    /// The `a=fmtp` value with the payload-number prefix stripped, "" if absent.
    /// H.264 `sprop-parameter-sets=` lives here.
    pub fmtp: String,
    /// `(rtp, rtcp)` channel numbers as confirmed by the server's Transport.
    pub interleaved: Option<(u8, u8)>,
}

/// A live RTSP session; one TCP connection to one stream (`/vs0`, `/vs1`, ...).
pub struct Session {
    stream: TcpStream,
    /// Request URL as built from the argument (no trailing slash).
    url: String,
    /// `url` + "/" — base for relative `a=control` names.
    base: String,
    sdp: String,
    tracks: Vec<Track>,
    session_id: String,
    /// Server-advertised session timeout, from `Session: ...; timeout=N`.
    server_timeout: Option<u32>,
    cseq: u32,
    /// Unconsumed inbound bytes (partial interleaved frame / partial reply).
    buf: Vec<u8>,
    /// NALs produced by one packet but not yet returned (STAP-A, extras).
    pending: VecDeque<(Vec<u8>, bool)>,
    /// FU-A reassembly buffer.
    fu: Vec<u8>,
    /// Interleaved channel carrying the video track's RTP, once known.
    video_chan: Option<u8>,
    read_timeout: Duration,
    /// Consecutive keepalives that got no reply (diagnostic only).
    missed_keepalives: u32,
    closed: bool,
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

impl Session {
    /// OPTIONS -> DESCRIBE -> SETUP for each media in `setup_media` -> PLAY.
    pub fn connect(
        url: &str,
        setup_media: &[&str],
        io_timeout: Duration,
    ) -> io::Result<Session> {
        let (host, port, req_url) = split_url(url)?;
        let base = if req_url.ends_with('/') {
            req_url.clone()
        } else {
            format!("{}/", req_url)
        };

        let stream = tcp_connect(&host, port, io_timeout)?;
        stream.set_read_timeout(Some(io_timeout))?;
        stream.set_write_timeout(Some(io_timeout))?;
        let _ = stream.set_nodelay(true);

        let mut s = Session {
            stream,
            url: req_url,
            base,
            sdp: String::new(),
            tracks: Vec::new(),
            session_id: String::new(),
            server_timeout: None,
            cseq: 0,
            buf: Vec::new(),
            pending: VecDeque::new(),
            fu: Vec::new(),
            video_chan: None,
            read_timeout: io_timeout,
            missed_keepalives: 0,
            closed: false,
        };

        // OPTIONS / DESCRIBE. A failing OPTIONS is not fatal on this server
        // (it always answers 200), but a missing SDP is.
        let opt_url = s.url.clone();
        let _ = s.request("OPTIONS", &opt_url, &[]);

        let desc_url = s.url.clone();
        let desc = s.request(
            "DESCRIBE",
            &desc_url,
            &[("Accept", "application/sdp")],
        )?;
        if desc.status != 200 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "rtsp: DESCRIBE {} returned {}{}",
                    desc_url,
                    desc.status,
                    if desc.status == 401 {
                        " (server wants authentication; not supported)"
                    } else {
                        ""
                    }
                ),
            ));
        }
        s.sdp = String::from_utf8_lossy(&desc.body).into_owned();
        s.tracks = parse_sdp(&s.sdp);
        if s.tracks.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rtsp: SDP contained no media sections",
            ));
        }

        // SETUP every requested media, in SDP order, one channel pair each.
        let order: Vec<usize> = (0..s.tracks.len()).collect();
        let mut set_up = 0usize;
        for (seq, ti) in order.iter().enumerate() {
            let media = s.tracks[*ti].media.clone();
            if !setup_media.iter().any(|m| m.eq_ignore_ascii_case(&media)) {
                continue;
            }
            let chan = (seq as u8) * 2;
            let uri = join_control(&s.base, &s.tracks[*ti].control);
            let transport = format!(
                "RTP/AVP/TCP;unicast;interleaved={}-{}",
                chan,
                chan.saturating_add(1)
            );
            // `request` adds the Session header by itself once we have one.
            let extra: Vec<(&str, &str)> = vec![("Transport", &transport)];
            let resp = match s.request("SETUP", &uri, &extra) {
                Ok(r) => r,
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("rtsp: SETUP {} failed: {}", uri, e),
                    ))
                }
            };
            if resp.status != 200 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("rtsp: SETUP {} returned {}", uri, resp.status),
                ));
            }
            // Session id (first response wins; this server keeps one per
            // connection and echoes it for the second SETUP).
            if s.session_id.is_empty() {
                if let Some(v) = resp.header("Session") {
                    let (id, tmo) = parse_session_header(v);
                    s.session_id = id;
                    s.server_timeout = tmo;
                }
            }
            let interleaved = resp
                .header("Transport")
                .and_then(parse_interleaved)
                .unwrap_or_else(|| (chan, chan.saturating_add(1)));
            s.tracks[*ti].interleaved = Some(interleaved);
            if media.eq_ignore_ascii_case("video") && s.video_chan.is_none() {
                s.video_chan = Some(interleaved.0);
            }
            set_up += 1;
        }

        if set_up == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "rtsp: none of {:?} present in SDP (had {:?})",
                    setup_media,
                    s.tracks.iter().map(|t| t.media.as_str()).collect::<Vec<_>>()
                ),
            ));
        }

        // PLAY. Range is optional for this server but harmless.
        let play_url = s.base.clone();
        let extra: Vec<(&str, &str)> = vec![("Range", "npt=0.000-")];
        let resp = s.request("PLAY", &play_url, &extra)?;
        if resp.status != 200 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("rtsp: PLAY {} returned {}", play_url, resp.status),
            ));
        }
        Ok(s)
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Next H.264 NAL of the video track, Annex-B prefixed
    /// (`00 00 00 01` + NAL). `Ok(None)` when this call consumed something that
    /// produced no NAL (RTCP, audio channel, mid-fragment, dropped type) or when
    /// the read timed out. Never an error for "nothing arrived yet".
    pub fn nal(&mut self) -> io::Result<Option<Vec<u8>>> {
        Ok(self.nal_with_marker()?.map(|(nal, _marker)| nal))
    }

    /// Same as [`Session::nal`] but also reports the RTP marker bit of the
    /// packet that completed the NAL, i.e. "this NAL ends an access unit".
    /// Useful for frame counting; `nal()` ignores it.
    pub fn nal_with_marker(&mut self) -> io::Result<Option<(Vec<u8>, bool)>> {
        let mut skips: u32 = 0;
        let mut garbage: u32 = 0;
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Ok(Some(item));
            }
            if let Some(&b0) = self.buf.first() {
                if b0 == b'$' {
                    // Interleaved frame: '$' chan len[2BE] payload
                    if self.buf.len() < 4 {
                        // need the header
                    } else {
                        let chan = self.buf[1];
                        let len = ((self.buf[2] as usize) << 8) | self.buf[3] as usize;
                        if self.buf.len() < 4 + len {
                            // need the payload
                        } else {
                            let payload: Vec<u8> = self.buf[4..4 + len].to_vec();
                            self.buf.drain(..4 + len);
                            if Some(chan) == self.video_chan {
                                if let Some((pl, marker)) = rtp_split(&payload) {
                                    let pl = pl.to_vec();
                                    self.depacketize(&pl, marker);
                                }
                            } else {
                                // audio RTP or RTCP: never parse as video
                                skips += 1;
                                if skips >= MAX_SKIPS {
                                    return Ok(None);
                                }
                            }
                            continue;
                        }
                    }
                } else if self.buf.starts_with(b"RTSP/") {
                    // Text reply (e.g. to a keepalive OPTIONS) inserted between
                    // frames: swallow it whole, then keep going.
                    match parse_response(&self.buf) {
                        Ok(Some((_resp, used))) => {
                            self.buf.drain(..used);
                            continue;
                        }
                        // incomplete: fall through to read more
                        Ok(None) => {}
                        Err(_) => {
                            self.buf.drain(..1);
                            continue;
                        }
                    }
                } else {
                    // Stray byte(s): drop one and resynchronise.
                    self.buf.drain(..1);
                    garbage += 1;
                    if garbage > 4096 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "rtsp: cannot resynchronise on stream",
                        ));
                    }
                    continue;
                }
            } else {
                garbage = 0;
            }

            // Need more bytes.
            match self.fill() {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "rtsp: server closed the connection",
                    ))
                }
                Ok(_) => {}
                Err(ref e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    return Ok(None)
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// RTSP keepalive (`OPTIONS` with the `Session` header). Send this about
    /// every 20 s; the stock server advertises `timeout=60`.
    ///
    /// Deliberately shallow: it writes the request and then waits at most
    /// [`KEEPALIVE_WAIT`] for the reply. Anything the server sends while we wait
    /// (video frames, RTCP, the reply itself) is kept, and a reply that does not
    /// turn up in time is *not* an error — it will simply be discarded by
    /// [`Session::nal`] as stray text later. Measured on this camera the reply
    /// normally lands in ~37-90 ms, but mid-stream replies are occasionally
    /// dropped by the server altogether (1 of 10 observed); blocking on one
    /// stalled the video path for exactly the wait duration, so the wait is
    /// bounded and a miss only bumps [`Session::missed_keepalives`].
    /// Real connection errors (EOF, reset) are still propagated.
    pub fn keepalive(&mut self) -> io::Result<()> {
        if self.session_id.is_empty() || self.closed {
            return Ok(());
        }
        let cseq = self.next_cseq();
        let req = format!(
            "OPTIONS {} RTSP/1.0\r\nCSeq: {}\r\nSession: {}\r\nUser-Agent: {}\r\n\r\n",
            self.url, cseq, self.session_id, UA
        );
        self.stream.write_all(req.as_bytes())?;
        self.stream.flush()?;

        // Best effort: drain frames (keeping their NALs) until the matching
        // reply shows up or we have waited long enough.
        let deadline = Instant::now() + KEEPALIVE_WAIT;
        let saved = self.read_timeout;
        let result = (|| -> io::Result<bool> {
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(false); // no reply within the window; nal() will eat it
                }
                let left = deadline.saturating_duration_since(now);
                if left < saved {
                    self.stream.set_read_timeout(Some(left))?;
                }
                let mut progressed = false;
                // Frames first: never let a keepalive swallow video data.
                while self.buf.len() >= 4 && self.buf[0] == b'$' {
                    let chan = self.buf[1];
                    let len = ((self.buf[2] as usize) << 8) | self.buf[3] as usize;
                    if self.buf.len() < 4 + len {
                        break;
                    }
                    let payload: Vec<u8> = self.buf[4..4 + len].to_vec();
                    self.buf.drain(..4 + len);
                    if Some(chan) == self.video_chan {
                        if let Some((pl, marker)) = rtp_split(&payload) {
                            let pl = pl.to_vec();
                            self.depacketize(&pl, marker);
                        }
                    }
                    progressed = true;
                }
                if self.buf.starts_with(b"RTSP/") {
                    match parse_response(&self.buf)? {
                        Some((resp, used)) => {
                            self.buf.drain(..used);
                            let matched = resp
                                .header("CSeq")
                                .and_then(|v| v.trim().parse::<u32>().ok())
                                == Some(cseq);
                            if matched {
                                return Ok(true);
                            }
                            continue;
                        }
                        None => {}
                    }
                }
                if !progressed {
                    match self.fill() {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "rtsp: server closed the connection during keepalive",
                            ))
                        }
                        Ok(_) => {}
                        Err(ref e)
                            if e.kind() == io::ErrorKind::WouldBlock
                                || e.kind() == io::ErrorKind::TimedOut =>
                        {
                            return Ok(false)
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        })();
        let _ = self.stream.set_read_timeout(Some(saved));
        match result {
            Ok(answered) => {
                if answered {
                    self.missed_keepalives = 0;
                } else {
                    self.missed_keepalives = self.missed_keepalives.saturating_add(1);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// SPS and PPS from the H.264 `sprop-parameter-sets` fmtp field, if the
    /// server advertised both.
    pub fn sprop_ps(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let track = self
            .tracks
            .iter()
            .find(|t| t.media.eq_ignore_ascii_case("video"))
            .or_else(|| self.tracks.first())?;
        let idx = track.fmtp.find("sprop-parameter-sets=")?;
        let rest = &track.fmtp[idx + "sprop-parameter-sets=".len()..];
        let end = rest
            .find(|c: char| c == ';' || c.is_whitespace())
            .unwrap_or(rest.len());
        let value = &rest[..end];
        let mut it = value.split(',');
        let sps = base64_decode(it.next()?)?;
        let pps = base64_decode(it.next()?)?;
        if sps.is_empty() || pps.is_empty() {
            return None;
        }
        Some((sps, pps))
    }

    /// The raw SDP body (diagnostics).
    pub fn sdp(&self) -> &str {
        &self.sdp
    }

    /// Session timeout advertised by the server, in seconds (60 on this camera).
    pub fn server_timeout(&self) -> Option<u32> {
        self.server_timeout
    }

    /// Consecutive keepalives with no reply (diagnostics; reset by any reply).
    pub fn missed_keepalives(&self) -> u32 {
        self.missed_keepalives
    }

    /// TEARDOWN and close. Called automatically on drop.
    pub fn close(&mut self) {
        self.teardown();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    // -- internals ---------------------------------------------------------

    fn depacketize(&mut self, payload: &[u8], marker: bool) {
        if payload.is_empty() {
            return;
        }
        let b0 = payload[0];
        match b0 & 0x1f {
            1..=23 => {
                self.pending.push_back((annexb(payload), marker));
            }
            24 => {
                // STAP-A: 2-byte big-endian length + NAL, repeated.
                let mut i = 1usize;
                let mut nals: Vec<Vec<u8>> = Vec::new();
                while i + 2 <= payload.len() {
                    let size = ((payload[i] as usize) << 8) | payload[i + 1] as usize;
                    i += 2;
                    if size == 0 || i + size > payload.len() {
                        break;
                    }
                    nals.push(annexb(&payload[i..i + size]));
                    i += size;
                }
                let n = nals.len();
                for (k, nal) in nals.into_iter().enumerate() {
                    // The marker belongs to whichever NAL ends the packet.
                    self.pending.push_back((nal, marker && k + 1 == n));
                }
            }
            28 => {
                // FU-A: indicator, FU header (start/end/type), fragment.
                if payload.len() < 2 {
                    return;
                }
                let fu = payload[1];
                let start = fu & 0x80 != 0;
                let end = fu & 0x40 != 0;
                let nal_type = fu & 0x1f;
                let frag = &payload[2..];
                if start {
                    self.fu.clear();
                    // Rebuild the NAL header from FU indicator + real type.
                    self.fu.push((b0 & 0xe0) | nal_type);
                }
                if self.fu.is_empty() {
                    // Continuation without a start (or after an oversize drop).
                    return;
                }
                if self.fu.len() + frag.len() > MAX_NAL {
                    self.fu.clear();
                    return;
                }
                self.fu.extend_from_slice(frag);
                if end {
                    let nal = annexb(&self.fu);
                    self.fu.clear();
                    self.pending.push_back((nal, marker));
                }
            }
            // 25 (STAP-B), 26 (MTAP-16), 27 (MTAP-24), 29 (FU-B) and reserved
            // types are not produced by this encoder: drop them.
            _ => {}
        }
    }

    fn next_cseq(&mut self) -> u32 {
        self.cseq = self.cseq.wrapping_add(1);
        if self.cseq == 0 {
            self.cseq = 1;
        }
        self.cseq
    }

    fn teardown(&mut self) {
        if self.closed || self.session_id.is_empty() {
            self.closed = true;
            return;
        }
        self.closed = true;
        let cseq = self.next_cseq();
        let req = format!(
            "TEARDOWN {} RTSP/1.0\r\nCSeq: {}\r\nSession: {}\r\n\r\n",
            self.base, cseq, self.session_id
        );
        let _ = self.stream.set_write_timeout(Some(Duration::from_millis(300)));
        let _ = self.stream.write_all(req.as_bytes());
        let _ = self.stream.flush();
    }

    /// Read more bytes into `self.buf`. Returns how many were read.
    fn fill(&mut self) -> io::Result<usize> {
        if self.buf.len() > MAX_BUF {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rtsp: inbound buffer exceeded limit",
            ));
        }
        let mut chunk = [0u8; CHUNK];
        let n = self.stream.read(&mut chunk)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    fn request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
    ) -> io::Result<Response> {
        let cseq = self.next_cseq();
        let mut req = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n");
        req.push_str(&format!("User-Agent: {UA}\r\n"));
        let mut have_session = false;
        for (k, v) in extra {
            if k.eq_ignore_ascii_case("session") {
                have_session = true;
            }
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !have_session
            && !self.session_id.is_empty()
            && !method.eq_ignore_ascii_case("OPTIONS")
            && !method.eq_ignore_ascii_case("DESCRIBE")
        {
            req.push_str(&format!("Session: {}\r\n", self.session_id));
        }
        req.push_str("\r\n");
        self.stream.write_all(req.as_bytes())?;
        self.stream.flush()?;

        // Wait for the reply, keeping any interleaved frames that arrive first.
        loop {
            // Drop complete interleaved frames ahead of the reply.
            loop {
                if self.buf.len() < 4 || self.buf[0] != b'$' {
                    break;
                }
                let chan = self.buf[1];
                let len = ((self.buf[2] as usize) << 8) | self.buf[3] as usize;
                if self.buf.len() < 4 + len {
                    break;
                }
                let payload: Vec<u8> = self.buf[4..4 + len].to_vec();
                self.buf.drain(..4 + len);
                if Some(chan) == self.video_chan {
                    if let Some((pl, marker)) = rtp_split(&payload) {
                        let pl = pl.to_vec();
                        self.depacketize(&pl, marker);
                    }
                }
            }
            if self.buf.first() != Some(&b'$') {
                match parse_response(&self.buf) {
                    Ok(Some((resp, used))) => {
                        self.buf.drain(..used);
                        return Ok(resp);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        // Not an RTSP reply after all: resynchronise.
                        self.buf.drain(..1);
                        continue;
                    }
                }
            }
            match self.fill() {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("rtsp: connection closed during {method}"),
                    ))
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.teardown();
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Split `rtsp://host[:port]/path` into (host, port, request_url).
/// Userinfo (`user:pass@`) is stripped: this server requires no auth and none
/// is implemented. Kept out of the returned URL so nothing leaks into logs.
fn split_url(url: &str) -> io::Result<(String, u16, String)> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("rtsp://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rtsp: URL must start with rtsp://",
        ));
    }
    let rest = &url[7..];
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority = match authority.rfind('@') {
        Some(i) => &authority[i + 1..], // drop credentials
        None => authority,
    };
    if authority.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rtsp: URL has no host",
        ));
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // [::1]:554
        match rest.find(']') {
            Some(i) => {
                let host = &rest[..i];
                let after = &rest[i + 1..];
                let port = after
                    .strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok())
                    .unwrap_or(554);
                (host.to_string(), port)
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rtsp: malformed IPv6 literal",
                ))
            }
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(port) => (h.to_string(), port),
                Err(_) => (authority.to_string(), 554),
            },
            None => (authority.to_string(), 554),
        }
    };
    let hostport = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let req_url = if path.is_empty() {
        format!("rtsp://{hostport}/")
    } else {
        format!("rtsp://{hostport}{path}")
    };
    Ok((host, port, req_url))
}

fn tcp_connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let addrs = (host, port).to_socket_addrs()?;
    let mut last: Option<io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) => e,
        None => io::Error::new(io::ErrorKind::NotFound, "rtsp: host did not resolve"),
    })
}

/// Join the base URL with a relative `a=control` value.
fn join_control(base: &str, control: &str) -> String {
    let c = control.trim();
    if c.is_empty() || c == "*" {
        return base.to_string();
    }
    if c.to_ascii_lowercase().starts_with("rtsp://") {
        return c.to_string();
    }
    if let Some(rest) = c.strip_prefix('/') {
        // absolute path: keep scheme+authority of base
        if let Some(i) = base[8.min(base.len())..].find('/') {
            return format!("{}/{}", &base[..8 + i], rest);
        }
        return base.to_string();
    }
    format!("{}{}", base, c)
}

fn annexb(nal: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(nal.len() + 4);
    v.extend_from_slice(&[0, 0, 0, 1]);
    v.extend_from_slice(nal);
    v
}

/// Strip the 12-byte RTP header (+CSRC list, +extension) and return
/// `(payload, marker)`.
fn rtp_split(pkt: &[u8]) -> Option<(&[u8], bool)> {
    if pkt.len() < 12 {
        return None;
    }
    if pkt[0] >> 6 != 2 {
        return None; // not RTP version 2
    }
    let cc = (pkt[0] & 0x0f) as usize;
    let mut off = 12usize.checked_add(cc.checked_mul(4)?)?;
    if pkt[0] & 0x10 != 0 {
        // header extension: 2-byte profile, 2-byte length in 32-bit words
        if pkt.len() < off + 4 {
            return None;
        }
        let words = ((pkt[off + 2] as usize) << 8) | pkt[off + 3] as usize;
        off = off.checked_add(4 + words.checked_mul(4)?)?;
    }
    if off > pkt.len() {
        return None;
    }
    let marker = pkt[1] & 0x80 != 0;
    Some((&pkt[off..], marker))
}

/// Parse one complete RTSP reply out of the front of `buf`.
/// `Ok(Some((resp, consumed)))`, `Ok(None)` when more bytes are needed.
fn parse_response(buf: &[u8]) -> io::Result<Option<(Response, usize)>> {
    let hdr_end = match find_subsequence(buf, b"\r\n\r\n") {
        Some(i) => i,
        None => {
            if buf.len() > 64 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "rtsp: reply header too large",
                ));
            }
            return Ok(None);
        }
    };
    let head = &buf[..hdr_end];
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut sp = status_line.split_whitespace();
    let _proto = sp.next().unwrap_or("");
    let status: u16 = match sp.next().and_then(|s| s.parse::<u16>().ok()) {
        Some(s) => s,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rtsp: not a status line",
            ))
        }
    };
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let clen = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let total = hdr_end + 4 + clen;
    if buf.len() < total {
        if clen > 8 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rtsp: absurd Content-Length",
            ));
        }
        return Ok(None);
    }
    let body = buf[hdr_end + 4..total].to_vec();
    Ok(Some((Response { status, headers, body }, total)))
}

fn find_subsequence(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `"00000006; timeout=60"` -> (`"00000006"`, `Some(60)`)
fn parse_session_header(v: &str) -> (String, Option<u32>) {
    let mut parts = v.split(';');
    let id = parts.next().unwrap_or("").trim().to_string();
    let mut timeout = None;
    for p in parts {
        let p = p.trim();
        if let Some(rest) = p.strip_prefix("timeout=") {
            timeout = rest.trim().parse::<u32>().ok();
        }
    }
    (id, timeout)
}

/// Parse `interleaved=0-1` out of a Transport header value.
fn parse_interleaved(t: &str) -> Option<(u8, u8)> {
    let idx = t.find("interleaved=")?;
    let rest = &t[idx + "interleaved=".len()..];
    let end = rest
        .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
        .unwrap_or(rest.len());
    let pair = &rest[..end];
    let mut it = pair.split('-');
    let a: u8 = it.next()?.trim().parse().ok()?;
    let b: u8 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(a + 1);
    Some((a, b))
}

/// SDP -> tracks, in SDP order.
fn parse_sdp(sdp: &str) -> Vec<Track> {
    let mut tracks: Vec<Track> = Vec::new();
    let mut payloads: Vec<u32> = Vec::new();
    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            let f: Vec<&str> = rest.split_whitespace().collect();
            let media = f.first().copied().unwrap_or("").to_ascii_lowercase();
            payloads = f
                .iter()
                .skip(3)
                .filter_map(|s| s.trim().parse::<u32>().ok())
                .collect();
            tracks.push(Track {
                media,
                encoding: String::new(),
                clock: 0,
                control: String::new(),
                fmtp: String::new(),
                interleaved: None,
            });
            continue;
        }
        let Some(t) = tracks.last_mut() else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let mut it = rest.splitn(2, ' ');
            let pt: u32 = it.next().unwrap_or("").trim().parse().unwrap_or(0);
            if payloads.is_empty() || payloads.contains(&pt) {
                let enc = it.next().unwrap_or("").trim();
                let mut parts = enc.split('/');
                t.encoding = parts.next().unwrap_or("").trim().to_ascii_uppercase();
                t.clock = parts
                    .next()
                    .and_then(|c| c.trim().parse::<u32>().ok())
                    .unwrap_or(0);
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            let mut it = rest.splitn(2, ' ');
            let pt: u32 = it.next().unwrap_or("").trim().parse().unwrap_or(0);
            if payloads.is_empty() || payloads.contains(&pt) {
                t.fmtp = it.next().unwrap_or("").trim().to_string();
            }
        } else if let Some(rest) = line.strip_prefix("a=control:") {
            t.control = rest.trim().to_string();
        }
    }
    tracks
}

/// Decode standard or URL-safe base64. Whitespace is ignored, padding optional.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // leftover bits must be padding only
    if bits >= 6 {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Unit tests (host-only): `rustc --test --edition 2021 src/rtsp.rs -o /tmp/t`
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64() {
        let sps = base64_decode("J03gKI1oBQBboQAAAwABAAADADIPFCKg").unwrap();
        assert_eq!(sps.len(), 24);
        assert_eq!(sps[0] & 0x1f, 7); // NAL type 7 = SPS
        assert_eq!(base64_decode("KO4C0kg=").unwrap(), vec![0x28, 0xee, 0x02, 0xd2, 0x48]);
        assert_eq!(base64_decode("").unwrap().len(), 0);
        assert!(base64_decode("a").is_none()); // 6 bits, no padding possible
    }

    #[test]
    fn sdp_vs0() {
        let sdp = "v=0\r\no=- 4 1 IN IP4 192.168.1.79\r\ns=/vs0\r\ni=luo_zhanzhao\r\nc=IN IP4 192.168.1.103\r\nt=0 0\r\na=control:*\r\na=range:npt=0-\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 sprop-parameter-sets=J03gKI1oBQBboQAAAwABAAADADIPFCKg,KO4C0kg=\r\na=recvonly\r\na=control:camera\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=recvonly\r\na=control:micphone\r\n";
        let t = parse_sdp(sdp);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].media, "video");
        assert_eq!(t[0].encoding, "H264");
        assert_eq!(t[0].clock, 90000);
        assert_eq!(t[0].control, "camera");
        assert!(t[0].fmtp.starts_with("sprop-parameter-sets="));
        assert_eq!(t[1].media, "audio");
        assert_eq!(t[1].encoding, "PCMA");
        assert_eq!(t[1].clock, 8000);
        assert_eq!(t[1].control, "micphone");
        assert_eq!(t[1].fmtp, "");
    }

    #[test]
    fn urls() {
        let (h, p, u) = split_url("rtsp://192.168.1.103:554/vs0").unwrap();
        assert_eq!((h.as_str(), p, u.as_str()), ("192.168.1.103", 554, "rtsp://192.168.1.103:554/vs0"));
        let (h, p, _) = split_url("rtsp://cam.local/vs1").unwrap();
        assert_eq!((h.as_str(), p), ("cam.local", 554));
        let (h, p, _) = split_url("rtsp://[::1]:8554/vs0").unwrap();
        assert_eq!((h.as_str(), p), ("::1", 8554));
        let (_, _, u) = split_url("rtsp://user:pw@host:554/").unwrap();
        assert_eq!(u, "rtsp://host:554/");
        assert_eq!(join_control("rtsp://h:554/vs0/", "camera"), "rtsp://h:554/vs0/camera");
        assert_eq!(join_control("rtsp://h:554/vs0/", "*"), "rtsp://h:554/vs0/");
        assert_eq!(join_control("rtsp://h:554/vs0/", "rtsp://x/y"), "rtsp://x/y");
    }

    #[test]
    fn interleaved_and_session() {
        assert_eq!(
            parse_interleaved("RTP/AVP/TCP;unicast;interleaved=2-3;ssrc=9"),
            Some((2, 3))
        );
        assert_eq!(parse_interleaved("RTP/AVP/UDP;unicast"), None);
        assert_eq!(
            parse_session_header("00000006; timeout=60"),
            ("00000006".to_string(), Some(60))
        );
        assert_eq!(parse_session_header("ABCD"), ("ABCD".to_string(), None));
    }

    #[test]
    fn rtp_header_variants() {
        let mut p = vec![0x80, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        p.extend_from_slice(&[0x65, 1, 2, 3]);
        let (pl, m) = rtp_split(&p).unwrap();
        assert_eq!((pl[0], m), (0x65, false));
        assert_eq!(pl.len(), 4);
        // version 1 must be rejected
        let mut bad = p.clone();
        bad[0] = 0x40;
        assert!(rtp_split(&bad).is_none());
        // 12-byte header + 1 CSRC (CC=1) + extension with 2 words + payload
        let mut q = vec![0x91, 0x80, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0xaa, 0xaa, 0xaa, 0xaa];
        q.extend_from_slice(&[0xbe, 0xde, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]); // ext hdr + 2 words
        q.extend_from_slice(&[0x41, 0x42]);
        let (pl, m) = rtp_split(&q).unwrap();
        assert_eq!(pl, &[0x41, 0x42]);
        assert!(m);
        // extension claiming more words than present must be rejected, not panic
        let mut short = q.clone();
        short.truncate(q.len() - 1);
        short[18] = 0xff;
        assert!(rtp_split(&short).is_none());
        // truncated header
        assert!(rtp_split(&vec![0x80, 0x80, 0, 1, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }

    fn fu(indicator_type: u8, is_start: bool, is_end: bool, typ: u8, frag: &[u8]) -> Vec<u8> {
        let mut v = vec![indicator_type, (if is_start { 0x80 } else { 0 }) | (if is_end { 0x40 } else { 0 }) | typ];
        v.extend_from_slice(frag);
        v
    }

    #[test]
    fn depacketize_rtp_level() {
        // build a fake session-free harness by using the free functions
        let mut p = vec![0x80, 0x80, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0];
        p.extend_from_slice(&[0x67, 0xaa]);
        let (pl, m) = rtp_split(&p).unwrap();
        assert_eq!((pl, m), (&[0x67, 0xaa][..], true));
        // FU-A split of an IDR: 0x65 0x88 0x84, then tail
        let a = fu(0x7c, true, false, 5, &[0x88, 0x84]);
        let b = fu(0x7c, false, true, 5, &[0x21, 0x22]);
        assert_eq!(a[0] & 0xe0 | (a[1] & 0x1f), 0x65);
        assert_eq!(b[1] & 0x40, 0x40);
    }

    #[test]
    fn response_parsing() {
        let raw = b"RTSP/1.0 200 OK\r\nContent-Type: application/sdp\r\nContent-Length: 4\r\nCSeq: 2\r\n\r\nv=0\r\nRTSP/1.0 454";
        let (r, used) = parse_response(raw).unwrap().unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("cseq"), Some("2"));
        assert_eq!(r.body, b"v=0\r");
        assert_eq!(used, 82); // header 77 + body 5
        // incomplete body
        assert!(parse_response(&raw[..81]).unwrap().is_none()); // body 1 byte short
        // and a header-only prefix is also incomplete
        assert!(parse_response(&raw[..40]).unwrap().is_none());
    }
}

/// End-to-end robustness tests against a fake server speaking this camera's
/// dialect with deliberately nasty framing. Runs anywhere, no camera needed.
#[cfg(test)]
mod fake_server {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    const SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=/vs0\r\nt=0 0\r\na=control:*\r\n\
m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
a=fmtp:96 sprop-parameter-sets=Z0EAQQ==,aM48gA==\r\na=recvonly\r\na=control:camera\r\n";

    fn frame_bytes(chan: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![b'$', chan, (payload.len() >> 8) as u8, payload.len() as u8];
        v.extend_from_slice(payload);
        v
    }

    fn frame(s: &mut TcpStream, chan: u8, payload: &[u8]) {
        s.write_all(&frame_bytes(chan, payload)).unwrap();
    }

    fn rtp(marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![
            0x80,
            if marker { 0x80 | 96 } else { 96 },
            0,
            1,
            0x11,
            0x22,
            0x33,
            0x44,
            0,
            0,
            0,
            0,
        ];
        v.extend_from_slice(payload);
        v
    }

    fn cases(s: &mut TcpStream) {
        // 1. stray non-protocol bytes (no '$') must be dropped
        s.write_all(b"?xy").unwrap();
        // 2. an RTSP text reply arriving mid-stream must be swallowed whole
        s.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 77\r\nPublic: OPTIONS\r\n\r\n")
            .unwrap();
        // 3. RTCP on the odd channel: looks like RTP v2, must never be parsed
        frame(
            s,
            1,
            &[0x81, 0xca, 0x00, 0x09, 0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0],
        );
        // 4. single NAL (SPS)
        frame(s, 0, &rtp(false, &[0x67, 0x11]));
        // 5. STAP-A with a non-IDR then an IDR slice; marker on the last one
        frame(
            s,
            0,
            &rtp(true, &[0x78, 0x00, 0x02, 0x61, 0xaa, 0x00, 0x02, 0x65, 0xbb]),
        );
        // 6. FU-A in three fragments (IDR 65 88 84 21)
        frame(s, 0, &rtp(false, &[0x7c, 0x85, 0x88]));
        frame(s, 0, &rtp(false, &[0x7c, 0x05, 0x84]));
        frame(s, 0, &rtp(true, &[0x7c, 0x45, 0x21]));
        // 7. orphan continuation (no start) must be dropped, not crash
        frame(s, 0, &rtp(false, &[0x7c, 0x05, 0x99]));
        // 8. FU-B (29) is not used by this encoder: dropped
        frame(s, 0, &rtp(true, &[0x7d, 0x85, 0x01]));
        // 9. empty interleaved frame
        frame(s, 0, &[]);
        // 10. RTP version 1: ignored
        let mut bad = rtp(false, &[0x67, 0x22]);
        bad[0] = 0x40;
        frame(s, 0, &bad);
        // 11. CSRC list (CC=2) + one header-extension word, then a SEI NAL
        let mut ext = vec![0x92, 96, 0, 3, 9, 9, 9, 9, 0, 0, 0, 0];
        ext.extend_from_slice(&[0, 0, 0, 0]); // CSRC 1
        ext.extend_from_slice(&[0, 0, 0, 0]); // CSRC 2
        ext.extend_from_slice(&[0xbe, 0xde, 0, 1, 1, 2, 3, 4]); // ext hdr + 1 word
        ext.extend_from_slice(&[0x06, 0x01, 0x02]);
        frame(s, 0, &ext);
        // 12. a frame delivered one byte at a time (partial reads)
        let v = frame_bytes(0, &rtp(true, &[0x41, 0x42]));
        for b in v {
            s.write_all(&[b]).unwrap();
            s.flush().unwrap();
            thread::sleep(Duration::from_micros(200));
        }
    }

    fn serve(mut s: TcpStream) {
        let rs = s.try_clone().unwrap();
        let mut r = BufReader::new(rs);
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let method = line.split_whitespace().next().unwrap_or("").to_string();
            let mut cseq = 1u32;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 {
                    return;
                }
                if h == "\r\n" || h == "\n" {
                    break;
                }
                if h.to_ascii_lowercase().starts_with("cseq:") {
                    cseq = h.split(':').nth(1).unwrap_or("1").trim().parse().unwrap_or(1);
                }
            }
            match method.as_str() {
                "OPTIONS" => {
                    let _ = write!(
                        s,
                        "RTSP/1.0 200 OK\r\nPublic: OPTIONS,DESCRIBE,SETUP,PLAY,TEARDOWN\r\nCSeq: {cseq}\r\n\r\n"
                    );
                }
                "DESCRIBE" => {
                    let _ = write!(
                        s,
                        "RTSP/1.0 200 OK\r\nContent-Type: application/sdp\r\nContent-Base: rtsp://127.0.0.1/vs0/\r\nContent-Length: {}\r\nCSeq: {cseq}\r\n\r\n{}",
                        SDP.len(),
                        SDP
                    );
                }
                "SETUP" => {
                    let _ = write!(
                        s,
                        "RTSP/1.0 200 OK\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=7\r\nCSeq: {cseq}\r\nSession: 0000abcd; timeout=60\r\n\r\n"
                    );
                }
                "PLAY" => {
                    let _ = write!(
                        s,
                        "RTSP/1.0 200 OK\r\nRange: npt=0.000-\r\nCSeq: {cseq}\r\nSession: 0000abcd; timeout=60\r\n\r\n"
                    );
                    cases(&mut s);
                    thread::sleep(Duration::from_millis(1500));
                    let _ = s.shutdown(std::net::Shutdown::Both);
                    return;
                }
                _ => {
                    let _ = write!(s, "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n");
                }
            }
        }
    }

    #[test]
    fn survives_garbage_and_reassembles() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = thread::spawn(move || {
            if let Ok((s, _)) = l.accept() {
                serve(s);
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/vs0");
        let mut sess =
            Session::connect(&url, &["video"], Duration::from_millis(300)).expect("connect");
        assert_eq!(sess.session_id(), "0000abcd");
        assert_eq!(sess.server_timeout(), Some(60));
        assert_eq!(sess.tracks().len(), 1);
        assert_eq!(sess.tracks()[0].interleaved, Some((0, 1)));
        let (sps, pps) = sess.sprop_ps().expect("sprop");
        assert_eq!(sps, vec![0x67, 0x41, 0x00, 0x41]);
        assert_eq!(pps, vec![0x68, 0xce, 0x3c, 0x80]);

        let mut got: Vec<(Vec<u8>, bool)> = Vec::new();
        while got.len() < 6 {
            match sess.nal_with_marker() {
                Ok(Some(x)) => got.push(x),
                Ok(None) => {}
                Err(e) => panic!("stream error after {} nals: {e}", got.len()),
            }
        }
        let expect: [(&[u8], bool); 6] = [
            (&[0, 0, 0, 1, 0x67, 0x11], false),
            (&[0, 0, 0, 1, 0x61, 0xaa], false),
            (&[0, 0, 0, 1, 0x65, 0xbb], true),
            (&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21], true),
            (&[0, 0, 0, 1, 0x06, 0x01, 0x02], false),
            (&[0, 0, 0, 1, 0x41, 0x42], true),
        ];
        for (k, (want, marker)) in expect.iter().enumerate() {
            assert_eq!(got[k].0, want.to_vec(), "nal {k}");
            assert_eq!(got[k].1, *marker, "marker {k}");
        }
        sess.close();
        let _ = t.join();
    }
}
