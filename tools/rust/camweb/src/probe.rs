//! Standalone test harness for `rtsp.rs` — pure std, no crates.
//!
//! Connects to the camera's stock RTSP server, prints the SDP it offered, the
//! tracks it set up, the session id and advertised timeout, then reads NALs for
//! a while and reports rates, NAL-type histogram and its own CPU cost. Optionally
//! writes the Annex-B elementary stream so `ffprobe` can prove the
//! depacketisation is correct.
//!
//! Build:
//!     RUSTC=$HOME/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin/rustc
//!     $RUSTC -O --edition 2021 src/probe.rs -o target/probe
//!
//! Usage:
//!     probe [URL] [options]
//!       --secs N            how long to read (default 15)
//!       --out FILE          write Annex-B H.264 to FILE
//!       --keepalive N       send OPTIONS every N seconds (default 20; 0 = never)
//!       --no-keepalive      same as --keepalive 0
//!       --timeout-ms N      socket read timeout (default 1500)
//!       --media v[,a]       media to set up (default "video"; use "video,audio")
//!       --verbose           print each NAL type as it arrives
//!
//! Exit codes: 0 ok, 1 connect/setup failure, 2 usage, 3 mid-stream error.

#[path = "rtsp.rs"]
mod rtsp;

use std::fs::File;
use std::io::{self, Write};
use std::time::{Duration, Instant};

fn cpu_ms() -> f64 {
    // /proc/self/stat: fields after the "(comm)" are state, ppid, ...;
    // utime and stime are the 14th and 15th fields overall.
    let stat = match std::fs::read_to_string("/proc/self/stat") {
        Ok(s) => s,
        Err(_) => return 0.0,
    };
    let rest = match stat.rfind(')') {
        Some(i) => &stat[i + 1..],
        None => return 0.0,
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0] = state (field 3), so utime (field 14) = f[11], stime (15) = f[12]
    let u: f64 = f.get(11).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let s: f64 = f.get(12).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    // USER_HZ is 100 on Linux; close enough for a cost estimate.
    (u + s) * 10.0
}

