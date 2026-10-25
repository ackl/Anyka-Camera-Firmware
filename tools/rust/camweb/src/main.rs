//! camweb — live video, PTZ and LED control panel for the Anyka CB211 camera.
//!
//! The host-side half of the project. It speaks RTSP itself (see `rtsp.rs`, pure
//! std: no ffmpeg RTSP, no crates), hands the H.264 it receives to an ffmpeg
//! child that only decodes and re-encodes to JPEG, and serves the frames to a
//! browser as multipart MJPEG. Controls go to the board over its stock telnet
//! (`telnet.rs`): the Rust `motorctl` for the motors, sysfs for the LED pins.
//!
//! Why a host panel: the camera is 36 MB of RAM on a single ARM926 core with no
//! JPEG path of its own, and it is only reachable over tailscale. Decoding and
//! the HTTP serving happen on firefly; the board is left to capture.
//!
//!   camweb serve [--listen 0.0.0.0:8090] [--rtsp URL] [--fps N] [--width N]
//!   camweb snap [OUT.jpg] [--ch vs0|vs1]
//!   camweb probe [--url URL]
//!   camweb sh 'board command'

mod rtsp;
mod telnet;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use telnet::Board;

const PANEL_HTML: &str = include_str!("panel.html");
const MOTORCTL: &str = "/mnt/Factory/custom/motorctl";

// ---------------------------------------------------------------- configuration

#[derive(Clone)]
struct Config {
    listen: String,
    rtsp: String,
    fps: u32,
    width: u32,
    quality: u32,
    board: String,
    /// "rust" = our own RTSP client, "ffmpeg" = let ffmpeg read RTSP (fallback).
    source: String,
}

impl Config {
    fn from_env_and_args(args: &[String]) -> Config {
        let mut cfg = Config {
            listen: std::env::var("CAMWEB_LISTEN").unwrap_or_else(|_| "0.0.0.0:8090".into()),
            rtsp: std::env::var("CAMWEB_RTSP")
                .unwrap_or_else(|_| "rtsp://192.168.1.103:554/vs0".into()),
            fps: std::env::var("CAMWEB_FPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            width: std::env::var("CAMWEB_WIDTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(960),
            quality: std::env::var("CAMWEB_QUALITY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6),
            board: std::env::var("CAM_HOST").unwrap_or_else(|_| telnet::DEFAULT_HOST.into()),
            source: std::env::var("CAMWEB_SOURCE").unwrap_or_else(|_| "rust".into()),
        };
        let mut i = 0;
        while i < args.len() {
            let v = args.get(i + 1).cloned().unwrap_or_default();
            match args[i].as_str() {
                "--listen" => cfg.listen = v,
                "--rtsp" | "--url" => cfg.rtsp = v,
                "--fps" => cfg.fps = v.parse().unwrap_or(cfg.fps),
                "--width" => cfg.width = v.parse().unwrap_or(cfg.width),
                "--quality" => cfg.quality = v.parse().unwrap_or(cfg.quality),
                "--host" => cfg.board = v,
                "--source" => cfg.source = v,
                _ => {}
            }
            i += 2;
        }
        cfg
    }
}

/// rtsp://host:554/vs0 + "vs1" -> rtsp://host:554/vs1
fn channel_url(base: &str, channel: &str) -> String {
    match base.rfind('/') {
        Some(pos) => format!("{}/{}", &base[..pos], channel),
        None => format!("{}/{}", base, channel),
    }
}

// ------------------------------------------------------------------------ hub

struct Sub {
    slot: Mutex<Option<Arc<Vec<u8>>>>,
    cv: Condvar,
}

struct Hub {
    cfg: Config,
    channel: Mutex<String>,
    restart: AtomicBool,
    latest: Mutex<Option<Arc<Vec<u8>>>>,
    subs: Mutex<Vec<Arc<Sub>>>,
    frames: AtomicU64,
    nals: AtomicU64,
    dropped_nals: AtomicU64,
    fps: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    online: AtomicBool,
    started: Instant,
    /// Frame rate limit, applied here rather than with ffmpeg's `fps=` filter: a
    /// raw H.264 pipe carries no timestamps, so ffmpeg assumes a 25 fps timebase
    /// and `fps=12` then emits 12 * (real_fps/25) — 7 fps on a 14.7 fps stream.
    /// Decode everything, publish at the rate we want.
    min_frame_us: u64,
    next_pub_us: AtomicU64,
    ptz_lock: Mutex<()>,
    last_ptz: Mutex<String>,
    last_err: Mutex<String>,
}

impl Hub {
    fn new(cfg: Config) -> Arc<Hub> {
        Arc::new(Hub {
            cfg: cfg.clone(),
            channel: Mutex::new("vs0".into()),
            restart: AtomicBool::new(false),
            latest: Mutex::new(None),
            subs: Mutex::new(Vec::new()),
            frames: AtomicU64::new(0),
            nals: AtomicU64::new(0),
            dropped_nals: AtomicU64::new(0),
            fps: AtomicU32::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            online: AtomicBool::new(false),
            min_frame_us: if cfg.fps == 0 { 0 } else { 1_000_000 / cfg.fps as u64 },
            next_pub_us: AtomicU64::new(0),
            started: Instant::now(),
            ptz_lock: Mutex::new(()),
            last_ptz: Mutex::new(String::new()),
            last_err: Mutex::new(String::new()),
        })
    }

    fn publish(&self, jpeg: Vec<u8>) {
        if self.min_frame_us > 0 {
            // Publish the first frame at or after each tick and drop the rest, so
            // the rate is the rate. (Measuring "is it too soon since the last one"
            // instead looks equivalent but is not: the camera delivers frames in
            // bursts of two, so every second frame was the one being dropped and
            // a 12 fps cap turned into 7.)
            let now_us = self.started.elapsed().as_micros() as u64;
            let next = self.next_pub_us.load(Ordering::Relaxed);
            if now_us < next {
                return;
            }
            // Advance the tick grid instead of resetting it to "now": resetting
            // makes every publish wait a full interval, which halves the rate
            // whenever the stream's own interval is close to it (66 ms frames vs
            // an 83 ms cap gave 7 fps, not 12). Catch up, never burst.
            let mut new_next = next + self.min_frame_us;
            if new_next <= now_us {
                new_next = now_us + self.min_frame_us;
            }
            self.next_pub_us.store(new_next, Ordering::Relaxed);
        }
        if let Some((w, h)) = jpeg_dimensions(&jpeg) {
            self.width.store(w, Ordering::Relaxed);
            self.height.store(h, Ordering::Relaxed);
        }
        let frame = Arc::new(jpeg);
        *self.latest.lock().unwrap() = Some(frame.clone());
        self.frames.fetch_add(1, Ordering::Relaxed);
        let subs = self.subs.lock().unwrap();
        for sub in subs.iter() {
            let mut slot = sub.slot.lock().unwrap();
            *slot = Some(frame.clone());
            sub.cv.notify_all();
        }
    }

    fn subscribe(&self) -> Arc<Sub> {
        let sub = Arc::new(Sub {
            slot: Mutex::new(self.latest.lock().unwrap().clone()),
            cv: Condvar::new(),
        });
        self.subs.lock().unwrap().push(sub.clone());
        sub
    }

    fn unsubscribe(&self, sub: &Arc<Sub>) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| !Arc::ptr_eq(s, sub));
    }

