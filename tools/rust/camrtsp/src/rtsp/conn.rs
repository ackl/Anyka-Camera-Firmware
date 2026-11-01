//! One RTSP control connection: the request/response loop, interleaved `$`
//! frames from the client (RTCP receiver reports, ignored), idle reaping,
//! and the single session the connection owns.
//!
//! One session per connection, as on the stock camera: a session id from
//! another connection is unknown here (454). Closing the connection ends the
//! session, including a UDP one (every client we care about keeps the
//! control connection open while it plays).

use super::msg::{self, Parsed, Request};
use super::rtp::{self, Rtp};
use super::sdp;
use super::session::{self, Kind, Out, Shared, Track, Wire};
use super::transport::{self, Lower};
use super::{rand_u64, Server};
use std::fmt::Write as _;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Read timeout: how often an idle connection re-checks liveness.
const READ_TICK: Duration = Duration::from_secs(1);
/// A client that cannot take a write within this long is dead or hopeless.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
/// Slack on top of the advertised session timeout before reaping.
const REAP_GRACE: Duration = Duration::from_secs(5);
/// Largest interleaved frame from the client we bother to look at; bigger
/// ones are skipped without buffering.
const RTCP_MAX: usize = 1500;

const PUBLIC: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER";

/// Turn a connection away when we are full.
pub fn refuse(s: TcpStream) {
    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = (&s).write_all(b"RTSP/1.0 503 Service Unavailable\r\nRetry-After: 10\r\n\r\n");
}

struct Reply {
    code: u16,
    extra: String,
    ctype: &'static str,
    body: String,
    play: bool,
}

impl Reply {
    fn code(code: u16) -> Reply {
        Reply { code, extra: String::new(), ctype: "", body: String::new(), play: false }
    }
    fn ok(extra: String) -> Reply {
        Reply { extra, ..Reply::code(200) }
    }
}

struct Sess {
    id: String,
    shared: Arc<Shared>,
    /// Set-up tracks, by track id, until PLAY hands them to the sender.
    tracks: [Option<Track>; 2],
    sender: Option<JoinHandle<()>>,
    /// Some track is interleaved on this connection.
    tcp: bool,
    playing: bool,
}

struct Conn {
    srv: Arc<Server>,
    wire: Arc<Wire>,
    peer: SocketAddr,
    local: SocketAddr,
    sess: Option<Sess>,
    out: Vec<u8>,
}

pub fn run(srv: Arc<Server>, stream: TcpStream, peer: SocketAddr) {
    serve_conn(&srv, stream, peer);
    srv.leave();
}

