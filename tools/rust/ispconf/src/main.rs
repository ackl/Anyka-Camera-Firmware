//! ispconf — inspect, validate and patch an Anyka "DV5" ISP configuration
//! container (`isp_<sensor>.conf`).
//!
//! The format was recovered from `libplat_vi.so` (build with debug info,
//! `check_data.constprop.3` / `isp_cfg_file_load`) on the AK3918EN088 camera;
//! see ../../../analysis/video-pipeline.md.
//!
//! A container is a sequence of equally-sized blocks:
//!
//!   off  size  field
//!   0x00    4  version, must be 3            (u32 LE)
//!   0x04   16  variant name, "DV5_092602"    (NUL padded)
//!   0x14    4  sensor id, e.g. 0x1346        (u32 LE)
//!   0x18    2  build year                    (u16 LE)
//!   0x1a    1  build month   (1-12)
//!   0x1b    1  build day     (1-31)
//!   0x1c    1  build hour    (0-23)
//!   0x1d    1  build minute  (0-59)
//!   0x1e    1  build second  (0-59)
//!   0x1f    1  block index   (0-4)   <- not a style id
//!   0x20    1  style id      (0-255) <- the one /etc/jffs2/styleId must equal
//!   0x21    3  zero
//!   0x24    4  block size, header included  (u32 LE)
//!   0x28  ...  zero to 0x200
//!  0x200  ...  module payload (block_size - 512 bytes)
//!
//! The library only ever loads the payload of the block whose index matches the
//! call (`isp_cfg_file_load(0, ...)` -> block index 0), so index 0 is the live
//! main-path config; the others are alternate modes.

use std::fs;
use std::process::ExitCode;

const HEADER: usize = 512;
const MAGIC: &[u8] = b"DV5_";
const MAX_INDEX: u8 = 4;

#[derive(Clone)]
struct Block {
    off: usize,
    version: u32,
    name: String,
    sensor: u32,
    year: u16,
    date: [u8; 5], // month, day, hour, minute, second
    index: u8,
    style: u8,
    size: u32,
    size_from_field: bool,
}

impl Block {
    fn payload(&self) -> usize {
        self.size as usize - HEADER
    }

    fn date_string(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.date[0], self.date[1], self.date[2], self.date[3], self.date[4]
        )
    }

    /// Field rules `check_data.constprop.3` enforces, in its own order.
    fn problems(&self, sensor: u32, style: u8) -> Vec<String> {
        let mut out = Vec::new();
        if self.version != 3 {
            out.push(format!("version is {}, must be 3 (\"config file is old version\")", self.version));
        }
        let [mon, day, hour, min, sec] = self.date;
        if self.year <= 0x76b {
            out.push(format!("year {} is implausible", self.year));
        }
        if !(1..=12).contains(&mon) {
            out.push(format!("month {mon} out of 1..12"));
        }
        if !(1..=31).contains(&day) {
            out.push(format!("day {day} out of 1..31"));
        }
        if hour > 23 {
            out.push(format!("hour {hour} > 23"));
        }
        if min > 59 {
            out.push(format!("minute {min} > 59"));
        }
        if sec > 59 {
            out.push(format!("second {sec} > 59"));
        }
        if self.index > MAX_INDEX {
            out.push(format!("block index {} > {MAX_INDEX}", self.index));
        }
        if self.sensor != sensor {
            out.push(format!(
                "sensor id mismatch: block 0x{:04x}, wanted 0x{:04x} (\"is not need file\")",
                self.sensor, sensor
            ));
        }
        if self.style != style {
            out.push(format!(
                "style id mismatch: block {}, wanted {} (\"is not need file\")",
                self.style, style
            ));
        }
        out
    }
}