    fn board(&self) -> Board {
        Board::new(&self.cfg.board, telnet::DEFAULT_PORT)
    }

    /// Run a board command, recording failures for the status line.
    fn board_run(&self, cmd: &str) -> Result<(i32, String), String> {
        match self.board().run(cmd, Duration::from_secs(30)) {
            Ok(v) => Ok(v),
            Err(e) => {
                *self.last_err.lock().unwrap() = format!("board: {}", e);
                Err(e.to_string())
            }
        }
    }
}

fn jpeg_dimensions(j: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < j.len() {
        if j[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = j[i + 1];
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            let h = ((j[i + 5] as u32) << 8) | j[i + 6] as u32;
            let w = ((j[i + 7] as u32) << 8) | j[i + 8] as u32;
            return Some((w, h));
        }
        let len = ((j[i + 2] as u32) << 8) | j[i + 3] as u32;
        i += 2 + len.max(2) as usize;
    }
    None
}

// ------------------------------------------------------------------- pipeline

fn pipeline_loop(hub: Arc<Hub>) {
    loop {
        let channel = hub.channel.lock().unwrap().clone();
        let url = channel_url(&hub.cfg.rtsp, &channel);
        hub.restart.store(false, Ordering::SeqCst);
        eprintln!("camweb: connecting {} (source: {})", url, hub.cfg.source);
        let result = if hub.cfg.source == "ffmpeg" {
            run_ffmpeg_source(&hub, &url)
        } else {
            run_pipeline(&hub, &url)
        };
        match result {
            Ok(()) => eprintln!("camweb: pipeline stopped (restart requested)"),
            Err(e) => {
                eprintln!("camweb: pipeline error: {}", e);
                *hub.last_err.lock().unwrap() = e;
            }
        }
        hub.online.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// One RTSP session: connect, spawn the decoder, pump NALs into it.
fn run_pipeline(hub: &Arc<Hub>, url: &str) -> Result<(), String> {
    let mut session = rtsp::Session::connect(url, &["video"], Duration::from_millis(500))
        .map_err(|e| format!("rtsp connect: {}", e))?;

    let mut decoder = spawn_decoder(&hub.cfg)?;

    // The camera repeats SPS/PPS in-band at every IDR, but the NALs right after
    // PLAY may not start at one, and a raw H.264 pipe cannot decode without them.
    if let Some((sps, pps)) = session.sprop_ps() {
        for ps in [&sps, &pps] {
            if !ps.is_empty() {
                let mut annexb = Vec::with_capacity(ps.len() + 4);
                annexb.extend_from_slice(&[0, 0, 0, 1]);
                annexb.extend_from_slice(ps);
                let _ = decoder.stdin_write(&annexb);
            }
        }
    }
    let feed = Feed::new(decoder.take_stdin().ok_or("decoder has no stdin")?);

    let _jpeg_reader = spawn_jpeg_reader(decoder.stdout_take(), hub.clone());
    hub.online.store(true, Ordering::SeqCst);
    eprintln!(
        "camweb: playing ({} track(s), session {})",
        session.tracks().len(),
        session.session_id()
    );

    let mut last_keepalive = Instant::now();
    let mut nal_count: u64 = 0;
    let mut last_frames = hub.frames.load(Ordering::Relaxed);
    let mut last_frame_at = Instant::now();
    loop {
        if hub.restart.swap(false, Ordering::SeqCst) {
            break;
        }
        // Watchdog: NALs arriving but nothing decoded means the decoder wedged (or
        // the bitstream is undecodable), which no pipe error would report.
        let frames_now = hub.frames.load(Ordering::Relaxed);
        if frames_now != last_frames {
            last_frames = frames_now;
            last_frame_at = Instant::now();
        } else if last_frame_at.elapsed() > Duration::from_secs(12) && nal_count > 0 {
            return Err(format!(
                "no frame decoded for 12 s after {} NALs",
                nal_count
            ));
        }
        if last_keepalive.elapsed() > Duration::from_secs(20) {
            if let Err(e) = session.keepalive() {
                return Err(format!("keepalive: {}", e));
            }
            last_keepalive = Instant::now();
        }
        match session.nal() {
            Ok(Some(nal)) => {
                nal_count += 1;
                hub.nals.fetch_add(1, Ordering::Relaxed);
                feed.feed(&nal);
            }
            Ok(None) => {}
            Err(e) => return Err(format!("rtsp read after {} NALs: {}", nal_count, e)),
        }
    }
    let dropped = feed.dropped();
    if dropped > 0 {
        eprintln!("camweb: pipeline stopped, {} NALs dropped to backpressure", dropped);
    }
    Ok(())
}

/// Fallback: let ffmpeg fetch the RTSP stream itself and hand us JPEGs. Used
/// only with --source ffmpeg; the default path is the Rust client above.
fn run_ffmpeg_source(hub: &Arc<Hub>, url: &str) -> Result<(), String> {
    let vf = if hub.cfg.width > 0 {
        format!("scale={}:-2", hub.cfg.width)
    } else {
        "null".to_string()
    };
    let q = hub.cfg.quality.to_string();
    let mut child = Command::new("ffmpeg")
        .args([
            "-v", "error", "-nostats", "-rtsp_transport", "tcp", "-i", url, "-an",
            "-fps_mode", "passthrough", "-vf", &vf, "-q:v", &q,
            "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawning ffmpeg: {}", e))?;
    let mut out = child.stdout.take().ok_or("ffmpeg produced no stdout")?;
    hub.online.store(true, Ordering::SeqCst);
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut chunk = [0u8; 65536];
    loop {
        if hub.restart.swap(false, Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        match out.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("ffmpeg stdout: {}", e)),
        }
        loop {
            let start = match find_soi(&buf) {
                Some(p) => p,
                None => {
                    if buf.len() > 1 << 20 {
                        buf.clear();
                    }
                    break;
                }
            };
            if start > 0 {
                buf.drain(..start);
            }
            match find_eoi(&buf) {
                Some(end) => {
                    let frame: Vec<u8> = buf.drain(..end).collect();
                    hub.publish(frame);
                }
                None => {
                    if buf.len() > 8 << 20 {
                        buf.clear();
                    }
                    break;
                }
            }
        }
    }
}

/// Decoupled NAL feed.
///
/// Writing into ffmpeg's stdin on the RTSP thread is a trap: any time the decoder
/// is momentarily slower than the stream (start-up probing, a keyframe burst) the
/// 64 KB pipe fills, `write` blocks, and the RTSP reader stops draining the socket
/// — which then looks like a stalled camera. So a writer thread owns stdin and the
/// reader only hands over whole NALs; if the queue is full a NAL is dropped (a
/// dropped NAL costs one frame's remainder, and the next IDR repairs it).
struct Feed {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl Feed {
    fn new(mut stdin: std::process::ChildStdin) -> Feed {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
        let writer = std::thread::spawn(move || {
            while let Ok(nal) = rx.recv() {
                if stdin.write_all(&nal).is_err() {
                    break;
                }
                let _ = stdin.flush();
            }
        });
        Feed {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            writer: Some(writer),
        }
    }

    fn feed(&self, nal: &[u8]) {
        match self.tx.try_send(nal.to_vec()) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        // Closing the channel ends the writer thread, which closes ffmpeg's stdin
        // so the decoder flushes and exits.
        let replacement = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let (dead_tx, _dead_rx) = replacement;
        let _ = std::mem::replace(&mut self.tx, dead_tx);
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
    }
}

struct Decoder {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
}

impl Decoder {
    fn stdin_write(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match &mut self.stdin {
            Some(s) => {
                s.write_all(buf)?;
                s.flush()
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "decoder stdin closed",
            )),
        }
    }

    fn stdout_take(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.stdin.take()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_decoder(cfg: &Config) -> Result<Decoder, String> {
    let vf = if cfg.width > 0 {
        format!("scale={}:-2", cfg.width)
    } else {
        "null".to_string()
    };
    let q = cfg.quality.to_string();
    let child = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostats",
            "-probesize",
            "32768",
            "-analyzeduration",
            "0",
            // Stamp each NAL by arrival time: a raw H.264 pipe has no timestamps,
            // so without this the muxer assumes 25 fps and duplicates or drops
            // frames unpredictably (measured 8/s and 33/s from the same stream).
            "-use_wallclock_as_timestamps",
            "1",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-an",
            "-fps_mode",
            "passthrough",
            "-vf",
            &vf,
            "-q:v",
            &q,
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Silent on purpose: wallclock timestamps make frames that arrive in the
        // same millisecond collide, and ffmpeg then logs one "non monotonically
        // increasing dts" warning per frame, forever. A decoder that dies is
        // caught by the EOF on stdout and by the no-frames watchdog below.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawning ffmpeg: {}", e))?;
    let mut child = child;
    let stdin = child.stdin.take();
    Ok(Decoder { child, stdin })
}

/// Read the JPEG stream coming back from ffmpeg and publish whole frames.
fn spawn_jpeg_reader(
    stdout: Option<std::process::ChildStdout>,
    hub: Arc<Hub>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut out = match stdout {
            Some(o) => o,
            None => return,
        };
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut chunk = [0u8; 65536];
        loop {
            match out.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
            loop {
                let start = match find_soi(&buf) {
                    Some(p) => p,
                    None => {
                        if buf.len() > 1 << 20 {
                            buf.clear();
                        }
                        break;
                    }
                };
                if start > 0 {
                    buf.drain(..start);
                }
                match find_eoi(&buf) {
                    Some(end) => {
                        let frame: Vec<u8> = buf.drain(..end).collect();
                        hub.publish(frame);
                    }
                    None => {
                        if buf.len() > 8 << 20 {
                            buf.clear();
                        }
                        break;
                    }
                }
            }
        }
    })
}

fn find_soi(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w[0] == 0xFF && w[1] == 0xD8)
}