fn serve_conn(srv: &Arc<Server>, stream: TcpStream, peer: SocketAddr) {
    let local = match stream.local_addr() {
        Ok(a) => a,
        Err(_) => return,
    };
    let _ = stream.set_nodelay(true);
    if stream.set_read_timeout(Some(READ_TICK)).is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let mut rd = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut c = Conn {
        srv: srv.clone(),
        wire: Arc::new(Wire::new(stream)),
        peer,
        local,
        sess: None,
        out: Vec::with_capacity(1024),
    };
    let idle_limit = Duration::from_secs(srv.cfg.session_timeout as u64) + REAP_GRACE;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let mut skip = 0usize;
    let mut last_rx = Instant::now();
    let why = 'conn: loop {
        if c.wire.dead() {
            break "write failed";
        }
        let n = match rd.read(&mut chunk) {
            Ok(0) => break "closed by peer",
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if c.idle(last_rx, idle_limit) {
                    break "idle timeout";
                }
                continue;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break "read error",
        };
        last_rx = Instant::now();
        let mut data = &chunk[..n];
        if skip > 0 {
            let k = skip.min(data.len());
            skip -= k;
            data = &data[k..];
        }
        buf.extend_from_slice(data);

        while !buf.is_empty() {
            if buf[0] == b'$' {
                // Interleaved data from the client: RTCP RRs on our RTCP
                // channels, or anything at all. Checked, never trusted.
                if buf.len() < 4 {
                    break;
                }
                let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                if buf.len() >= 4 + len {
                    let _ = rtp::rtcp_valid(&buf[4..4 + len]);
                    buf.drain(..4 + len);
                    continue;
                }
                if len > RTCP_MAX {
                    skip = 4 + len - buf.len();
                    buf.clear();
                }
                break;
            }
            if buf[0] == b'\r' || buf[0] == b'\n' {
                buf.drain(..1); // stray CRLF between requests
                continue;
            }
            let (used, close) = match msg::parse(&buf) {
                Parsed::More => break,
                Parsed::Bad { code, cseq, len } => {
                    let name = &c.srv.cfg.server_name;
                    msg::render(&mut c.out, code, cseq, name, "", "", b"");
                    (len, len == 0)
                }
                Parsed::Req(req, len) => {
                    let r = c.handle(&req);
                    let name = &c.srv.cfg.server_name;
                    msg::render(&mut c.out, r.code, req.header("CSeq"), name, &r.extra, r.ctype, r.body.as_bytes());
                    if c.wire.send(&c.out) && r.play {
                        // Only after the 200 is on the wire may RTP follow it.
                        if let Some(s) = &c.sess {
                            s.shared.set_playing(true);
                        }
                    }
                    c.out.clear();
                    (len, false)
                }
            };
            if !c.out.is_empty() {
                c.wire.send(&c.out);
                c.out.clear();
            }
            if close {
                break 'conn "unparseable request";
            }
            buf.drain(..used);
        }
    };
    let id = c.sess.as_ref().map(|s| s.id.clone()).unwrap_or_default();
    c.end_session();
    eprintln!("rtsp: {} closed ({}){}{}", peer, why, if id.is_empty() { "" } else { ", session " }, id);
}

impl Conn {
    /// Has the client gone quiet for longer than the session timeout? A
    /// session interleaved on this connection proves liveness by accepting
    /// writes (a dead peer fails them within WRITE_TIMEOUT), so only UDP and
    /// idle connections are reaped here.
    fn idle(&self, last_rx: Instant, limit: Duration) -> bool {
        let mut last = last_rx;
        if let Some(s) = &self.sess {
            if s.playing && s.tcp {
                return false;
            }
            if let Some(t) = s.shared.last_rx() {
                last = last.max(t);
            }
        }
        last.elapsed() > limit
    }

    fn handle(&mut self, req: &Request) -> Reply {
        match req.method {
            "OPTIONS" => Reply::ok(format!("Public: {}\r\n", PUBLIC)),
            "DESCRIBE" => self.describe(req),
            "SETUP" => self.setup(req),
            "PLAY" => self.play(req),
            "PAUSE" => self.pause(req),
            "TEARDOWN" => self.teardown(req),
            "GET_PARAMETER" | "SET_PARAMETER" => self.parameter(req),
            _ => Reply { extra: format!("Allow: {}\r\n", PUBLIC), ..Reply::code(405) },
        }
    }

    fn session_header(&self) -> String {
        match &self.sess {
            Some(s) => format!("Session: {};timeout={}\r\n", s.id, self.srv.cfg.session_timeout),
            None => String::new(),
        }
    }

    /// PLAY/PAUSE/TEARDOWN need the session this connection set up: none
    /// set up at all is 455 (method not valid before SETUP), a missing or
    /// different id is 454.
    fn check_session(&self, req: &Request) -> Result<(), u16> {
        match (req.header("Session").map(msg::session_id), &self.sess) {
            (Some(h), Some(s)) if h == s.id => Ok(()),
            (Some(_), _) => Err(454),
            (None, None) => Err(455),
            (None, Some(_)) => Err(454),
        }
    }

