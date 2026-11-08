//! camrtsp — clean-room Rust RTSP (+ MJPEG) server for the YI CB211.
//!
//! Everything protocol-shaped is ours; the only non-Rust piece is the vendor
//! encoder behind `--source pump:`, because the AK3918 MPI libraries are
//! uClibc shared objects.

mod b64;
mod client;
mod httpd;
mod media;
mod motion;
mod rec;
mod rtsp;
mod source;
mod venc;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "\
camrtsp — Rust RTSP server for the AK3918 camera

USAGE:
  camrtsp serve  [OPTIONS]
  camrtsp info   [--source SPEC]
  camrtsp version

  --listen ADDR      RTSP listen address        (default 0.0.0.0:8554)
  --http ADDR        MJPEG HTTP listen address  (default off)
  --source SPEC      where frames come from:
                       client:HOST[:PORT][/PATH]
                                             a live RTSP server upstream, e.g.
                                             client:127.0.0.1:554/vs0 — we pull
                                             H.264 + a-law and re-serve it
                       replay:PATH[:FPS[:loop]]  raw Annex-B H.264 file
                       pump:PATH|HOST:PORT       encoder socket (see venc.rs)
                       synth[:FPS]               fake frames, for tests
                       none                      no frames (RTSP still answers)
                     (default none)

  --rec DIR          record motion-triggered segments into DIR/rec
  --rec-quota BYTES  byte cap for DIR/rec (accepts 8G, 800M, 12MiB)
                                       (default 8589934592, 8 GiB)
  --rec-always       write every segment, not only while motion is detected
                     (for tuning the detector from real footage)
  --motion-log       print the detector state once a second
";


fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        Some("serve") => cmd_serve(&args[1..]),
        Some("info") => cmd_info(&args[1..]),
        Some("version") | Some("-V") | Some("--version") => {
            println!("camrtsp {}", media::VERSION);
            0
        }
        Some("-h") | Some("--help") | Some("help") | None => {
            print!("{}", USAGE);
            0
        }
        Some(other) => {
            eprintln!("camrtsp: unknown command {:?}\n", other);
            print!("{}", USAGE);
            2
        }
    };
    std::process::exit(code);
}

fn parse_addr(s: &str) -> Result<SocketAddr, String> {
    s.to_socket_addrs()
        .map_err(|e| format!("bad address {:?}: {}", s, e))?
        .next()
        .ok_or_else(|| format!("address {:?} resolved to nothing", s))
}

fn build_source(spec: &str) -> Result<Arc<dyn source::VideoSource>, String> {
    let spec = spec.trim();
    if spec.is_empty() || spec == "none" || spec == "null" {
        let hub = source::empty("vs0");
        return Ok(hub as Arc<dyn source::VideoSource>);
    }
    // Pull from an upstream RTSP server and re-serve it. This is the live
    // path while the stock `rtsp` binary still owns the hardware encoder: it
    // makes our server the only thing clients talk to, and it is also how a
    // second camera or a site relay feeds us.
    if let Some(rest) = spec
        .strip_prefix("client:")
        .or_else(|| spec.strip_prefix("rtsp:"))
    {
        let (addr, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/vs0"),
        };
        if addr.is_empty() {
            return Err("client: needs host[:port]/path".into());
        }
        let hub = client::pull(addr, path).map_err(|e| format!("client {}: {}", addr, e))?;
        return Ok(hub as Arc<dyn source::VideoSource>);
    }
    if let Some(rest) = spec.strip_prefix("replay:") {
        let mut parts = rest.splitn(3, ':');
        let path = parts.next().unwrap_or("").to_string();
        let fps = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(15);
        let loopy = matches!(parts.next(), Some("loop"));
        if path.is_empty() {
            return Err("replay: needs a file path".into());
        }
        let hub = source::replay(&path, fps, loopy).map_err(|e| format!("replay {}: {}", path, e))?;
        return Ok(hub as Arc<dyn source::VideoSource>);
    }
    if let Some(rest) = spec.strip_prefix("pump:") {
        let hub = venc::connect(rest).map_err(|e| format!("pump {}: {}", rest, e))?;
        return Ok(hub as Arc<dyn source::VideoSource>);
    }
    if let Some(rest) = spec.strip_prefix("synth") {
        let fps = rest
            .strip_prefix(':')
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(15);
        let hub = source::synthetic(fps, false);
        return Ok(hub as Arc<dyn source::VideoSource>);
    }
    Err(format!("unknown source spec {:?}", spec))
}