fn find_eoi(b: &[u8]) -> Option<usize> {
    if b.len() < 4 {
        return None;
    }
    b.windows(2).position(|w| w[0] == 0xFF && w[1] == 0xD9).map(|p| p + 2)
}

/// Frames per second over the last second, for the panel's status line.
fn fps_loop(hub: Arc<Hub>) {
    let mut last = hub.frames.load(Ordering::Relaxed);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = hub.frames.load(Ordering::Relaxed);
        hub.fps.store((now - last) as u32, Ordering::Relaxed);
        last = now;
    }
}

// ----------------------------------------------------------------------- http

fn http_loop(hub: Arc<Hub>, listener: TcpListener) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let hub = hub.clone();
                std::thread::spawn(move || handle_conn(&hub, s));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

struct Request {
    path: String,
    query: Vec<(String, String)>,
}

fn read_request(s: &mut TcpStream) -> Option<Request> {
    s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
        if raw.windows(4).any(|w| w == b"\r\n\r\n") || raw.len() > 16384 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    let (path, qs) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    let mut query = Vec::new();
    for pair in qs.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (pair.to_string(), String::new()),
        };
        query.push((pct(&k), pct(&v)));
    }
    Some(Request { path, query })
}

impl Request {
    fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    #[allow(dead_code)]
    fn u32(&self, key: &str, lo: u32, hi: u32, default: u32) -> u32 {
        self.number(key, lo, hi).unwrap_or(default)
    }

