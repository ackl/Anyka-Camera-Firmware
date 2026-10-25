//! The hardware encoder side: where H.264 access units are supposed to come
//! from. See `tools/rust/camrtsp/README.md` — the vendor MPI libraries are
//! uClibc dynamic objects, so the pump that calls them lives behind a socket
//! and this process only ever reads frames.
//!
//! Wire format (unix SOCK_STREAM, or TCP when `--pump host:port`):
//!   magic "CAMRTSP1" | u32 flags (bit0 alaw audio) | u32 fps | u32 w | u32 h
//!   u16 sps_len | sps | u16 pps_len | pps
//!   then repeated: u8 kind (1=video,2=audio) | u64 ts_ms | u32 len | payload
//! Video payloads are Annex-B access units as the encoder emits them.
//!
//! All integers are little-endian: the pump is a C program on the same ARM
//! and writes its native layout. An address containing `/` is a unix socket
//! path, anything else is `host:port`.
//!
//! Anything that does not parse (wrong magic, absurd lengths, unknown record
//! kind) drops the connection; the reader reconnects with backoff. The Hub's
//! `StreamInfo` comes from the first header and stays fixed.

use crate::media::{nal_type, split_au, AudioFrame, AudioInfo, StreamInfo, VideoFrame};
use crate::source::Hub;
use std::io::{self, Read};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"CAMRTSP1";
const MAX_PS: usize = 1024;
const MAX_VIDEO: usize = 2 * 1024 * 1024;
const MAX_AUDIO: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct Cfg {
    /// How long `connect()` waits for the first header.
    header_wait: Duration,
    /// Read timeout: the tick at which the reader checks for a dead Hub.
    io_timeout: Duration,
    /// No record for this long -> reconnect.
    watchdog: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
}

impl Default for Cfg {
    fn default() -> Cfg {
        Cfg {
            header_wait: Duration::from_secs(10),
            io_timeout: Duration::from_millis(500),
            watchdog: Duration::from_secs(10),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(8),
        }
    }
}

/// Connect to the pump, wait for its header (up to ~10 s), and return a Hub
/// that a background thread keeps fed, reconnecting as needed.
pub fn connect(addr: &str) -> io::Result<Arc<Hub>> {
    connect_with(addr, Cfg::default())
}

fn connect_with(addr: &str, cfg: Cfg) -> io::Result<Arc<Hub>> {
    let addr = addr.trim().to_string();
    if addr.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "pump: empty address"));
    }
    let (tx, rx) = mpsc::sync_channel::<io::Result<Arc<Hub>>>(1);
    let wait = cfg.header_wait;
    thread::Builder::new()
        .name("venc-pump".into())
        .spawn(move || Reader::new(addr, cfg).run(tx))?;
    match rx.recv_timeout(wait + Duration::from_secs(2)) {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "pump: no header in time")),
    }
}

enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Conn {
    fn open(addr: &str, timeout: Duration) -> io::Result<Conn> {
        let c = if addr.contains('/') {
            Conn::Unix(UnixStream::connect(addr)?)
        } else {
            let mut last = None;
            let mut s = None;
            for a in std::net::ToSocketAddrs::to_socket_addrs(addr)? {
                match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
                    Ok(x) => {
                        s = Some(x);
                        break;
                    }
                    Err(e) => last = Some(e),
                }
            }
            match s {
                Some(s) => Conn::Tcp(s),
                None => {
                    return Err(last.unwrap_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "address resolved to nothing")
                    }))
                }
            }
        };
        match &c {
            Conn::Tcp(s) => s.set_read_timeout(Some(timeout))?,
            Conn::Unix(s) => s.set_read_timeout(Some(timeout))?,
        }
        Ok(c)
    }
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(b),
            Conn::Unix(s) => s.read(b),
        }
    }
}

struct Header {
    alaw: bool,
    fps: u32,
    w: u32,
    h: u32,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("pump: {}", msg))
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

enum End {
    Abandoned,
}

struct Reader {
    addr: String,
    cfg: Cfg,
    hub: Option<Arc<Hub>>,
    deadline: Instant,
    last_ts: u64,
    last_pub: Option<Instant>,
    produced: bool,
}

impl Reader {
    fn new(addr: String, cfg: Cfg) -> Reader {
        Reader {
            deadline: Instant::now() + cfg.header_wait,
            addr,
            cfg,
            hub: None,
            last_ts: 0,
            last_pub: None,
            produced: false,
        }
    }

