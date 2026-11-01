//! End-to-end tests over real loopback sockets: a hand-rolled client speaks
//! to `serve_listener` the way ffmpeg / VLC / our camweb client do.

use super::*;
use crate::media::{nal_type, AudioFrame, AudioInfo, StreamInfo, VideoFrame};
use crate::source::Hub;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::Instant;

const SPS: [u8; 12] = [0x67, 0x42, 0xc0, 0x1e, 0xda, 0x02, 0x80, 0xbf, 0xe5, 0x84, 0x00, 0x00];
const PPS: [u8; 4] = [0x68, 0xce, 0x38, 0x80];

fn info(audio: bool) -> Arc<StreamInfo> {
    Arc::new(StreamInfo {
        name: "vs0".into(),
        sps: SPS.to_vec(),
        pps: PPS.to_vec(),
        width: 1280,
        height: 720,
        fps: 25,
        audio: if audio { Some(AudioInfo { rate: 8000, channels: 1, alaw: true }) } else { None },
    })
}

/// Deterministic slice payload, so the client can check reassembly.
fn slice(key: bool, size: usize) -> Vec<u8> {
    let mut v = vec![if key { 0x65 } else { 0x41 }];
    v.extend((0..size).map(|k| (k % 251) as u8));
    v
}

/// A Hub fed at 25 fps with a keyframe every `gop` frames. Odd-numbered
/// keyframes carry SPS/PPS in-band, even ones are a bare IDR, so both the
/// injection and the no-duplicate paths run. Stops when the hub is stopped.
fn feeder(gop: u64, size: usize, audio: bool) -> Arc<Hub> {
    let hub = Hub::new(info(audio));
    let h = hub.clone();
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut i = 0u64;
        while h.is_running() {
            let key = i % gop == 0;
            let mut nals = Vec::new();
            if key && (i / gop) % 2 == 1 {
                nals.push(SPS.to_vec());
                nals.push(PPS.to_vec());
            }
            nals.push(slice(key, size));
            let ts = start.elapsed().as_millis() as u64;
            h.publish(VideoFrame { nals, keyframe: key, ts_ms: ts, seq: 0 });
            if audio {
                h.publish_audio(AudioFrame { data: vec![0xd5; 320], ts_ms: ts, seq: 0 });
            }
            i += 1;
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    hub
}

fn start(src: Arc<dyn VideoSource>) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let a = l.local_addr().expect("addr");
    let cfg = Cfg::new(a);
    std::thread::spawn(move || {
        let _ = serve_listener(l, cfg, src);
    });
    a
}

#[derive(Debug)]
struct Resp {
    code: u16,
    head: String,
    body: String,
}

impl Resp {
    fn h(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
}

#[derive(Clone, Debug)]
struct Pkt {
    marker: bool,
    pt: u8,
    seq: u16,
    ts: u32,
    ssrc: u32,
    payload: Vec<u8>,
}

fn rtp(b: &[u8]) -> Pkt {
    assert!(b.len() >= 12, "short rtp");
    assert_eq!(b[0], 0x80, "V=2, no P/X/CC");
    Pkt {
        marker: b[1] & 0x80 != 0,
        pt: b[1] & 0x7f,
        seq: u16::from_be_bytes([b[2], b[3]]),
        ts: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        ssrc: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        payload: b[12..].to_vec(),
    }
}

struct Client {
    s: TcpStream,
    url: String,
    buf: Vec<u8>,
    cseq: u32,
    session: Option<String>,
    pkts: VecDeque<(u8, Vec<u8>)>,
    resps: VecDeque<Resp>,
    fu: Vec<u8>,
    /// Packets `au()` saw on other channels (RTCP, audio).
    other: Vec<(u8, Vec<u8>)>,
}

impl Client {
    fn new(addr: SocketAddr, path: &str) -> Client {
        let s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        Client {
            s,
            url: format!("rtsp://{}{}", addr, path),
            buf: Vec::new(),
            cseq: 0,
            session: None,
            pkts: VecDeque::new(),
            resps: VecDeque::new(),
            fu: Vec::new(),
            other: Vec::new(),
        }
    }