    fn number(&self, key: &str, lo: u32, hi: u32) -> Option<u32> {
        match self.get(key).and_then(|v| v.parse::<u32>().ok()) {
            Some(v) if v >= lo && v <= hi => Some(v),
            _ => None,
        }
    }
}

fn pct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Status + headers, *not* terminated: the caller appends Content-Length and the
/// blank line (single-shot responses) or a terminating blank line and then the
/// body stream (MJPEG).
fn send_headers(s: &mut TcpStream, code: &str, ctype: &str, extra: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nCache-Control: no-store\r\n{}Connection: close\r\n",
        code, ctype, extra
    );
    s.write_all(head.as_bytes())
}

/// Terminate a header block that has no Content-Length (streaming responses).
fn end_headers(s: &mut TcpStream) -> std::io::Result<()> {
    s.write_all(b"\r\n")?;
    s.flush()
}

fn send_text(s: &mut TcpStream, code: &str, body: &str) {
    let _ = send_headers(s, code, "text/plain; charset=utf-8", "");
    let _ = write!(s, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = s.flush();
}

fn send_json(s: &mut TcpStream, body: &str) {
    let _ = send_headers(s, "200 OK", "application/json", "");
    let _ = write!(s, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = s.flush();
}

fn handle_conn(hub: &Arc<Hub>, mut s: TcpStream) {
    let req = match read_request(&mut s) {
        Some(r) => r,
        None => return,
    };
    match req.path.as_str() {
        "/" | "/index.html" => {
            let _ = send_headers(&mut s, "200 OK", "text/html; charset=utf-8", "");
            let _ = write!(s, "Content-Length: {}\r\n\r\n{}", PANEL_HTML.len(), PANEL_HTML);
            let _ = s.flush();
        }
        "/stream.mjpg" => stream_mjpeg(hub, s),
        "/snap.jpg" => match hub.latest.lock().unwrap().clone() {
            Some(frame) => {
                let _ = send_headers(&mut s, "200 OK", "image/jpeg", "");
                let _ = write!(s, "Content-Length: {}\r\n\r\n", frame.len());
                let _ = s.write_all(&frame);
                let _ = s.flush();
            }
            None => send_text(&mut s, "503 Service Unavailable", "no frame yet"),
        },
        "/snap" => save_snapshot(hub, &mut s),
        "/ptz" => ptz(hub, &req, &mut s),
        "/led" => led(hub, &req, &mut s),
        "/daynight" => daynight(hub, &req, &mut s),
        "/channel" => channel(hub, &req, &mut s),
        "/pins" => pins(hub, &mut s),
        "/status" => status(hub, &mut s),
        _ => send_text(&mut s, "404 Not Found", "no such endpoint"),
    }
}

fn stream_mjpeg(hub: &Arc<Hub>, mut s: TcpStream) {
    if send_headers(
        &mut s,
        "200 OK",
        "multipart/x-mixed-replace; boundary=frame",
        "Pragma: no-cache\r\n",
    )
    .is_err()
        || end_headers(&mut s).is_err()
    {
        return;
    }
    let sub = hub.subscribe();
    loop {
        let frame = {
            let mut slot = sub.slot.lock().unwrap();
            let mut idle = 0;
            while slot.is_none() && idle < 30 {
                let (guard, _) = sub.cv.wait_timeout(slot, Duration::from_secs(1)).unwrap();
                slot = guard;
                idle += 1;
            }
            slot.take()
        };
        let frame = match frame {
            Some(f) => f,
            // Nothing for ~30 s: the camera (or the decoder) went away. Close the
            // connection so the <img> errors and reconnects instead of hanging.
            None => break,
        };
        let head = format!(
            "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            frame.len()
        );
        if s.write_all(head.as_bytes()).is_err() || s.write_all(&frame).is_err() {
            break;
        }
        if s.write_all(b"\r\n").is_err() {
            break;
        }
        if s.flush().is_err() {
            break;
        }
    }
    hub.unsubscribe(&sub);
}

fn save_snapshot(hub: &Arc<Hub>, s: &mut TcpStream) {
    let frame = match hub.latest.lock().unwrap().clone() {
        Some(f) => f,
        None => {
            send_text(s, "503 Service Unavailable", "no frame yet");
            return;
        }
    };
    let dir = std::path::Path::new("work/video/captures");
    let _ = std::fs::create_dir_all(dir);
    let stamp = fmt_stamp();
    let path = dir.join(format!("web-{}.jpg", stamp));
    match std::fs::write(&path, &frame[..]) {
        Ok(()) => send_text(
            s,
            "200 OK",
            &format!("{} {} bytes", path.display(), frame.len()),
        ),
        Err(e) => send_text(s, "500 Internal Server Error", &format!("write: {}", e)),
    }
}

fn fmt_stamp() -> String {
    // Busybox-free local time without a date crate: ask the OS.
    let out = Command::new("date").arg("+%H%M%S").output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "snap".to_string(),
    }
}

fn ptz(hub: &Arc<Hub>, req: &Request, s: &mut TcpStream) {
    // Strict: the motors have no end-stops and nothing knows where the head is
    // pointing, so a malformed request must not silently turn into a move.
    let (axis, dir, steps, speed) = match (
        req.number("axis", 0, 1),
        req.number("dir", 0, 1),
        req.number("steps", 1, 32),
        req.number("speed", 1, 10),
    ) {
        (Some(a), Some(d), Some(st), Some(sp)) => (a, d, st, sp),
        _ => {
            send_text(
                s,
                "400 Bad Request",
                "need axis=0|1 dir=0|1 steps=1..32 speed=1..10",
            );
            return;
        }
    };
    // One move at a time: motorctl's lock is advisory and only guards the device
    // opening, not two overlapping UI requests.
    let _guard = hub.ptz_lock.lock().unwrap();
    let cmd = format!("{} {} {} {} {}", MOTORCTL, axis, dir, steps, speed);
    let reply = match hub.board_run(&cmd) {
        Ok((rc, out)) => {
            let first = out
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .last()
                .unwrap_or("")
                .to_string();
            let line = format!(
                "{} rc={} {}",
                fmt_stamp(),
                rc,
                if first.is_empty() { "(no output)" } else { &first }
            );
            *hub.last_ptz.lock().unwrap() = line.clone();
            line
        }
        Err(e) => {
            let line = format!("{} failed: {}", fmt_stamp(), e);
            *hub.last_ptz.lock().unwrap() = line.clone();
            line
        }
    };
    send_text(s, "200 OK", &reply);
}

fn led(hub: &Arc<Hub>, req: &Request, s: &mut TcpStream) {
    let name = match req.get("name").unwrap_or("white") {
        "white" => "WHITE_LED",
        "ir" => "IR_LED",
        other => {
            send_text(s, "400 Bad Request", &format!("unknown LED '{}'", other));
            return;
        }
    };
    let pin = format!("/sys/user-gpio/{}", name);
    let action = req.get("action").unwrap_or("off");
    let cmd = match action {
        "on" => format!("echo 1 > {}", pin),
        "off" => format!("echo 0 > {}", pin),
        "blink" => format!(
            "for i in 1 2 3; do echo 1 > {p}; sleep 0.3; echo 0 > {p}; sleep 0.3; done",
            p = pin
        ),
        other => {
            send_text(s, "400 Bad Request", &format!("unknown action '{}'", other));
            return;
        }
    };
    let reply = match hub.board_run(&cmd) {
        Ok((rc, out)) => format!(
            "{} {} {} rc={} {}",
            fmt_stamp(),
            name,
            action,
            rc,
            out.trim()
        ),
        Err(e) => format!("{} {} {} failed: {}", fmt_stamp(), name, action, e),
    };
    send_text(s, "200 OK", &reply);
}

fn daynight(hub: &Arc<Hub>, req: &Request, s: &mut TcpStream) {
    let mode = req.get("mode").unwrap_or("day");
    let value = match mode {
        "night" => "1",
        "day" => "0",
        other => {
            send_text(s, "400 Bad Request", &format!("unknown mode '{}'", other));
            return;
        }
    };
    // The rtsp day/night thread polls this file every 5 s and only acts on a
    // change, so writing it is the whole interface. It is volatile: no /tmp/photo
    // at boot means the guard in config.sh decides (day).
    let cmd = format!("printf '{}\\n' > /tmp/photo", value);
    let reply = match hub.board_run(&cmd) {
        Ok((0, _)) => format!(
            "{} {}: rtsp applies ircut/IR/ISP within 5 s",
            fmt_stamp(),
            mode
        ),
        Ok((rc, out)) => format!("{} {} rc={} {}", fmt_stamp(), mode, rc, out.trim()),
        Err(e) => format!("{} {} failed: {}", fmt_stamp(), mode, e),
    };
    send_text(s, "200 OK", &reply);
}

fn channel(hub: &Arc<Hub>, req: &Request, s: &mut TcpStream) {
    let ch = match req.get("ch").unwrap_or("vs0") {
        c @ ("vs0" | "vs1") => c.to_string(),
        other => {
            send_text(s, "400 Bad Request", &format!("unknown channel '{}'", other));
            return;
        }
    };
    *hub.channel.lock().unwrap() = ch.clone();
    hub.restart.store(true, Ordering::SeqCst);
    send_text(s, "200 OK", &format!("{} switched to {}", fmt_stamp(), ch));
}

fn pins(hub: &Arc<Hub>, s: &mut TcpStream) {
    let cmd = "for p in IR_LED WHITE_LED ircut_a ircut_b SPK_PA; do \
               echo \"$p=$(cat /sys/user-gpio/$p 2>/dev/null)\"; done; \
               echo \"photo=$(cat /tmp/photo 2>/dev/null)\"; \
               echo \"rtsp=$(ps | grep -c \"[r]tsp\")\"; \
               echo \"up=$(cut -d. -f1 /proc/uptime)\"";
    let mut fields = Vec::new();
    match hub.board_run(cmd) {
        Ok((_, out)) => {
            for line in out.lines() {
                let line = line.trim().trim_start_matches('$');
                if let Some((k, v)) = line.split_once('=') {
                    if !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        fields.push(format!(
                            "\"{}\":\"{}\"",
                            k,
                            v.trim().replace('"', "")
                        ));
                    }
                }
            }
        }
        Err(e) => fields.push(format!("\"error\":\"{}\"", json_esc(&e))),
    }
    send_json(s, &format!("{{{}}}", fields.join(",")));
}