    fn describe(&mut self, req: &Request) -> Reply {
        let info = self.srv.source.info();
        let t = match msg::target(req.url, sdp::controls(&info), &self.local) {
            Some(t) if t.track.is_none() => t,
            _ => return Reply::code(404),
        };
        Reply {
            extra: format!("Content-Base: {}\r\n", t.base),
            ctype: "application/sdp",
            body: sdp::build(&info, self.srv.origin, self.local.ip()),
            ..Reply::code(200)
        }
    }

    fn setup(&mut self, req: &Request) -> Reply {
        let info = self.srv.source.info();
        let track = match msg::target(req.url, sdp::controls(&info), &self.local) {
            // The aggregate URL alone means the video track.
            Some(t) => t.track.unwrap_or(0),
            None => return Reply::code(404),
        };
        if let Some(h) = req.header("Session").map(msg::session_id) {
            match &self.sess {
                Some(s) if s.id == h => {}
                _ => return Reply::code(454),
            }
        }
        // Tracks are handed to the sender on the first PLAY; adding one to a
        // running session is not supported.
        if matches!(&self.sess, Some(s) if s.sender.is_some()) {
            return Reply::code(455);
        }
        let th = match req.header("Transport") {
            Some(h) => h,
            None => return Reply::code(400),
        };
        let lower = match transport::parse(th) {
            Some(l) => l,
            None => return Reply::code(461),
        };
        // Interleaved channels the *other* track already uses.
        let taken: Option<(u8, u8)> = self.sess.as_ref().and_then(|s| {
            s.tracks.iter().enumerate().find_map(|(i, t)| match t {
                Some(Track { out: Out::Tcp { rtp_ch, rtcp_ch, .. }, .. }) if i != track => Some((*rtp_ch, *rtcp_ch)),
                _ => None,
            })
        });
        let clash = |a: u8, b: u8| matches!(taken, Some((x, y)) if a == x || a == y || b == x || b == y);
        let (out, tdesc) = match lower {
            Lower::Tcp(want) => {
                let (a, b) = match want {
                    Some(p) => p,
                    // Default 0-1; a second track without a request gets the
                    // next free even pair.
                    None => match (0..=252u8).step_by(2).find(|&a| !clash(a, a + 1)) {
                        Some(a) => (a, a + 1),
                        None => return Reply::code(461),
                    },
                };
                if a == b || clash(a, b) {
                    return Reply::code(461);
                }
                (
                    Out::Tcp { wire: self.wire.clone(), rtp_ch: a, rtcp_ch: b },
                    format!("RTP/AVP/TCP;unicast;interleaved={}-{}", a, b),
                )
            }
            Lower::Udp { rtp: cp, rtcp: cq } => {
                let (rs, cs) = match session::bind_pair(self.local.ip()) {
                    Ok(p) => p,
                    Err(_) => return Reply::code(503),
                };
                let (sp, sq) = match (rs.local_addr(), cs.local_addr()) {
                    (Ok(a), Ok(b)) => (a.port(), b.port()),
                    _ => return Reply::code(503),
                };
                let ip = self.peer.ip();
                (
                    Out::Udp {
                        rtp: rs,
                        rtcp: cs,
                        dst_rtp: SocketAddr::new(ip, cp),
                        dst_rtcp: SocketAddr::new(ip, cq),
                    },
                    format!("RTP/AVP;unicast;client_port={}-{};server_port={}-{}", cp, cq, sp, sq),
                )
            }
        };
        let r = rand_u64();
        let ssrc = r as u32;
        let seq = (r >> 32) as u16;
        let base = rand_u64() as u32;
        let (kind, rtp) = if track == 0 {
            (Kind::Video, Rtp::new(sdp::VIDEO_PT, 90_000, ssrc, seq, base))
        } else {
            let rate = info.audio.as_ref().map(|a| a.rate).filter(|&r| r > 0).unwrap_or(8000);
            (Kind::Audio, Rtp::new(sdp::AUDIO_PT, rate, ssrc, seq, base))
        };
        let s = self.sess.get_or_insert_with(|| Sess {
            id: format!("{:016X}", rand_u64()),
            shared: Shared::new(),
            tracks: [None, None],
            sender: None,
            tcp: false,
            playing: false,
        });
        s.tracks[track] = Some(Track { kind, out, rtp });
        s.tcp = s.tracks.iter().flatten().any(|t| matches!(t.out, Out::Tcp { .. }));
        let mut extra = format!("Transport: {};ssrc={:08X}\r\n", tdesc, ssrc);
        extra.push_str(&self.session_header());
        Reply::ok(extra)
    }