    /// Read once; split whatever is complete into packets and responses.
    fn fill(&mut self) -> bool {
        let mut tmp = [0u8; 65536];
        match self.s.read(&mut tmp) {
            Ok(0) => return false,
            Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            Err(_) => return false,
        }
        loop {
            if self.buf.first() == Some(&b'$') {
                if self.buf.len() < 4 {
                    break;
                }
                let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                if self.buf.len() < 4 + len {
                    break;
                }
                self.pkts.push_back((self.buf[1], self.buf[4..4 + len].to_vec()));
                self.buf.drain(..4 + len);
                continue;
            }
            let end = match self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                Some(e) => e,
                None => break,
            };
            let head = String::from_utf8(self.buf[..end].to_vec()).expect("utf8 head");
            assert!(head.starts_with("RTSP/1.0 "), "not a response: {:?}", head);
            let mut r = Resp { code: head[9..12].parse().unwrap(), head, body: String::new() };
            let clen: usize = r.h("Content-Length").map(|v| v.parse().unwrap()).unwrap_or(0);
            if self.buf.len() < end + 4 + clen {
                break;
            }
            r.body = String::from_utf8(self.buf[end + 4..end + 4 + clen].to_vec()).unwrap();
            self.buf.drain(..end + 4 + clen);
            self.resps.push_back(r);
        }
        true
    }

    fn raw(&mut self, bytes: &[u8]) -> Resp {
        self.s.write_all(bytes).unwrap();
        self.resp()
    }

    fn resp(&mut self) -> Resp {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(r) = self.resps.pop_front() {
                return r;
            }
            self.fill();
        }
        panic!("no response");
    }

    fn req(&mut self, method: &str, url: &str, extra: &[(&str, &str)]) -> Resp {
        self.cseq += 1;
        let mut m = format!("{} {} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: test\r\n", method, url, self.cseq);
        if let Some(s) = &self.session {
            m.push_str(&format!("Session: {}\r\n", s));
        }
        for (k, v) in extra {
            m.push_str(&format!("{}: {}\r\n", k, v));
        }
        m.push_str("\r\n");
        let r = self.raw(m.as_bytes());
        assert_eq!(r.h("CSeq"), Some(self.cseq.to_string()), "CSeq echoed");
        r
    }

    fn setup(&mut self, track: usize, transport: &str) -> Resp {
        let url = format!("{}/trackID={}", self.url, track);
        let r = self.req("SETUP", &url, &[("Transport", transport)]);
        if r.code == 200 && self.session.is_none() {
            let v = r.h("Session").unwrap();
            self.session = Some(v.split(';').next().unwrap().to_string());
        }
        r
    }

    fn play(&mut self) -> Resp {
        let url = format!("{}/", self.url);
        self.req("PLAY", &url, &[("Range", "npt=0.000-")])
    }

    /// OPTIONS, DESCRIBE, SETUP video on 0-1, PLAY: what camweb does.
    fn start_tcp(addr: SocketAddr) -> Client {
        let mut c = Client::new(addr, "/vs0");
        let u = c.url.clone();
        assert_eq!(c.req("OPTIONS", &u, &[]).code, 200);
        assert_eq!(c.req("DESCRIBE", &u, &[("Accept", "application/sdp")]).code, 200);
        assert_eq!(c.setup(0, "RTP/AVP/TCP;unicast;interleaved=0-1").code, 200);
        assert_eq!(c.play().code, 200);
        c
    }

    fn packet(&mut self, within: Duration) -> Option<(u8, Vec<u8>)> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(p) = self.pkts.pop_front() {
                return Some(p);
            }
            if Instant::now() >= deadline {
                return None;
            }
            self.fill();
        }
    }

    /// Next complete access unit on `ch`: its NALs and its packets.
    fn au(&mut self, ch: u8) -> (Vec<Vec<u8>>, Vec<Pkt>) {
        let mut nals = Vec::new();
        let mut pk = Vec::new();
        loop {
            let (c, b) = self.packet(Duration::from_secs(5)).expect("rtp packet");
            if c != ch {
                self.other.push((c, b));
                continue;
            }
            let p = rtp(&b);
            match p.payload[0] & 0x1f {
                28 => {
                    let (ind, fh) = (p.payload[0], p.payload[1]);
                    if fh & 0x80 != 0 {
                        self.fu.clear();
                        self.fu.push((ind & 0xe0) | (fh & 0x1f));
                    }
                    self.fu.extend_from_slice(&p.payload[2..]);
                    if fh & 0x40 != 0 {
                        nals.push(std::mem::take(&mut self.fu));
                    }
                }
                1..=23 => nals.push(p.payload.clone()),
                t => panic!("unexpected payload type {}", t),
            }
            let m = p.marker;
            pk.push(p);
            if m {
                return (nals, pk);
            }
        }
    }

    /// True if nothing at all arrives on `ch` for `quiet`.
    fn silent(&mut self, ch: u8, quiet: Duration) -> bool {
        let deadline = Instant::now() + quiet;
        while Instant::now() < deadline {
            if let Some((c, _)) = self.packet(Duration::from_millis(50)) {
                if c == ch {
                    return false;
                }
            }
        }
        true
    }

    fn drain(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.fill();
        }
        self.pkts.clear();
    }
}