fn status(hub: &Arc<Hub>, s: &mut TcpStream) {
    let last_err = hub.last_err.lock().unwrap().clone();
    let last_ptz = hub.last_ptz.lock().unwrap().clone();
    let channel = hub.channel.lock().unwrap().clone();
    let body = format!(
        "{{\"online\":{},\"fps\":{},\"frames\":{},\"nals\":{},\"dropped\":{},\
         \"width\":{},\"height\":{},\"clients\":{},\"channel\":\"{}\",\
         \"uptime_s\":{},\"last_ptz\":\"{}\",\"last_err\":\"{}\"}}",
        hub.online.load(Ordering::Relaxed),
        hub.fps.load(Ordering::Relaxed),
        hub.frames.load(Ordering::Relaxed),
        hub.nals.load(Ordering::Relaxed),
        hub.dropped_nals.load(Ordering::Relaxed),
        hub.width.load(Ordering::Relaxed),
        hub.height.load(Ordering::Relaxed),
        hub.subs.lock().unwrap().len(),
        channel,
        hub.started.elapsed().as_secs(),
        json_esc(&last_ptz),
        json_esc(&last_err)
    );
    send_json(s, &body);
}

fn json_esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push(' '),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

// ------------------------------------------------------------------------ cli

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "serve".to_string());
    match cmd.as_str() {
        "serve" => serve(&args[1..]),
        "snap" => snapshot_cmd(&args[1..]),
        "rec" => record_cmd(&args[1..]),
        "probe" => probe_cmd(&args[1..]),
        "sh" => {
            let board = Board::from_env();
            let line = args[1..].join(" ");
            match board.run(&line, Duration::from_secs(30)) {
                Ok((rc, out)) => {
                    println!("{}", out);
                    eprintln!("rc={}", rc);
                }
                Err(e) => {
                    eprintln!("camweb sh: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "-h" | "--help" | "help" => eprintln!(
            "usage: camweb serve [--listen ADDR] [--rtsp URL] [--fps N] [--width N]\n\
             \x20      camweb snap [OUT.jpg] [--ch vs0|vs1]\n\
             \x20      camweb rec SECONDS [OUT.mp4] [--ch vs0|vs1]\n\
             \x20      camweb probe [--url URL]\n\
             \x20      camweb sh 'board command'"
        ),
        other => {
            eprintln!("camweb: unknown command '{}'", other);
            std::process::exit(2);
        }
    }
}

fn serve(args: &[String]) {
    let cfg = Config::from_env_and_args(args);
    let listener = match TcpListener::bind(&cfg.listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("camweb: cannot bind {}: {}", cfg.listen, e);
            std::process::exit(1);
        }
    };
    let hub = Hub::new(cfg.clone());

    {
        let hub = hub.clone();
        std::thread::spawn(move || pipeline_loop(hub));
    }
    {
        let hub = hub.clone();
        std::thread::spawn(move || fps_loop(hub));
    }
    println!(
        "camweb: panel     http://{}/",
        cfg.listen.replace("0.0.0.0", "localhost")
    );
    println!(
        "camweb: video     {} -> {} fps, {} px wide, jpeg q{}",
        cfg.rtsp,
        if cfg.fps == 0 {
            "camera's own".to_string()
        } else {
            cfg.fps.to_string()
        },
        if cfg.width == 0 {
            "native".to_string()
        } else {
            cfg.width.to_string()
        },
        cfg.quality
    );
    println!("camweb: board     {}:{}", cfg.board, telnet::DEFAULT_PORT);
    http_loop(hub, listener);
}

fn snapshot_cmd(args: &[String]) {
    let cfg = Config::from_env_and_args(args);
    let mut channel = "vs0";
    let mut out = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ch" => {
                channel = args.get(i + 1).map(|s| s.as_str()).unwrap_or("vs0");
                i += 2;
            }
            other => {
                if !other.starts_with("--") {
                    out = other.to_string();
                }
                i += 1;
            }
        }
    }
    if out.is_empty() {
        out = format!(
            "work/video/captures/rust-snap-{}.jpg",
            fmt_stamp()
        );
    }
    let url = channel_url(&cfg.rtsp, channel);
    match grab_jpeg(&url, &cfg) {
        Ok(jpeg) => {
            if let Some(dir) = std::path::Path::new(&out).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(&out, &jpeg) {
                Ok(()) => println!("{} {} bytes", out, jpeg.len()),
                Err(e) => {
                    eprintln!("camweb snap: {}: {}", out, e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("camweb snap: {}", e);
            std::process::exit(1);
        }
    }
}

/// Pull one decoded frame using our own RTSP client.
fn grab_jpeg(url: &str, cfg: &Config) -> Result<Vec<u8>, String> {
    let mut session = rtsp::Session::connect(url, &["video"], Duration::from_millis(300))
        .map_err(|e| format!("rtsp connect: {}", e))?;
    let mut decoder = spawn_decoder(cfg)?;
    if let Some((sps, pps)) = session.sprop_ps() {
        for ps in [&sps, &pps] {
            if !ps.is_empty() {
                let mut annexb = vec![0u8, 0, 0, 1];
                annexb.extend_from_slice(ps);
                let _ = decoder.stdin_write(&annexb);
            }
        }
    }
    let mut stdout = decoder
        .stdout_take()
        .ok_or_else(|| "decoder produced no stdout".to_string())?;
    let feed = Feed::new(decoder.take_stdin().ok_or("decoder has no stdin")?);

    // ffmpeg's stdout can only be read blocking, so the first whole JPEG is
    // collected on its own thread while this one keeps feeding NALs: reading
    // inline would stall the feed exactly when ffmpeg wants more input.
    let first: Arc<(Mutex<Option<Vec<u8>>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
    {
        let first = first.clone();
        std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
            let mut chunk = [0u8; 65536];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => return,
                }
                if let Some(start) = find_soi(&buf) {
                    if start > 0 {
                        buf.drain(..start);
                    }
                    if let Some(end) = find_eoi(&buf) {
                        let jpeg: Vec<u8> = buf.drain(..end).collect();
                        let (lock, cv) = &*first;
                        *lock.lock().unwrap() = Some(jpeg);
                        cv.notify_all();
                        return;
                    }
                }
            }
        });
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut nals = 0u64;
    loop {
        {
            let (lock, _) = &*first;
            if let Some(jpeg) = lock.lock().unwrap().take() {
                return Ok(jpeg);
            }
        }
        if Instant::now() > deadline {
            return Err(format!("no frame within 20 s ({} NALs read)", nals));
        }
        match session.nal() {
            Ok(Some(nal)) => {
                nals += 1;
                feed.feed(&nal);
            }
            Ok(None) => {}
            Err(e) => return Err(format!("rtsp read: {}", e)),
        }
    }
}

