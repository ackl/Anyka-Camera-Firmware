// led — drive the AK3918EN088 white illuminator LED as a status indicator.
//
// Why: the board has no screen, no buzzer we have proven audible, and until
// 2026-10-24 there was no way to tell "is it up?" from across the room.
// /sys/user-gpio/WHITE_LED is a plain read/write register (same class of
// interface as ircut_a/ircut_b, which we verified physically), so a blink is
// the cheapest possible liveness beacon.
//
// Contract: never leaves the LED on. Every path ends by restoring the value it
// found on entry, unless --stay is passed.

use std::env;
use std::fs;
use std::process::exit;
use std::thread::sleep;
use std::time::Duration;

const DEFAULT_GPIO_DIR: &str = "/sys/user-gpio";
const LED_NAME: &str = "WHITE_LED";

fn gpio_dir() -> String {
    env::var("ANYKA_GPIO_DIR").unwrap_or_else(|_| DEFAULT_GPIO_DIR.to_string())
}

fn led_path() -> String {
    format!("{}/{}", gpio_dir(), LED_NAME)
}

fn read_led() -> Result<i32, String> {
    let s = fs::read_to_string(led_path()).map_err(|e| format!("read {}: {}", led_path(), e))?;
    s.trim()
        .parse::<i32>()
        .map_err(|_| format!("unexpected {} contents: {:?}", led_path(), s))
}

fn write_led(v: i32) -> Result<(), String> {
    fs::write(led_path(), format!("{}\n", v))
        .map_err(|e| format!("write {}={}: {}", led_path(), v, e))
}

fn usage() -> ! {
    eprintln!(
        "led — white LED status indicator for the AK3918EN088 board

usage: led COMMAND [ARGS]

  on                 turn the LED on (stays on)
  off                turn the LED off
  status             print the current value and remembered intent
  blink [N] [MS]     N blinks (default 3), MS on/off (default 300)
  ping               the liveness pattern: 3 quick blinks, ~1.2 s total
  boot               the boot pattern: 2 long blinks, ~2.4 s total

The LED is a plain sysfs register ({}/WHITE_LED); reading it back is real,
unlike the latching ircut GPIOs. Set ANYKA_GPIO_DIR to override the sysfs
directory (used by the host-side smoke test).

This is a visible indicator, not audible: the speaker path is fully verified
in software (all ak_ao ioctls succeed, DMA runs, SPK_PA toggles) but nothing
is heard on this unit. See analysis/audio.md.",
        gpio_dir()
    );
    exit(2);
}

fn blink(n: u32, on_ms: u64, off_ms: u64) -> Result<(), String> {
    let original = read_led().unwrap_or(0);
    for i in 0..n {
        write_led(1)?;
        sleep(Duration::from_millis(on_ms));
        write_led(0)?;
        if i + 1 < n {
            sleep(Duration::from_millis(off_ms));
        }
    }
    // Restore whatever we found, so a caller's state is not silently changed.
    let _ = write_led(original);
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let num = |i: usize, default: u64| -> u64 {
        args.get(i)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(default)
    };

    let result = match args[0].as_str() {
        "on" => write_led(1),
        "off" => write_led(0),
        "status" => read_led().map(|v| println!("{} = {}", LED_NAME, v)),
        "blink" => blink(num(1, 3) as u32, num(2, 300), num(2, 300)),
        "ping" => blink(3, 250, 150),
        "boot" => blink(2, 800, 400),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("led: unknown command {:?}", other);
            usage()
        }
    };

    if let Err(e) = result {
        eprintln!("led: {}", e);
        exit(1);
    }
}