fn types(nals: &[Vec<u8>]) -> Vec<u8> {
    nals.iter().map(|n| nal_type(n)).collect()
}

// ---------------------------------------------------------------------------

#[test]
fn options_lists_public_and_echoes_cseq() {
    let hub = feeder(10, 100, false);
    let addr = start(hub.clone());
    let mut c = Client::new(addr, "/vs0");
    let u = c.url.clone();
    let r = c.req("OPTIONS", &u, &[]);
    assert_eq!(r.code, 200);
    assert_eq!(r.h("Server").as_deref(), Some(Cfg::new(addr).server_name.as_str()));
    let public = r.h("Public").unwrap();
    for m in ["OPTIONS", "DESCRIBE", "SETUP", "PLAY", "PAUSE", "TEARDOWN", "GET_PARAMETER", "SET_PARAMETER"] {
        assert!(public.contains(m), "{} missing from {}", m, public);
    }
    // asterisk form and an odd CSeq value are echoed verbatim
    let r = c.raw(b"OPTIONS * RTSP/1.0\r\nCSeq: 0042\r\n\r\n");
    assert_eq!((r.code, r.h("CSeq").as_deref()), (200, Some("0042")));
    hub.stop();
}

#[test]
fn describe_serves_h264_sdp_on_any_single_segment_path() {
    let hub = feeder(10, 100, true);
    let addr = start(hub.clone());
    for path in ["/vs0", "/vs1", "/video0.sdp", "/"] {
        let mut c = Client::new(addr, path);
        let u = c.url.clone();
        let r = c.req("DESCRIBE", &u, &[("Accept", "application/sdp")]);
        assert_eq!(r.code, 200, "{}", path);
        assert_eq!(r.h("Content-Type").as_deref(), Some("application/sdp"));
        assert_eq!(r.h("Content-Length").unwrap(), r.body.len().to_string());
        let base = r.h("Content-Base").unwrap();
        assert!(base.ends_with('/') && !base.ends_with("//"), "{}", base);
        let sdp = &r.body;
        assert!(sdp.starts_with("v=0\r\n"));
        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"));
        let fmtp = format!(
            "a=fmtp:96 packetization-mode=1;profile-level-id=42c01e;sprop-parameter-sets={},{}\r\n",
            crate::b64::encode(&SPS),
            crate::b64::encode(&PPS)
        );
        assert!(sdp.contains(&fmtp), "{}", sdp);
        assert!(sdp.contains("m=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\n"));
        assert!(sdp.contains("a=control:trackID=0\r\n") && sdp.contains("a=control:trackID=1\r\n"));
    }
    let mut c = Client::new(addr, "/a/b/c");
    let u = c.url.clone();
    assert_eq!(c.req("DESCRIBE", &u, &[]).code, 404);
    let u = format!("rtsp://{}/vs0/nope", addr);
    assert_eq!(c.req("DESCRIBE", &u, &[]).code, 404);
    hub.stop();
}