fn cmd_serve(args: &[String]) -> i32 {
    let mut listen = "0.0.0.0:8554".to_string();
    let mut http = String::new();
    let mut spec = "none".to_string();
    let mut rec_dir: Option<PathBuf> = None;
    let mut rec_quota: u64 = 8 << 30;
    let mut rec_always = false;
    let mut motion_log = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                match args.get(i) {
                    Some(v) => listen = v.clone(),
                    None => return usage_err("--listen needs an address"),
                }
            }
            "--http" => {
                i += 1;
                match args.get(i) {
                    Some(v) => http = v.clone(),
                    None => return usage_err("--http needs an address"),
                }
            }
            "--source" => {
                i += 1;
                match args.get(i) {
                    Some(v) => spec = v.clone(),
                    None => return usage_err("--source needs a spec"),
                }
            }
            "--rec" => {
                i += 1;
                match args.get(i) {
                    Some(v) => rec_dir = Some(PathBuf::from(v)),
                    None => return usage_err("--rec needs a directory"),
                }
            }
            "--rec-quota" => {
                i += 1;
                match args.get(i).and_then(|v| parse_bytes(v)) {
                    Some(v) => rec_quota = v,
                    None => return usage_err("--rec-quota needs a byte count (8G, 500M, 12345)"),
                }
            }
            "--rec-always" => rec_always = true,
            "--motion-log" => motion_log = true,
            other => return usage_err(&format!("unknown flag {:?}", other)),
        }
        i += 1;
    }

    let addr = match parse_addr(&listen) {
        Ok(a) => a,
        Err(e) => return usage_err(&e),
    };
    let src = match build_source(&spec) {
        Ok(s) => s,
        Err(e) => return usage_err(&e),
    };
    let info = src.info();
    eprintln!(
        "camrtsp: source {} {}x{} {}fps{} (sps {} B, pps {} B)",
        if info.name.is_empty() { "?" } else { &info.name },
        info.width,
        info.height,
        info.fps,
        if info.audio.is_some() { ", alaw audio" } else { "" },
        info.sps.len(),
        info.pps.len()
    );

    if !http.is_empty() {
        let haddr = match parse_addr(&http) {
            Ok(a) => a,
            Err(e) => return usage_err(&e),
        };
        let s2 = src.clone();
        std::thread::Builder::new()
            .name("httpd".into())
            .spawn(move || {
                let cfg = httpd::Cfg {
                    listen: haddr,
                    stream_path: "/stream.mjpg".to_string(),
                };
                if let Err(e) = httpd::serve(cfg, s2) {
                    eprintln!("camrtsp: http server: {}", e);
                }
            })
            .ok();
    }

    // Motion detection and recording watch the same source the sessions do,
    // through their own cursor: the ring in `source::Hub` hands every reader
    // the same frames in order, so tapping cannot steal frames from a session.
    if rec_dir.is_some() || motion_log {
        let tap_src = src.clone();
        let tap_dir = rec_dir.clone();
        std::thread::Builder::new()
            .name("tap".into())
            .spawn(move || tap(tap_src, tap_dir.as_deref(), rec_quota, rec_always, motion_log))
            .ok();
    }

    let cfg = rtsp::Cfg::new(addr);
    eprintln!("camrtsp: rtsp listening on {}", addr);
    match rtsp::serve(cfg, src) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("camrtsp: rtsp server: {}", e);
            1
        }
    }
}

