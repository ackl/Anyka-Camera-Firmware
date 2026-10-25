//! irctl — day/night control for the YI CB211 (Anyka AK3918EN088).
//!
//! The stock IR path is dead in Factory mode: libplat_vi.so's
//! `ak_drv_ir_get_input_level` logs "not inited" because the photo-sensor GPIO
//! is configured -1 in the stock kernel. But the hardware is still reachable
//! through the Anyka user-gpio sysfs exports:
//!
//!     /sys/user-gpio/IR_LED    IR illuminator power (1 = on)
//!     /sys/user-gpio/ircut_a   IR-cut solenoid, one coil
//!     /sys/user-gpio/ircut_b   IR-cut solenoid, other coil
//!
//! The IR-cut filter sits in a *latching* solenoid: pulse one coil to move it,
//! pulse the other to move it back, and it stays where it was pushed with both
//! lines at 0. That is why this tool pulses (1, sleep, 0) instead of holding a
//! level, and why the current position cannot be read back — the registers only
//! ever show 0 after a move. Position is therefore remembered in `$STATE`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::thread::sleep;
use std::time::Duration;

const DEFAULT_GPIO_DIR: &str = "/sys/user-gpio";
const STATE_FILE: &str = "/tmp/irctl.mode";
const PULSE_MS: u64 = 300;

struct Gpio {
    dir: PathBuf,
}

impl Gpio {
    fn new(dir: PathBuf) -> Self {
        Gpio { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read(&self, name: &str) -> Result<String, String> {
        let p = self.path(name);
        match fs::read_to_string(&p) {
            Ok(s) => Ok(s.trim().to_string()),
            Err(e) => Err(format!("read {}: {}", p.display(), e)),
        }
    }

    fn write(&self, name: &str, val: &str) -> Result<(), String> {
        let p = self.path(name);
        // The user-gpio attrs are plain 0666 files taking "0"/"1" (plus NL is fine).
        fs::write(&p, val).map_err(|e| format!("write {} <- {}: {}", p.display(), val, e))
    }

    /// Drive a latching coil: raise, hold briefly, drop. Leaves the mechanism
    /// where it was pushed.
    fn pulse(&self, name: &str) -> Result<(), String> {
        self.write(name, "1")?;
        sleep(Duration::from_millis(PULSE_MS));
        self.write(name, "0")
    }
}

fn usage() -> ! {
    eprintln!(
        "irctl — day/night mode for the Anyka AK3918EN088 camera

usage: irctl <command>

commands:
  day            IR-cut filter in, IR illuminator off   (normal colour)
  night          IR-cut filter out, IR illuminator on   (mono-ish, sees in the dark)
  ir on|off      only switch the IR illuminator, leave the filter alone
  filter in|out  only move the IR-cut filter
  status         print raw GPIO values and the remembered mode

env:
  ANYKA_GPIO_DIR  default {DEFAULT_GPIO_DIR}
  IRCTL_STATE     default {STATE_FILE}

notes:
  The IR-cut solenoid is latching and write-only, so the filter position is
  remembered in the state file, not read from hardware. After a reboot the
  filter is wherever it physically was; `irctl day` or `irctl night` re-latches
  it deterministically, so run one of those if in doubt.
  AE adapts over a second or two after a switch: the first frames may look
  washed out or tinted."
    );
    exit(2)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let gpio_dir = env::var("ANYKA_GPIO_DIR").unwrap_or_else(|_| DEFAULT_GPIO_DIR.to_string());
    let state_path = env::var("IRCTL_STATE").unwrap_or_else(|_| STATE_FILE.to_string());
    let g = Gpio::new(PathBuf::from(gpio_dir));

    let result = match args[0].as_str() {
        "day" => set_mode(&g, &state_path, "day"),
        "night" => set_mode(&g, &state_path, "night"),
        "ir" => match args.get(1).map(|s| s.as_str()) {
            Some("on") => set_ir(&g, true),
            Some("off") => set_ir(&g, false),
            _ => usage(),
        },
        "filter" => match args.get(1).map(|s| s.as_str()) {
            Some("in") => move_filter(&g, "in"),
            Some("out") => move_filter(&g, "out"),
            _ => usage(),
        },
        "status" => status(&g, &state_path),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("irctl: unknown command '{}'", other);
            usage()
        }
    };

    match result {
        Ok(msg) => {
            println!("{}", msg);
        }
        Err(e) => {
            eprintln!("irctl: {}", e);
            exit(1);
        }
    }
}

fn set_ir(g: &Gpio, on: bool) -> Result<String, String> {
    g.write("IR_LED", if on { "1" } else { "0" })?;
    Ok(format!("IR illuminator {}", if on { "on" } else { "off" }))
}

/// ircut_a = "filter out" (night), ircut_b = "filter in" (day), as observed on
/// the board: pulsing a with the illuminator on floods the image with IR.
fn move_filter(g: &Gpio, pos: &str) -> Result<String, String> {
    match pos {
        "out" => g.pulse("ircut_a")?,
        "in" => g.pulse("ircut_b")?,
        _ => return Err(format!("bad filter position '{}'", pos)),
    }
    Ok(format!("IR-cut filter {}", pos))
}

fn set_mode(g: &Gpio, state_path: &str, mode: &str) -> Result<String, String> {
    let mut out = Vec::new();
    if mode == "night" {
        out.push(move_filter(g, "out")?);
        out.push(set_ir(g, true)?);
    } else {
        out.push(set_ir(g, false)?);
        out.push(move_filter(g, "in")?);
    }
    // Best-effort: the state file is a convenience, not truth.
    let _ = fs::write(state_path, format!("{}\n", mode));
    Ok(out.join("; "))
}

fn status(g: &Gpio, state_path: &str) -> Result<String, String> {
    let mut lines = Vec::new();
    for name in ["IR_LED", "ircut_a", "ircut_b"] {
        match g.read(name) {
            Ok(v) => lines.push(format!("{} = {}", name, v)),
            Err(e) => lines.push(format!("{} = ? ({})", name, e)),
        }
    }
    let remembered = fs::read_to_string(state_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    lines.push(format!("remembered mode = {}", remembered));
    Ok(lines.join("\n"))
}

#[allow(dead_code)]
fn exists(p: &Path) -> bool {
    p.exists()
}