fn u32_at(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

fn parse(d: &[u8]) -> (Vec<Block>, Vec<String>) {
    let mut blocks = Vec::new();
    let mut notes = Vec::new();
    let mut pos = 0usize;
    while pos + HEADER <= d.len() {
        if u32_at(d, pos) != 3 || &d[pos + 4..pos + 8] != MAGIC {
            // Not a header here: resynchronise on the next magic.
            match (pos..d.len().saturating_sub(4)).find(|&i| &d[i + 4..i + 8] == MAGIC && u32_at(d, i) == 3) {
                Some(next) => {
                    notes.push(format!(
                        "skipped {} bytes of unrecognised data at 0x{pos:06x}",
                        next - pos
                    ));
                    pos = next;
                    continue;
                }
                None => break,
            }
        }
        let name_end = d[pos + 4..pos + 20].iter().position(|&b| b == 0).unwrap_or(16);
        let name = String::from_utf8_lossy(&d[pos + 4..pos + 4 + name_end]).into_owned();
        let field_size = u32_at(d, pos + 0x24);
        let mut size_from_field = true;
        let size = if field_size as usize >= HEADER && pos + field_size as usize <= d.len() {
            field_size
        } else {
            // Fall back to "next header, or end of file".
            size_from_field = false;
            let next = (pos + HEADER..d.len().saturating_sub(4))
                .find(|&i| &d[i + 4..i + 8] == MAGIC && u32_at(d, i) == 3)
                .unwrap_or(d.len());
            notes.push(format!(
                "block at 0x{pos:06x}: size field {field_size} unusable, using {next}",
                next = next - pos
            ));
            (next - pos) as u32
        };
        blocks.push(Block {
            off: pos,
            version: u32_at(d, pos),
            name,
            sensor: u32_at(d, pos + 0x14),
            year: u16::from_le_bytes([d[pos + 0x18], d[pos + 0x19]]),
            date: [d[pos + 0x1a], d[pos + 0x1b], d[pos + 0x1c], d[pos + 0x1d], d[pos + 0x1e]],
            index: d[pos + 0x1f],
            style: d[pos + 0x20],
            size,
            size_from_field,
        });
        pos += size as usize;
    }
    if pos < d.len() {
        notes.push(format!("{} trailing bytes after the last block", d.len() - pos));
    }
    (blocks, notes)
}

fn parse_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    let (radix, body) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => (16, hex),
        None => (10, s),
    };
    u32::from_str_radix(body, radix).ok()
}