fn nal_type_name(t: u8) -> &'static str {
    match t {
        1 => "non-IDR slice",
        5 => "IDR slice",
        6 => "SEI",
        7 => "SPS",
        8 => "PPS",
        9 => "AUD",
        12 => "filler",
        _ => "other",
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut url = String::from("rtsp://192.168.1.103:554/vs0");
    let mut secs: f64 = 15.0;
    let mut out: Option<String> = None;
    let mut keepalive_s: f64 = 20.0;
    let mut timeout_ms: u64 = 1500;
    let mut media: Vec<String> = vec!["video".to_string()];
    let mut verbose = false;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let val = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_default()
        };
        match a {
            "--secs" => secs = val(&mut i).parse().unwrap_or(15.0),
            "--out" => out = Some(val(&mut i)),
            "--keepalive" => keepalive_s = val(&mut i).parse().unwrap_or(20.0),
            "--no-keepalive" => keepalive_s = 0.0,
            "--timeout-ms" => timeout_ms = val(&mut i).parse().unwrap_or(1500),
            "--media" => {
                media = val(&mut i)
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            "--verbose" => verbose = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: probe [URL] [--secs N] [--out FILE] [--keepalive N|--no-keepalive] \
                     [--timeout-ms N] [--media video[,audio]] [--verbose]"
                );
                std::process::exit(2);
            }
            other if other.starts_with("--") => {
                eprintln!("probe: unknown option {other}");
                std::process::exit(2);
            }
            other => url = other.to_string(),
        }
        i += 1;
    }

    let media_refs: Vec<&str> = media.iter().map(|s| s.as_str()).collect();
    let io_timeout = Duration::from_millis(timeout_ms);

    println!("== rtsp probe ==");
    println!("url          {url}");
    println!("set_up       {media_refs:?}");
    println!("duration     {secs}s");
    println!("io_timeout   {timeout_ms} ms");
    println!("keepalive    {}", if keepalive_s > 0.0 { format!("{keepalive_s}s") } else { "off".to_string() });

    let t_connect = Instant::now();
    let mut s = match rtsp::Session::connect(&url, &media_refs, io_timeout) {
        Ok(s) => s,
        Err(e) => {
            println!("CONNECT_FAILED after {:.0} ms: {e}", t_connect.elapsed().as_secs_f64() * 1000.0);
            std::process::exit(1);
        }
    };
    println!(
        "CONNECTED in {:.0} ms",
        t_connect.elapsed().as_secs_f64() * 1000.0
    );
    println!("session_id   {}", s.session_id());
    println!(
        "srv_timeout  {}",
        s.server_timeout()
            .map(|t| format!("{t}s"))
            .unwrap_or_else(|| "not advertised".into())
    );

    println!("\n-- SDP --");
    print!("{}", s.sdp());
    println!("-- end SDP --\n");

    println!("-- tracks --");
    for (k, t) in s.tracks().iter().enumerate() {
        println!(
            "[{k}] {} enc={} clock={} control={:?} interleaved={:?}",
            t.media, t.encoding, t.clock, t.control, t.interleaved
        );
        if !t.fmtp.is_empty() {
            println!("    fmtp {}", t.fmtp);
        }
    }
    match s.sprop_ps() {
        Some((sps, pps)) => {
            println!(
                "SPROP present sps={} B (nal type {}) pps={} B (nal type {})",
                sps.len(),
                sps.first().map(|b| b & 0x1f).unwrap_or(0),
                pps.len(),
                pps.first().map(|b| b & 0x1f).unwrap_or(0)
            );
            println!(
                "SPROP sps={} pps={}",
                hex(&sps),
                hex(&pps)
            );
        }
        None => println!("SPROP absent"),
    }
    println!();

    let mut fh: Option<File> = match &out {
        Some(path) => match File::create(path) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("probe: cannot create {path}: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };

    // ---- read loop -------------------------------------------------------
    let cpu0 = cpu_ms();
    let start = Instant::now();
    let deadline = start + Duration::from_secs_f64(secs);
    let mut next_keepalive = start + Duration::from_secs_f64(keepalive_s.max(0.001));
    let keepalive_on = keepalive_s > 0.0;

    let mut nals: u64 = 0;
    let mut bytes: u64 = 0;
    let mut hist = [0u64; 32];
    let mut aus: u64 = 0; // NALs carrying the RTP marker bit = access units
    let mut empties: u64 = 0;
    let mut idr: u64 = 0;
    let mut sps_inband: u64 = 0;
    let mut pps_inband: u64 = 0;
    let mut aud_inband: u64 = 0;
    let mut min_nal = usize::MAX;
    let mut max_nal = 0usize;
    let mut last_nal_at = start;
    let mut max_gap = 0.0f64;
    let mut stalls: u64 = 0; // gaps > 1 s
    let mut first_au_at: Option<Instant> = None;
    let mut last_au_at: Option<Instant> = None;
    let mut keepalives: u64 = 0;
    let mut max_keepalive_ms = 0.0f64;
    let mut sum_keepalive_ms = 0.0f64;
    let mut keepalive_errors: u64 = 0;
    let mut io_err: Option<io::Error> = None;
    let mut next_report = start + Duration::from_secs(10);

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if keepalive_on && now >= next_keepalive {
            keepalives += 1;
            let ka_t0 = Instant::now();
            let ka_res = s.keepalive();
            let ka_ms = ka_t0.elapsed().as_secs_f64() * 1000.0;
            if ka_ms > max_keepalive_ms {
                max_keepalive_ms = ka_ms;
            }
            sum_keepalive_ms += ka_ms;
            if ka_ms > 200.0 {
                println!(
                    "keepalive #{} took {:.0} ms at t={:.2}",
                    keepalives,
                    ka_ms,
                    start.elapsed().as_secs_f64()
                );
            }
            if let Err(e) = ka_res {
                keepalive_errors += 1;
                eprintln!("KEEPALIVE_ERROR t={:.2} {e}", start.elapsed().as_secs_f64());
            } else if verbose {
                println!(
                    "keepalive #{} ok at t={:.2}",
                    keepalives,
                    start.elapsed().as_secs_f64()
                );
            }
            next_keepalive = Instant::now() + Duration::from_secs_f64(keepalive_s);
            continue;
        }

        match s.nal_with_marker() {
            Ok(Some((nal, marker))) => {
                let t = start.elapsed().as_secs_f64();
                let gap = last_nal_at.elapsed().as_secs_f64();
                if nals > 0 && gap > max_gap {
                    max_gap = gap;
                }
                if gap > 1.0 {
                    stalls += 1;
                }
                last_nal_at = Instant::now();
                let ty = nal.get(4).map(|b| b & 0x1f).unwrap_or(0); // 4-byte Annex-B prefix
                hist[(ty & 0x1f) as usize] += 1;
                nals += 1;
                bytes += nal.len() as u64;
                if nal.len() < min_nal {
                    min_nal = nal.len();
                }
                if nal.len() > max_nal {
                    max_nal = nal.len();
                }
                if marker {
                    aus += 1;
                    if first_au_at.is_none() {
                        first_au_at = Some(Instant::now());
                    }
                    last_au_at = Some(Instant::now());
                }
                match ty {
                    5 => idr += 1,
                    7 => sps_inband += 1,
                    8 => pps_inband += 1,
                    9 => aud_inband += 1,
                    _ => {}
                }
                if verbose {
                    println!("t={t:.3} nal type={ty} len={} marker={marker}", nal.len());
                }
                if let Some(f) = fh.as_mut() {
                    if let Err(e) = f.write_all(&nal) {
                        eprintln!("probe: write failed: {e}");
                        break;
                    }
                }
            }
            Ok(None) => {
                empties += 1;
            }
            Err(e) => {
                io_err = Some(e);
                break;
            }
        }

        if Instant::now() >= next_report {
            next_report = Instant::now() + Duration::from_secs(10);
            println!(
                "t={:.0}s nals={} aus={} bytes={} cpu={:.0}ms",
                start.elapsed().as_secs_f64(),
                nals,
                aus,
                bytes,
                cpu_ms() - cpu0
            );
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let cpu = cpu_ms() - cpu0;
    if let Some(f) = fh.as_mut() {
        let _ = f.flush();
    }

    println!("\n-- results --");
    if let Some(e) = &io_err {
        println!(
            "STREAM_ERROR at t={:.2}s kind={:?}: {e}",
            elapsed,
            e.kind()
        );
    }
    println!("clock        {elapsed:.3}s wall");
    println!("nals         {nals}  ({:.2}/s)", rate(nals, elapsed));
    println!("aus          {aus}  ({:.2}/s)  [RTP marker bits = access units]", rate(aus, elapsed));
    println!(
        "frames/s     {:.2}  (AUs between first and last marker)",
        match (first_au_at, last_au_at) {
            (Some(a), Some(b)) if aus > 1 => {
                let d = b.duration_since(a).as_secs_f64();
                if d > 0.0 { (aus - 1) as f64 / d } else { 0.0 }
            }
            _ => 0.0,
        }
    );
    println!(
        "bytes        {bytes}  ({:.0} B/s = {:.0} kbit/s)",
        rate(bytes, elapsed),
        rate(bytes, elapsed) * 8.0 / 1000.0
    );
    println!(
        "nal size     min={} avg={:.0} max={}",
        if min_nal == usize::MAX { 0 } else { min_nal },
        if nals > 0 { bytes as f64 / nals as f64 } else { 0.0 },
        max_nal
    );
    println!("empty_reads  {empties}  (Ok(None) returns: audio/RTCP/timeouts)");
    println!("idr          {idr}  (NAL type 5)");
    println!(
        "in_band_ps   SPS={sps_inband} PPS={pps_inband} AUD={aud_inband}"
    );
    println!("max_gap      {max_gap:.3}s   stalls(>1s)={stalls}");
    println!(
        "keepalives   {keepalives} sent, {keepalive_errors} errors, missed={}, \
         max={max_keepalive_ms:.0} ms avg={:.0} ms",
        s.missed_keepalives(),
        if keepalives > 0 { sum_keepalive_ms / keepalives as f64 } else { 0.0 }
    );
    println!("cpu          {cpu:.0} ms  ({:.1}% of one core)", if elapsed > 0.0 { cpu / (elapsed * 1000.0) * 100.0 } else { 0.0 });
    println!("-- nal types --");
    for (ty, n) in hist.iter().enumerate() {
        if *n > 0 {
            println!("type {:2} {:<14} {n}", ty, nal_type_name(ty as u8));
        }
    }
    if let Some(p) = &out {
        println!("wrote        {p}");
    }

    s.close();
    if io_err.is_some() {
        std::process::exit(3);
    }
}

fn rate(n: u64, secs: f64) -> f64 {
    if secs > 0.0 {
        n as f64 / secs
    } else {
        0.0
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}