#[test]
fn setup_tcp_interleaved_tracks_share_one_session() {
    let hub = feeder(10, 100, true);
    let addr = start(hub.clone());
    let mut c = Client::new(addr, "/vs0");
    let r = c.setup(0, "RTP/AVP/TCP;unicast;interleaved=0-1");
    assert_eq!(r.code, 200);
    let t = r.h("Transport").unwrap();
    assert!(t.starts_with("RTP/AVP/TCP;unicast;interleaved=0-1;ssrc="), "{}", t);
    let sess = r.h("Session").unwrap();
    assert!(sess.ends_with(";timeout=60"), "{}", sess);
    let id = sess.split(';').next().unwrap().to_string();
    assert!(id.len() >= 8 && id.bytes().all(|b| b.is_ascii_hexdigit()), "{}", id);
    // audio without explicit channels gets the next free pair
    let r = c.setup(1, "RTP/AVP/TCP;unicast");
    assert_eq!(r.code, 200);
    assert!(r.h("Transport").unwrap().contains("interleaved=2-3"));
    assert_eq!(r.h("Session").unwrap().split(';').next().unwrap(), id);
    // a clash with the other track's channels is refused
    let r = c.setup(1, "RTP/AVP/TCP;unicast;interleaved=1-2");
    assert_eq!(r.code, 461);

    // default channels when none are asked for, explicit odd ones honoured
    let mut d = Client::new(addr, "/vs0");
    assert!(d.setup(0, "RTP/AVP/TCP;unicast").h("Transport").unwrap().contains("interleaved=0-1"));
    let mut e = Client::new(addr, "/vs0");
    assert!(e.setup(0, "RTP/AVP/TCP;unicast;interleaved=6").h("Transport").unwrap().contains("interleaved=6-7"));
    assert_ne!(d.session, c.session);
    hub.stop();
}

#[test]
fn play_tcp_delivers_sequenced_marked_frames_and_sr() {
    let hub = feeder(10, 300, false);
    let addr = start(hub.clone());
    let mut c = Client::new(addr, "/vs0");
    let r = c.setup(0, "RTP/AVP/TCP;unicast;interleaved=0-1");
    let ssrc = u32::from_str_radix(r.h("Transport").unwrap().rsplit("ssrc=").next().unwrap(), 16).unwrap();
    let r = c.play();
    assert_eq!(r.code, 200);
    let info = r.h("RTP-Info").unwrap();
    let rtptime: u32 = info.rsplit("rtptime=").next().unwrap().parse().unwrap();
    let seq0: u16 = info.split("seq=").nth(1).unwrap().split(';').next().unwrap().parse().unwrap();

    let (first, pk) = c.au(0);
    assert_eq!(types(&first), vec![7, 8, 5], "stream starts at a keyframe with parameter sets");
    assert_eq!(first[0], SPS);
    assert_eq!(first[1], PPS);
    assert_eq!(pk[0].seq, seq0, "first seq matches RTP-Info");
    assert_eq!(pk[0].ts, rtptime, "first timestamp matches RTP-Info");
    let mut prev = pk.last().unwrap().clone();
    let mut prev_ts = pk[0].ts;
    for p in &pk {
        assert_eq!((p.pt, p.ssrc, p.ts), (96, ssrc, prev_ts));
    }
    assert!(pk[..pk.len() - 1].iter().all(|p| !p.marker));
    for _ in 0..12 {
        let (nals, pk) = c.au(0);
        assert!(nals.iter().any(|n| crate::media::is_vcl(nal_type(n))), "every marked AU holds a picture");
        for p in &pk {
            assert_eq!(p.seq, prev.seq.wrapping_add(1), "contiguous seq");
            assert_eq!((p.pt, p.ssrc), (96, ssrc));
            assert_eq!(p.ts, pk[0].ts, "one timestamp per AU");
            prev = p.clone();
        }
        assert!(pk.last().unwrap().marker && pk[..pk.len() - 1].iter().all(|p| !p.marker));
        let d = pk[0].ts.wrapping_sub(prev_ts);
        // 40 ms frames at 90 kHz = 3600 ticks; allow scheduler jitter.
        assert!((1500..=9000).contains(&d), "ts step {}", d);
        prev_ts = pk[0].ts;
    }
    // an RTCP SR on channel 1 for the same SSRC
    let mut sr = c.other.iter().find(|(ch, _)| *ch == 1).map(|(_, b)| b.clone());
    let deadline = Instant::now() + Duration::from_secs(6);
    while sr.is_none() && Instant::now() < deadline {
        if let Some((1, b)) = c.packet(Duration::from_millis(100)) {
            sr = Some(b);
        }
    }
    let sr = sr.expect("sender report");
    assert!(rtp::rtcp_valid(&sr));
    assert_eq!(sr[1], 200);
    assert_eq!(u32::from_be_bytes([sr[4], sr[5], sr[6], sr[7]]), ssrc);
    hub.stop();
}