    fn play(&mut self, req: &Request) -> Reply {
        if let Err(code) = self.check_session(req) {
            return Reply::code(code);
        }
        let info = self.srv.source.info();
        let t = match msg::target(req.url, sdp::controls(&info), &self.local) {
            Some(t) => t,
            None => return Reply::code(404),
        };
        let header = self.session_header();
        let s = match self.sess.as_mut() {
            Some(s) => s,
            None => return Reply::code(455),
        };
        let mut extra = String::from("Range: npt=0.000-\r\n");
        if s.sender.is_none() {
            let mut tracks = Vec::with_capacity(2);
            let mut rtp_info = String::new();
            for (i, slot) in s.tracks.iter_mut().enumerate() {
                if let Some(tr) = slot.take() {
                    // RTP-Info (RFC 2326 §12.33): first seq and the RTP time
                    // of the first frame, which `Rtp::ts_for` pins to `base`.
                    let _ = write!(
                        rtp_info,
                        "{}url={}{};seq={};rtptime={}",
                        if rtp_info.is_empty() { "" } else { "," },
                        t.base,
                        sdp::CONTROLS[i],
                        tr.rtp.seq,
                        tr.rtp.base
                    );
                    tracks.push(tr);
                }
            }
            if tracks.is_empty() {
                return Reply::code(455);
            }
            let label = format!("{} session {}", self.peer, s.id);
            match session::spawn_sender(s.shared.clone(), info, tracks, self.peer.ip(), label) {
                Ok(h) => s.sender = Some(h),
                Err(_) => return Reply::code(503),
            }
            self.srv.fanout.add(s.shared.clone());
            let _ = write!(extra, "RTP-Info: {}\r\n", rtp_info);
        }
        s.playing = true;
        extra.push_str(&header);
        Reply { play: true, ..Reply::ok(extra) }
    }

    fn pause(&mut self, req: &Request) -> Reply {
        if let Err(code) = self.check_session(req) {
            return Reply::code(code);
        }
        if let Some(s) = self.sess.as_mut() {
            if s.playing {
                s.playing = false;
                s.shared.set_playing(false);
            }
        }
        Reply::ok(self.session_header())
    }

    fn teardown(&mut self, req: &Request) -> Reply {
        if let Err(code) = self.check_session(req) {
            return Reply::code(code);
        }
        self.end_session();
        Reply::ok(String::new())
    }

    /// GET_PARAMETER is the usual keepalive (VLC, ffmpeg over UDP). We
    /// understand no parameters, so SET_PARAMETER with a body is 451.
    fn parameter(&mut self, req: &Request) -> Reply {
        if let Some(h) = req.header("Session").map(msg::session_id) {
            if !matches!(&self.sess, Some(s) if s.id == h) {
                return Reply::code(454);
            }
        }
        if req.method == "SET_PARAMETER" && !req.body.iter().all(|b| b.is_ascii_whitespace()) {
            return Reply::code(451);
        }
        Reply::ok(self.session_header())
    }

    /// Stop the sender, free the queues and sockets. The fan-out forgets the
    /// session on its next frame.
    fn end_session(&mut self) {
        if let Some(mut s) = self.sess.take() {
            s.shared.kill();
            if let Some(h) = s.sender.take() {
                let _ = h.join();
            }
        }
    }
}