fn cmd_info(args: &[String]) -> i32 {
    let mut spec = "none".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--source" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    spec = v.clone();
                }
            }
            other => return usage_err(&format!("unknown flag {:?}", other)),
        }
        i += 1;
    }
    let src = match build_source(&spec) {
        Ok(s) => s,
        Err(e) => return usage_err(&e),
    };
    let info = src.info();
    println!("name     {}", info.name);
    println!("size     {}x{}", info.width, info.height);
    println!("fps      {}", info.fps);
    println!("sps      {:02x?}", info.sps);
    println!("pps      {:02x?}", info.pps);
    if let Some(a) = &info.audio {
        println!("audio    {} Hz {} ch{}", a.rate, a.channels, if a.alaw { " a-law" } else { "" });
    } else {
        println!("audio    none");
    }
    // Show the first frame we can get, so a dead source is obvious.
    let mut got = None;
    for _ in 0..15 {
        if let Some(f) = src.latest() {
            got = Some(f);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match got {
        Some(f) => println!(
            "frame    seq {} {} nals {} B keyframe {}",
            f.seq,
            f.nals.len(),
            f.len(),
            f.keyframe
        ),
        None => println!("frame    (none within 1.5 s)"),
    }
    0
}

/// Byte count with an optional suffix: `8G`, `500M`, `12MiB`, `12345`.
fn parse_bytes(s: &str) -> Option<u64> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Everything that is not the protocol: watch frames, decide motion, record.
///
/// One thread, one cursor, one clock. It is deliberately dumb — if it falls
/// behind, `Hub` drops the oldest history and this resyncs on a keyframe rather
/// than trying to catch up, because a recording is worth more than a complete
/// one.
fn tap(src: Arc<dyn source::VideoSource>, rec_dir: Option<&std::path::Path>,
       quota: u64, rec_always: bool, motion_log: bool) {
    let mut det = motion::Detector::new(src.info());
    let mut recorder = match rec_dir.map(|d| {
        rec::Recorder::new(d, rec::RecConfig { quota_bytes: quota, ..Default::default() })
    }) {
        Some(Ok(r)) => Some(r),
        Some(Err(e)) => {
            eprintln!("camrtsp: recorder disabled: {}", e);
            None
        }
        None => None,
    };
    if let Some(r) = &recorder {
        eprintln!("camrtsp: recording into {}", r.rec_dir().display());
    }

    let t0 = Instant::now();
    let mut cursor = 0u64;
    let mut last_ts = 0u64;
    let mut last_tick = Instant::now();
    let mut last_log = Instant::now();
    let mut moving = false;

    loop {
        match src.next(cursor, Duration::from_millis(500)) {
            Some(f) => {
                cursor = f.seq;
                last_ts = last_ts.max(f.ts_ms);
                let m = det.observe_frame(&f);
                if m.just_started && motion_log {
                    eprintln!(
                        "camrtsp: motion at {} ms (au {} B, baseline {:?} B, score {:.2})",
                        f.ts_ms, f.len(), det.baseline_bytes(), m.score
                    );
                }
                moving = m.active;
                if let Some(r) = recorder.as_mut() {
                    let write = rec_always || moving;
                    match r.on_frame(&f, write) {
                        Ok(a) => {
                            if let Some(p) = &a.opened {
                                eprintln!("camrtsp: rec open {}", p.display());
                            }
                            for _ in 0..a.evicted {
                                eprintln!("camrtsp: rec quota evicted the oldest segment");
                            }
                            if let Some(e) = &a.error {
                                eprintln!("camrtsp: rec halted: {}", e);
                            }
                        }
                        Err(e) => eprintln!("camrtsp: rec: {}", e),
                    }
                }
            }
            None => {
                if src.ended() {
                    eprintln!("camrtsp: source ended");
                    break;
                }
            }
        }

        // Housekeeping: close a segment that has had its post-roll, and keep the
        // quota honest even while nothing is arriving. The recorder clock is the
        // frames' own clock, extended by wall time so an idle stream still ticks.
        if last_tick.elapsed() >= Duration::from_millis(500) {
            last_tick = Instant::now();
            let now_ms = last_ts.max(t0.elapsed().as_millis() as u64);
            if let Some(r) = recorder.as_mut() {
                match r.tick(now_ms) {
                    Ok(Some(seg)) => eprintln!(
                        "camrtsp: rec closed {} ({} B, {} AUs, {} ms)",
                        seg.path.display(), seg.bytes, seg.au_count, seg.duration_ms
                    ),
                    Ok(None) => {}
                    Err(e) => eprintln!("camrtsp: rec tick: {}", e),
                }
            }
        }

        if motion_log && last_log.elapsed() >= Duration::from_secs(1) {
            last_log = Instant::now();
            eprintln!(
                "camrtsp: motion {} (baseline {:?} B, {} samples, {} AUs seen, {} recorded)",
                if moving { "ACTIVE" } else { "quiet" },
                det.baseline_bytes(),
                det.baseline_samples(),
                det.frames_seen(),
                recorder.as_ref().map(|r| r.finished_count()).unwrap_or(0)
            );
        }
    }
}

fn usage_err(msg: &str) -> i32 {
    eprintln!("camrtsp: {}\n", msg);
    print!("{}", USAGE);
    2
}