#[test]
fn udp_transport_delivers_rtp_and_rtcp() {
    let hub = feeder(10, 3000, false);
    let addr = start(hub.clone());
    let rtp_s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let rtcp_s = UdpSocket::bind("127.0.0.1:0").unwrap();
    rtp_s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    rtcp_s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
    let (p, q) = (rtp_s.local_addr().unwrap().port(), rtcp_s.local_addr().unwrap().port());
    let mut c = Client::new(addr, "/vs0");
    let r = c.setup(0, &format!("RTP/AVP;unicast;client_port={}-{}", p, q));
    assert_eq!(r.code, 200);
    let t = r.h("Transport").unwrap();
    assert!(t.starts_with(&format!("RTP/AVP;unicast;client_port={}-{};server_port=", p, q)), "{}", t);
    let sp: u16 = t.split("server_port=").nth(1).unwrap().split('-').next().unwrap().parse().unwrap();
    // NAT-style hole punch and a receiver report to our server ports: ignored
    rtp_s.send_to(&[0xce, 0xfa, 0xed, 0xfe], ("127.0.0.1", sp)).unwrap();
    assert_eq!(c.play().code, 200);
    let mut buf = [0u8; 2048];
    let mut marked = 0;
    let mut first = None;
    while marked < 5 {
        let (n, _) = rtp_s.recv_from(&mut buf).expect("udp rtp");
        let pk = rtp(&buf[..n]);
        assert_eq!(pk.pt, 96);
        assert!(n <= 12 + rtp::MAX_PAYLOAD);
        first.get_or_insert(pk.payload[0] & 0x1f);
        marked += pk.marker as u32;
    }
    assert_eq!(first, Some(7), "first datagram is the SPS");
    let (n, _) = rtcp_s.recv_from(&mut buf).expect("udp rtcp sr");
    assert!(rtp::rtcp_valid(&buf[..n]));
    assert_eq!(buf[1], 200);
    hub.stop();
}

#[test]
fn large_nal_is_fragmented_and_reassembles() {
    let hub = feeder(5, 6000, false);
    let addr = start(hub.clone());
    let mut c = Client::start_tcp(addr);
    for _ in 0..6 {
        let (nals, pk) = c.au(0);
        let pic = nals.last().unwrap();
        assert_eq!(pic, &slice(nal_type(pic) == 5, 6000), "reassembled bytes");
        let frags: Vec<_> = pk.iter().filter(|p| p.payload[0] & 0x1f == 28).collect();
        assert_eq!(frags.len(), 5, "6000 bytes in 1398-byte fragments");
        assert!(frags[0].payload[1] & 0x80 != 0 && frags[4].payload[1] & 0x40 != 0);
        assert!(pk.iter().all(|p| p.payload.len() <= rtp::MAX_PAYLOAD));
        // small NALs (SPS/PPS) stay single-NAL packets
        assert!(pk.iter().filter(|p| p.payload[0] & 0x1f != 28).all(|p| p.payload.len() < 20));
    }
    hub.stop();
}

#[test]
fn parameter_sets_precede_every_keyframe_once() {
    let hub = feeder(3, 200, false);
    let addr = start(hub.clone());
    let mut c = Client::start_tcp(addr);
    let mut keys = 0;
    for _ in 0..14 {
        let (nals, _) = c.au(0);
        let t = types(&nals);
        if t.contains(&5) {
            // both bare-IDR (injected) and in-band keyframes look the same
            assert_eq!(t, vec![7, 8, 5]);
            assert_eq!((&nals[0][..], &nals[1][..]), (&SPS[..], &PPS[..]));
            keys += 1;
        } else {
            assert_eq!(t, vec![1]);
        }
    }
    assert!(keys >= 4, "{} keyframes", keys);
    hub.stop();
}