/// Record to MP4 without re-encoding: our RTSP client produces the Annex-B
/// bitstream, ffmpeg only muxes it. Audio is deliberately not muxed — the
/// streams carry 8 kHz a-law, which mp4 cannot hold (that is what produced the
/// 0-byte recordings early on).
fn record_cmd(args: &[String]) {
    let secs: u64 = args
        .iter()
        .find_map(|a| a.parse::<u64>().ok())
        .unwrap_or(10);
    let mut channel = "vs0";
    let mut out = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ch" => {
                channel = args.get(i + 1).map(|s| s.as_str()).unwrap_or("vs0");
                i += 2;
            }
            other => {
                if !other.starts_with("--") && other.parse::<u64>().is_err() && out.is_empty() {
                    out = other.to_string();
                }
                i += 1;
            }
        }
    }
    if out.is_empty() {
        out = format!("work/video/captures/rust-rec-{}.mp4", fmt_stamp());
    }
    let cfg = Config::from_env_and_args(args);
    let url = channel_url(&cfg.rtsp, channel);
    let session = match rtsp::Session::connect(&url, &["video"], Duration::from_millis(500)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("camweb rec: {}: {}", url, e);
            std::process::exit(1);
        }
    };
    if let Some(dir) = std::path::Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut child = match Command::new("ffmpeg")
        .args([
            // Wallclock timestamps again: without them the muxer times a raw
            // H.264 pipe at an assumed 25 fps, so a 6 s recording came out 2.2 s
            // long and played back fast.
            "-v", "error", "-use_wallclock_as_timestamps", "1", "-f", "h264",
            "-i", "pipe:0", "-an", "-c", "copy", "-movflags", "+faststart",
            "-y", &out,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("camweb rec: ffmpeg: {}", e);
            std::process::exit(1);
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut session = session;
    if let Some((sps, pps)) = session.sprop_ps() {
        for ps in [&sps, &pps] {
            if !ps.is_empty() {
                let mut annexb = vec![0u8, 0, 0, 1];
                annexb.extend_from_slice(ps);
                let _ = stdin.write_all(&annexb);
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut nals = 0u64;
    let mut bytes = 0u64;
    while Instant::now() < deadline {
        match session.nal() {
            Ok(Some(nal)) => {
                nals += 1;
                bytes += nal.len() as u64;
                if stdin.write_all(&nal).is_err() {
                    eprintln!("camweb rec: ffmpeg stopped taking input");
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("camweb rec: rtsp read: {}", e);
                break;
            }
        }
    }
    drop(stdin);
    let _ = child.wait();
    println!(
        "{} {} NALs, {} KB, {} s requested",
        out,
        nals,
        bytes / 1024,
        secs
    );
}

fn probe_cmd(args: &[String]) {
    let cfg = Config::from_env_and_args(args);
    let mut url = cfg.rtsp.clone();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--url" || args[i] == "--rtsp" {
            if let Some(v) = args.get(i + 1) {
                url = v.clone();
            }
        }
        i += 1;
    }
    let mut session = match rtsp::Session::connect(&url, &["video"], Duration::from_millis(500)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("camweb probe: connect {}: {}", url, e);
            std::process::exit(1);
        }
    };
    println!("url      {}", url);
    println!("session  {}", session.session_id());
    for t in session.tracks() {
        println!(
            "track    {} {} {} Hz interleaved={:?}",
            t.media, t.encoding, t.clock, t.interleaved
        );
    }
    match session.sprop_ps() {
        Some((sps, pps)) => println!("sprop    SPS {} B, PPS {} B", sps.len(), pps.len()),
        None => println!("sprop    (none advertised)"),
    }
    let mut nals = 0u64;
    let mut bytes = 0u64;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match session.nal() {
            Ok(Some(nal)) => {
                nals += 1;
                bytes += nal.len() as u64;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("camweb probe: read: {}", e);
                break;
            }
        }
    }
    println!(
        "read     {} NALs, {} bytes in 10 s ({:.0} B/s)",
        nals,
        bytes,
        bytes as f64 / 10.0
    );
}