    fn abandoned(&self) -> bool {
        match &self.hub {
            Some(h) => Arc::strong_count(h) == 1 || !h.is_running(),
            None => false,
        }
    }

    fn run(mut self, tx: mpsc::SyncSender<io::Result<Arc<Hub>>>) {
        let mut tx = Some(tx);
        let mut backoff = self.cfg.backoff_min;
        loop {
            self.produced = false;
            let err = match self.session(&mut tx) {
                Ok(End::Abandoned) => return,
                Err(e) => e,
            };
            if self.abandoned() {
                return;
            }
            if self.hub.is_none() {
                // Before the first header: a wrong magic is not a pump at all,
                // anything else may be one that is still starting.
                let fatal = err.kind() == io::ErrorKind::InvalidData;
                if fatal || Instant::now() >= self.deadline {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(Err(err));
                    }
                    return;
                }
            }
            if self.produced {
                backoff = self.cfg.backoff_min;
            }
            eprintln!("camrtsp: pump {}: {}; retry in {} ms", self.addr, err, backoff.as_millis());
            let until = Instant::now() + backoff;
            while Instant::now() < until {
                if self.abandoned() {
                    return;
                }
                thread::sleep(self.cfg.io_timeout.min(until - Instant::now()));
            }
            backoff = (backoff * 2).min(self.cfg.backoff_max);
        }
    }

    /// Fill `buf` completely. Read timeouts are tolerated (partial reads
    /// keep their progress) until the watchdog fires or the Hub is gone.
    fn read_full(&self, c: &mut Conn, buf: &mut [u8], last: &mut Instant) -> io::Result<Option<End>> {
        let mut got = 0;
        while got < buf.len() {
            if self.abandoned() {
                return Ok(Some(End::Abandoned));
            }
            if self.hub.is_none() && Instant::now() >= self.deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no header from pump"));
            }
            match c.read(&mut buf[got..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        if got == 0 { "pump closed the connection" } else { "pump closed mid-record" },
                    ))
                }
                Ok(n) => {
                    got += n;
                    *last = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    if last.elapsed() > self.cfg.watchdog {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "pump went silent"));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    fn read_header(&self, c: &mut Conn, last: &mut Instant) -> io::Result<Result<Header, End>> {
        let mut fixed = [0u8; 8 + 16 + 2];
        if let Some(e) = self.read_full(c, &mut fixed, last)? {
            return Ok(Err(e));
        }
        if &fixed[..8] != MAGIC {
            return Err(bad(format!("bad magic {:02x?}", &fixed[..8])));
        }
        let flags = u32le(&fixed[8..]);
        let fps = u32le(&fixed[12..]);
        let (w, h) = (u32le(&fixed[16..]), u32le(&fixed[20..]));
        if fps > 240 || w > 8192 || h > 8192 {
            return Err(bad(format!("implausible header fps {} {}x{}", fps, w, h)));
        }
        let sps_len = u16le(&fixed[24..]) as usize;
        if sps_len > MAX_PS {
            return Err(bad(format!("sps of {} bytes", sps_len)));
        }
        let mut sps = vec![0u8; sps_len + 2];
        if let Some(e) = self.read_full(c, &mut sps, last)? {
            return Ok(Err(e));
        }
        let pps_len = u16le(&sps[sps_len..]) as usize;
        sps.truncate(sps_len);
        if pps_len > MAX_PS {
            return Err(bad(format!("pps of {} bytes", pps_len)));
        }
        let mut pps = vec![0u8; pps_len];
        if let Some(e) = self.read_full(c, &mut pps, last)? {
            return Ok(Err(e));
        }
        Ok(Ok(Header { alaw: flags & 1 != 0, fps, w, h, sps, pps }))
    }

    fn session(&mut self, tx: &mut Option<mpsc::SyncSender<io::Result<Arc<Hub>>>>) -> io::Result<End> {
        let mut c = Conn::open(&self.addr, self.cfg.io_timeout)?;
        let mut last = Instant::now();
        let hdr = match self.read_header(&mut c, &mut last)? {
            Ok(h) => h,
            Err(e) => return Ok(e),
        };
        if self.hub.is_none() {
            let hub = Hub::new(Arc::new(StreamInfo {
                name: "vs0".to_string(),
                sps: hdr.sps,
                pps: hdr.pps,
                width: hdr.w,
                height: hdr.h,
                fps: if hdr.fps == 0 { 15 } else { hdr.fps },
                audio: if hdr.alaw {
                    Some(AudioInfo { rate: 8000, channels: 1, alaw: true })
                } else {
                    None
                },
            }));
            match tx.take() {
                Some(t) if t.send(Ok(hub.clone())).is_ok() => self.hub = Some(hub),
                // connect() already gave up: nobody will ever read this
                _ => return Ok(End::Abandoned),
            }
        }
        let hub = match self.hub.clone() {
            Some(h) => h,
            None => return Ok(End::Abandoned),
        };
        let audio = hub.info.audio.is_some();

        // Pump timestamps pass through, shifted after a reconnect so they
        // stay monotonic even when the pump restarted its clock.
        let mut shift: Option<i64> = None;
        let mut rec = [0u8; 13];
        loop {
            if let Some(e) = self.read_full(&mut c, &mut rec, &mut last)? {
                return Ok(e);
            }
            let kind = rec[0];
            let ts = u64::from_le_bytes([rec[1], rec[2], rec[3], rec[4], rec[5], rec[6], rec[7], rec[8]]);
            let len = u32le(&rec[9..]) as usize;
            let max = match kind {
                1 => MAX_VIDEO,
                2 => MAX_AUDIO,
                k => return Err(bad(format!("unknown record kind {}", k))),
            };
            if len > max {
                return Err(bad(format!("record of {} bytes", len)));
            }
            let mut payload = vec![0u8; len];
            if let Some(e) = self.read_full(&mut c, &mut payload, &mut last)? {
                return Ok(e);
            }
            let shift = *shift.get_or_insert_with(|| match self.last_pub {
                None => 0,
                Some(t) => {
                    let resume = self.last_ts + t.elapsed().as_millis() as u64 + 1;
                    resume as i64 - ts as i64
                }
            });
            let ts = ((ts as i64).saturating_add(shift)).max(self.last_ts as i64) as u64;
            if kind == 1 {
                let nals = split_au(&payload);
                if nals.is_empty() {
                    continue;
                }
                let keyframe = nals.iter().any(|n| matches!(nal_type(n), 5 | 7));
                hub.publish(VideoFrame { nals, keyframe, ts_ms: ts, seq: 0 });
            } else if audio && !payload.is_empty() {
                hub.publish_audio(AudioFrame { data: payload, ts_ms: ts, seq: 0 });
            } else {
                continue;
            }
            self.last_ts = ts;
            self.last_pub = Some(Instant::now());
            self.produced = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::VideoSource;
    use std::io::Write;
    use std::net::TcpListener;

    fn header(alaw: bool) -> Vec<u8> {
        let mut v = MAGIC.to_vec();
        for x in [alaw as u32, 15, 1280, 720] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        for ps in [&[0x67u8, 0x42, 0, 0x1e][..], &[0x68, 0xce, 0x38, 0x80]] {
            v.extend_from_slice(&(ps.len() as u16).to_le_bytes());
            v.extend_from_slice(ps);
        }
        v
    }

    fn record(kind: u8, ts: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&ts.to_le_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn cfg() -> Cfg {
        Cfg {
            header_wait: Duration::from_millis(800),
            io_timeout: Duration::from_millis(50),
            watchdog: Duration::from_secs(3),
            backoff_min: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
        }
    }

    /// Serve `conns` connections, each fed `script(conn)`'s bytes in torn
    /// chunks, then closed (or held open when `hold`).
    fn pump(conns: Vec<Vec<u8>>, hold: bool) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            for bytes in conns {
                let Ok((mut s, _)) = l.accept() else { return };
                let _ = s.set_nodelay(true);
                for c in bytes.chunks(7) {
                    if s.write_all(c).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_micros(300));
                }
                if hold {
                    thread::sleep(Duration::from_secs(2));
                }
            }
        });
        port
    }

    fn wait_frames(hub: &Hub, n: u64) {
        let t0 = Instant::now();
        while hub.frames_published() < n {
            assert!(t0.elapsed() < Duration::from_secs(4), "only {} frames", hub.frames_published());
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn valid_stream() {
        let mut s = header(true);
        s.extend(record(1, 1000, &[0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 9, 9]));
        s.extend(record(2, 1010, &[0xd5; 160]));
        s.extend(record(2, 1030, &[0xd4; 160]));
        s.extend(record(1, 1066, &[0, 0, 0, 1, 0x41, 7]));
        let port = pump(vec![s], true);
        let hub = connect_with(&format!("127.0.0.1:{}", port), cfg()).expect("connect");
        let i = hub.info();
        assert_eq!((i.width, i.height, i.fps), (1280, 720, 15));
        assert_eq!(i.sps, vec![0x67, 0x42, 0, 0x1e]);
        assert_eq!(i.pps, vec![0x68, 0xce, 0x38, 0x80]);
        assert!(i.audio.is_some());
        wait_frames(&hub, 2);
        let f = hub.latest().unwrap();
        assert_eq!((f.seq, f.ts_ms, f.keyframe), (1, 1066, false));
        assert_eq!(f.nals, vec![vec![0x41, 7]]);
        let a = hub.next_audio(0, Duration::from_secs(2)).expect("audio");
        assert_eq!((a.seq, a.ts_ms, a.data.len(), a.data[0]), (1, 1030, 160, 0xd4));
        drop(hub);
    }

    #[test]
    fn reconnects_and_keeps_time_monotonic() {
        let mut a = header(false);
        a.extend(record(1, 5000, &[0, 0, 0, 1, 0x65, 1]));
        let mut b = header(false);
        b.extend(record(1, 0, &[0, 0, 0, 1, 0x41, 2])); // pump restarted its clock
        let port = pump(vec![a, b], true);
        let hub = connect_with(&format!("127.0.0.1:{}", port), cfg()).expect("connect");
        wait_frames(&hub, 2);
        let f = hub.latest().unwrap();
        assert_eq!(f.nals, vec![vec![0x41, 2]]);
        assert!(f.ts_ms > 5000, "ts {}", f.ts_ms);
    }

    #[test]
    fn truncated_header_is_an_error() {
        let h = header(false);
        let port = pump(vec![h[..13].to_vec(); 20], false);
        let t0 = Instant::now();
        let e = connect_with(&format!("127.0.0.1:{}", port), cfg()).err().expect("error");
        assert!(t0.elapsed() < Duration::from_secs(3));
        assert!(
            matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::TimedOut),
            "{}",
            e
        );
    }

    #[test]
    fn wrong_magic_is_an_error() {
        let mut h = header(false);
        h[..8].copy_from_slice(b"RTSP/1.0");
        let port = pump(vec![h], true);
        let e = connect_with(&format!("127.0.0.1:{}", port), cfg()).err().expect("error");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("magic"), "{}", e);
    }

    #[test]
    fn mid_record_close_then_garbage() {
        let mut a = header(false);
        a.extend(record(1, 100, &[0, 0, 0, 1, 0x65, 1]));
        let r = record(1, 166, &[0, 0, 0, 1, 0x41, 2, 3, 4, 5, 6]);
        a.extend_from_slice(&r[..r.len() - 3]); // cut inside the payload
        let mut b = header(false);
        b.extend(record(9, 0, &[1, 2, 3])); // unknown kind: dropped connection
        let mut c = header(false);
        c.extend(record(1, 300, &[0, 0, 0, 1, 0x41, 7]));
        let mut d = header(false);
        d.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]); // 2 GiB
        let port = pump(vec![a, b, c, d], false);
        let hub = connect_with(&format!("127.0.0.1:{}", port), cfg()).expect("connect");
        wait_frames(&hub, 2);
        let f = hub.latest().unwrap();
        assert_eq!(f.nals, vec![vec![0x41, 7]], "the torn record never surfaced");
        // the 2 GiB claim is refused without allocating; reader keeps going
        thread::sleep(Duration::from_millis(300));
        assert_eq!(hub.frames_published(), 2);
    }

    #[test]
    fn unix_socket() {
        let dir = std::env::temp_dir().join(format!("camrtsp-venc-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pump.sock");
        let _ = std::fs::remove_file(&path);
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut v = header(false);
                v.extend(record(1, 7, &[0, 0, 0, 1, 0x65, 1]));
                let _ = s.write_all(&v);
                thread::sleep(Duration::from_secs(1));
            }
        });
        let hub = connect_with(path.to_str().unwrap(), cfg()).expect("connect");
        wait_frames(&hub, 1);
        assert!(hub.latest().unwrap().keyframe);
        drop(hub);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