#[test]
fn hub_frames_without_pictures_are_not_sent() {
    let hub = Hub::new(info(false));
    let addr = start(hub.clone());
    let mut c = Client::start_tcp(addr);
    let h = hub.clone();
    std::thread::spawn(move || {
        for i in 0..40u64 {
            let nals = match i % 4 {
                0 => vec![slice(true, 50)],
                1 => vec![SPS.to_vec(), PPS.to_vec()], // parameter sets alone
                2 => vec![vec![0x0c, 0xff, 0xff]],     // filler alone
                _ => vec![slice(false, 50)],
            };
            let seq_key = i % 4 == 0;
            h.publish(VideoFrame { nals, keyframe: seq_key, ts_ms: i * 40, seq: 0 });
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    // Frames 1 and 2 of every 4 carry no picture: they are skipped, and that
    // leaves the stream keyframe → P only, still contiguous in RTP seq.
    let mut last_seq = None;
    for _ in 0..6 {
        let (nals, pk) = c.au(0);
        let t = types(&nals);
        assert!(t == vec![7, 8, 5] || t == vec![1], "{:?}", t);
        if let Some(l) = last_seq {
            assert_eq!(pk[0].seq, u16::wrapping_add(l, 1));
        }
        last_seq = Some(pk.last().unwrap().seq);
    }
    hub.stop();
}

#[test]
fn pause_stops_and_play_resumes_with_parameter_sets() {
    let hub = feeder(5, 200, false);
    let addr = start(hub.clone());
    let mut c = Client::start_tcp(addr);
    let mut last = None;
    for _ in 0..3 {
        let (_, pk) = c.au(0);
        last = Some(pk.last().unwrap().seq);
    }
    let u = format!("{}/", c.url);
    assert_eq!(c.req("PAUSE", &u, &[]).code, 200);
    // anything already in flight
    c.drain(Duration::from_millis(250));
    assert!(c.silent(0, Duration::from_millis(500)), "no RTP while paused");
    let r = c.play();
    assert_eq!(r.code, 200);
    let (nals, pk) = c.au(0);
    assert_eq!(types(&nals), vec![7, 8, 5], "resume starts with SPS, PPS, IDR");
    assert!(pk[0].seq.wrapping_sub(last.unwrap()) < 100, "seq continues after resume");
    hub.stop();
}

#[test]
fn teardown_ends_the_session() {
    let hub = feeder(5, 200, false);
    let addr = start(hub.clone());
    let mut c = Client::start_tcp(addr);
    c.au(0);
    let u = format!("{}/", c.url);
    assert_eq!(c.req("TEARDOWN", &u, &[]).code, 200);
    c.drain(Duration::from_millis(250));
    assert!(c.silent(0, Duration::from_millis(400)));
    // the old id is gone
    assert_eq!(c.req("PLAY", &u, &[]).code, 454);
    // the connection is still usable for a fresh session
    c.session = None;
    assert_eq!(c.setup(0, "RTP/AVP/TCP;unicast;interleaved=0-1").code, 200);
    assert_eq!(c.play().code, 200);
    assert_eq!(types(&c.au(0).0), vec![7, 8, 5]);
    hub.stop();
}

#[test]
fn error_codes() {
    let hub = feeder(10, 100, false);
    let addr = start(hub.clone());
    let mut c = Client::new(addr, "/vs0");
    let u = c.url.clone();
    let r = c.raw(b"DESCRIBE rtsp://x/vs0\r\nCSeq: 3\r\n\r\n");
    assert_eq!((r.code, r.h("CSeq").as_deref()), (400, Some("3")));
    assert_eq!(c.raw(b"hello there\r\n\r\n").code, 400);
    assert_eq!(c.raw(b"PLAY rtsp://x/vs0 RTSP/2.0\r\nCSeq: 4\r\n\r\n").code, 505);
    assert_eq!(c.raw(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").code, 400);
    let r = c.req("ANNOUNCE", &u, &[]);
    assert_eq!(r.code, 405);
    assert!(r.h("Allow").unwrap().contains("DESCRIBE"));
    assert_eq!(c.req("RECORD", &u, &[]).code, 405);
    assert_eq!(c.req("PLAY", &u, &[]).code, 455, "PLAY before SETUP");
    assert_eq!(c.req("PAUSE", &u, &[]).code, 455);
    assert_eq!(c.req("PLAY", &u, &[("Session", "DEADBEEF")]).code, 454);
    assert_eq!(c.req("SETUP", &format!("{}/trackID=0", u), &[]).code, 400, "no Transport");
    for t in [
        "RTP/SAVP;unicast;client_port=5000-5001",
        "RTP/AVP;multicast",
        "RTP/AVP/TCP;interleaved=0-1;mode=record",
        "RTP/AVP;unicast",
    ] {
        assert_eq!(c.setup(0, t).code, 461, "{}", t);
    }
    assert_eq!(c.setup(1, "RTP/AVP/TCP;interleaved=0-1").code, 404, "no audio track");
    assert_eq!(c.setup(0, "RTP/AVP/TCP;interleaved=0-1").code, 200);
    let real = c.session.clone();
    c.session = Some("0123".into());
    assert_eq!(c.play().code, 454, "wrong session id");
    assert_eq!(c.req("GET_PARAMETER", &u, &[]).code, 454);
    c.session = None;
    assert_eq!(c.req("PLAY", &u, &[]).code, 454, "missing session id");
    c.session = real;
    assert_eq!(c.req("GET_PARAMETER", &u, &[]).code, 200);
    let r = c.raw(
        format!("SET_PARAMETER {} RTSP/1.0\r\nCSeq: 99\r\nContent-Length: 6\r\n\r\nfoo: 1", u).as_bytes(),
    );
    assert_eq!(r.code, 451);
    // a header block that never ends: 400 and the connection closes
    let mut junk = b"OPTIONS * RTSP/1.0\r\nX: ".to_vec();
    junk.extend(std::iter::repeat(b'a').take(9000));
    let r = c.raw(&junk);
    assert_eq!(r.code, 400);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut closed = false;
    while Instant::now() < deadline && !closed {
        let mut b = [0u8; 64];
        closed = match c.s.read(&mut b) {
            Ok(0) => true,
            Err(e) => e.kind() == std::io::ErrorKind::ConnectionReset,
            Ok(_) => false,
        };
    }
    assert!(closed, "server closes after losing framing");
    hub.stop();
}

#[test]
fn audio_and_video_on_separate_channels() {
    let hub = feeder(5, 200, true);
    let addr = start(hub.clone());
    let mut c = Client::new(addr, "/vs0");
    assert_eq!(c.setup(0, "RTP/AVP/TCP;unicast;interleaved=0-1").code, 200);
    assert_eq!(c.setup(1, "RTP/AVP/TCP;unicast;interleaved=2-3").code, 200);
    assert_eq!(c.play().code, 200);
    let mut audio = Vec::new();
    let mut video = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while (audio.len() < 10 || video < 10) && Instant::now() < deadline {
        match c.packet(Duration::from_millis(200)) {
            Some((0, _)) => video += 1,
            Some((2, b)) => audio.push(rtp(&b)),
            _ => {}
        }
    }
    assert!(video >= 10 && audio.len() >= 10, "video {} audio {}", video, audio.len());
    for w in audio.windows(2) {
        assert_eq!(w[1].seq, w[0].seq.wrapping_add(1));
        let d = w[1].ts.wrapping_sub(w[0].ts);
        assert!((100..=800).contains(&d), "a-law ts step {}", d);
    }
    assert!(audio.iter().all(|p| p.pt == 8 && p.marker && p.payload.len() == 320));
    hub.stop();
}

#[test]
fn two_clients_both_receive() {
    let hub = feeder(5, 500, false);
    let addr = start(hub.clone());
    let mut a = Client::start_tcp(addr);
    let mut b = Client::start_tcp(addr);
    assert_ne!(a.session, b.session);
    let ta = std::thread::spawn(move || {
        let ssrc = a.au(0).1[0].ssrc;
        for _ in 0..10 {
            a.au(0);
        }
        ssrc
    });
    let sb = b.au(0).1[0].ssrc;
    for _ in 0..10 {
        b.au(0);
    }
    let sa = ta.join().unwrap();
    assert_ne!(sa, sb, "random SSRC per session");
    hub.stop();
}

#[test]
fn stalled_and_vanishing_clients_do_not_disturb_others() {
    // ~25 kB frames at 25 fps: a client that stops reading fills its socket
    // buffers within a few seconds.
    let hub = feeder(10, 25_000, false);
    let addr = start(hub.clone());
    let mut good = Client::start_tcp(addr);
    let mut stalled = Client::start_tcp(addr);
    let mut gone = Client::start_tcp(addr);
    stalled.au(0);
    gone.au(0);
    good.au(0);
    // `gone` vanishes without TEARDOWN mid-stream; `stalled` stops reading.
    drop(gone);
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_secs(4) {
        good.au(0);
        n += 1;
    }
    // 4 s at 25 fps is 100 frames; demand most of them.
    assert!(n >= 70, "healthy client got only {} frames in 4 s", n);
    // and keeps receiving after the stalled one finally goes away too
    drop(stalled);
    for _ in 0..10 {
        good.au(0);
    }
    hub.stop();
}

#[test]
fn source_end_sends_rtcp_bye() {
    let hub = Hub::new(info(false));
    let addr = start(hub.clone());
    let mut c = Client::start_tcp(addr);
    let h = hub.clone();
    std::thread::spawn(move || {
        for i in 0..10u64 {
            h.publish(VideoFrame { nals: vec![slice(i == 0, 100)], keyframe: i == 0, ts_ms: i * 40, seq: 0 });
            std::thread::sleep(Duration::from_millis(40));
        }
        h.stop();
    });
    let mut aus = 0;
    let mut bye = false;
    while let Some((ch, b)) = c.packet(Duration::from_secs(3)) {
        if ch == 0 && rtp(&b).marker {
            aus += 1;
        }
        // walk the compound looking for PT 203
        let mut i = 0;
        while ch == 1 && i + 4 <= b.len() {
            bye |= b[i + 1] == 203;
            i += (u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize + 1) * 4;
        }
        if bye {
            break;
        }
    }
    assert!(bye, "BYE after the source ended");
    assert_eq!(aus, 10, "every frame before the BYE");
    // a session started after the end gets its BYE straight away
    let mut d = Client::start_tcp(addr);
    let (ch, b) = d.packet(Duration::from_secs(3)).expect("bye");
    assert_eq!((ch, b[1]), (1, 200));
}

#[test]
fn client_garbage_and_bad_rtcp_are_ignored() {
    let hub = feeder(5, 200, false);
    let addr = start(hub.clone());
    let mut c = Client::start_tcp(addr);
    c.au(0);
    // a valid RR, an RR with a lying length, random bytes, an oversized
    // frame on the RTP channel, and stray CRLFs
    let mut rr = vec![b'$', 1, 0, 32, 0x81, 201, 0, 7];
    rr.extend_from_slice(&[0; 28]);
    c.s.write_all(&rr).unwrap();
    c.s.write_all(&[b'$', 1, 0, 8, 0x80, 200, 0xff, 0xff, 1, 2, 3, 4]).unwrap();
    c.s.write_all(&[b'$', 1, 0, 5, 0xde, 0xad, 0xbe, 0xef, 0x00]).unwrap();
    let mut big = vec![b'$', 0, 0x0b, 0xb8];
    big.extend(std::iter::repeat(0x5a).take(3000));
    c.s.write_all(&big).unwrap();
    c.s.write_all(b"\r\n\r\n").unwrap();
    for _ in 0..5 {
        c.au(0);
    }
    let u = c.url.clone();
    assert_eq!(c.req("GET_PARAMETER", &u, &[]).code, 200, "control channel still in sync");
    c.au(0);
    hub.stop();
}
