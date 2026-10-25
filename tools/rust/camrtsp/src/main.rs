//! camrtsp — clean-room Rust RTSP (+ MJPEG) server for the YI CB211.
//!
//! Everything protocol-shaped is ours; the only non-Rust piece is the vendor
//! encoder behind `--source pump:`, because the AK3918 MPI libraries are
//! uClibc shared objects.

mod b64;
mod httpd;
mod media;
mod rtsp;
mod source;
mod venc;

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "\
camrtsp — Rust RTSP server for the AK3918 camera

USAGE:
  camrtsp serve  [--listen ADDR] [--http ADDR] [--source SPEC]
  camrtsp info   [--source SPEC]
  camrtsp version

  --listen ADDR   RTSP listen address        (default 0.0.0.0:8554)
  --http ADDR     MJPEG HTTP listen address  (default off)
  --source SPEC   where frames come from:
                    replay:PATH[:FPS[:loop]]  raw Annex-B H.264 file
                    pump:PATH|HOST:PORT       encoder socket (see venc.rs)
                    synth[:FPS]               fake frames, for tests
                    none                      no frames (RTSP still answers)
                  (default none)
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

fn usage_err(msg: &str) -> i32 {
    eprintln!("camrtsp: {}\n", msg);
    print!("{}", USAGE);
    2
}