fn usage() -> ! {
    eprintln!(
        "\
ispconf — Anyka \"DV5\" ISP config container inspector

usage:
  ispconf inspect FILE               list blocks: offset, size, sensor, date, index, style
  ispconf check   FILE [--sensor ID] [--style N]
                                     apply libplat_vi.so's own header rules; exit 1 on failure
  ispconf extract FILE --index N [--out FILE]
                                     dump a block payload (the bytes the ISP actually parses)
  ispconf set-style FILE --index N --style N [--out FILE|--in-place]
                                     rewrite one byte: the block's style id at 0x20

numbers accept 0x prefixes. Defaults for check: --sensor 0x1346 --style 0.

libplat_vi.so reads the wanted style from /etc/jffs2/styleId as ONE RAW BYTE
(fread of 1 byte). Writing \"48\" there yields 0x34 ('4') = 52, which is the
classic \"style_id mismatch\" failure. A missing file reads as 0."
    );
    std::process::exit(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        usage();
    }
    let cmd = args[0].as_str();
    let path = &args[1];
    let mut sensor = 0x1346u32;
    let mut style = 0u8;
    let mut index: Option<u8> = None;
    let mut out: Option<String> = None;
    let mut in_place = false;

    let mut i = 2;
    while i < args.len() {
        let (key, inline) = match args[i].split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (args[i].as_str(), None),
        };
        let mut take = |what: &str| -> String {
            match inline.clone() {
                Some(v) => v,
                None => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => v.clone(),
                        None => {
                            eprintln!("ispconf: {what} needs a value");
                            usage()
                        }
                    }
                }
            }
        };
        match key {
            "--sensor" => {
                let v = take("--sensor");
                sensor = parse_u32(&v).unwrap_or_else(|| {
                    eprintln!("ispconf: bad --sensor {v:?}");
                    usage()
                })
            }
            "--style" => {
                let v = take("--style");
                style = parse_u32(&v).unwrap_or_else(|| {
                    eprintln!("ispconf: bad --style {v:?}");
                    usage()
                }) as u8
            }
            "--index" => {
                let v = take("--index");
                index = Some(parse_u32(&v).unwrap_or_else(|| {
                    eprintln!("ispconf: bad --index {v:?}");
                    usage()
                }) as u8)
            }
            "--out" => out = Some(take("--out")),
            "--in-place" => in_place = true,
            other => {
                eprintln!("ispconf: unknown option {other:?}");
                usage()
            }
        }
        i += 1;
    }

    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ispconf: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (blocks, notes) = parse(&data);

    match cmd {
        "inspect" => {
            println!("{path}: {} bytes, {} block(s)", data.len(), blocks.len());
            for b in &blocks {
                println!(
                    "[{}] off=0x{:06x} size={} payload={} ver={} name={} sensor=0x{:04x} built={} index={} style={}{}",
                    b.index,
                    b.off,
                    b.size,
                    b.payload(),
                    b.version,
                    b.name,
                    b.sensor,
                    b.date_string(),
                    b.index,
                    b.style,
                    if b.size_from_field { "" } else { "  (size field unusable)" }
                );
            }
            for n in &notes {
                println!("note: {n}");
            }
            if let Some(b) = blocks.iter().find(|b| b.index == 0) {
                println!(
                    "main path: the library loads block index 0, payload at 0x{:06x} ({} bytes)",
                    b.off + HEADER,
                    b.payload()
                );
                if let Some(last) = blocks.last() {
                    if last.off != b.off {
                        println!(
                            "  (its log line prints the LAST block's build time {}, which is why it can \
                             disagree with the block in use)",
                            last.date_string()
                        );
                    }
                }
            }
            ExitCode::SUCCESS
        }
        "check" => {
            if blocks.is_empty() {
                println!("{path}: no DV5 blocks found — the library would report \"read sensor fail\"");
                return ExitCode::FAILURE;
            }
            let mut bad = 0;
            for b in &blocks {
                let problems = b.problems(sensor, style);
                if problems.is_empty() {
                    println!("block index {} (0x{:06x}): ok", b.index, b.off);
                } else {
                    bad += problems.len();
                    for p in problems {
                        println!("block index {} (0x{:06x}): {p}", b.index, b.off);
                    }
                }
            }
            match blocks.iter().find(|b| b.index == 0) {
                Some(_) => {
                    println!(
                        "wanted sensor 0x{sensor:04x}, style {style}; {} block(s), {bad} problem(s)",
                        blocks.len()
                    );
                }
                None => {
                    bad += 1;
                    println!("no block has index 0 — the library wants one (\"no this subfile\")");
                }
            }
            if bad == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        "extract" => {
            let Some(idx) = index else {
                eprintln!("ispconf: extract needs --index N");
                return ExitCode::FAILURE;
            };
            match blocks.iter().find(|b| b.index == idx) {
                Some(b) => {
                    let payload = &data[b.off + HEADER..b.off + b.size as usize];
                    match &out {
                        Some(f) => match fs::write(f, payload) {
                            Ok(()) => {
                                println!("block {idx} payload: {} bytes -> {f}", payload.len());
                                ExitCode::SUCCESS
                            }
                            Err(e) => {
                                eprintln!("ispconf: {f}: {e}");
                                ExitCode::FAILURE
                            }
                        },
                        None => {
                            use std::io::Write;
                            let _ = std::io::stdout().write_all(payload);
                            ExitCode::SUCCESS
                        }
                    }
                }
                None => {
                    eprintln!("ispconf: no block with index {idx}");
                    ExitCode::FAILURE
                }
            }
        }
        "set-style" => {
            let (Some(idx), style) = (index, style) else {
                eprintln!("ispconf: set-style needs --index N and --style N");
                return ExitCode::FAILURE;
            };
            let Some(b) = blocks.iter().find(|b| b.index == idx) else {
                eprintln!("ispconf: no block with index {idx}");
                return ExitCode::FAILURE;
            };
            let at = b.off + 0x20;
            let old = data[at];
            let mut out_data = data.clone();
            if let Some(ref f) = out {
                out_data[at] = style;
                match fs::write(f, &out_data) {
                    Ok(()) => {
                        println!(
                            "block {idx}: style {old} -> {style} (byte 0x{at:06x}) -> {f}"
                        );
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("ispconf: {f}: {e}");
                        ExitCode::FAILURE
                    }
                }
            } else if in_place {
                out_data[at] = style;
                match fs::write(path, &out_data) {
                    Ok(()) => {
                        println!("block {idx}: style {old} -> {style} (byte 0x{at:06x}), in place");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("ispconf: {path}: {e}");
                        ExitCode::FAILURE
                    }
                }
            } else {
                println!("block {idx}: style is {old}, would become {style} (pass --out FILE or --in-place)");
                ExitCode::SUCCESS
            }
        }
        other => {
            eprintln!("ispconf: unknown command {other:?}");
            usage()
        }
    }
}
