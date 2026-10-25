//! Bounded PTZ motor controller for the AK3918EN088 stock motor driver.
//!
//! Faithful port of `tools/motorctl.c`. Same ioctl numbers, same cooperative
//! `/tmp` lock, same /proc owner scan, same bounded move plus exact stop-event
//! wait. No homing, no position persistence, no hardware fail-safe.

use std::ffi::CString;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// _IOW('m', 0x0b, int) and friends, exactly as the stock kernel expects.
// 32-bit ARM wants the request as a signed int, like the C original.
const IOC_SET_SPEED: libc::c_int = 0x4004_6d0b_u32 as libc::c_int;
const IOC_MOVE_CW: libc::c_int = 0x4004_6d0d_u32 as libc::c_int;
const IOC_MOVE_CCW: libc::c_int = 0x4004_6d0e_u32 as libc::c_int;
const IOC_STOP: libc::c_int = 0x4004_6d10_u32 as libc::c_int;

const STOP_BIT: u32 = 4;
const MOVE_TIMEOUT_MS: u64 = 8_000;
const STOP_TIMEOUT_MS: u64 = 1_500;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Strict base-10 parse of a whole string, bounded to [lo, hi].
fn number(arg: &str, lo: i32, hi: i32) -> Option<i32> {
    let bytes = arg.as_bytes();
    let digits = match bytes.first() {
        Some(b'+') | Some(b'-') => &bytes[1..],
        Some(_) => bytes,
        None => return None,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    match arg.parse::<i32>() {
        Ok(n) if n >= lo && n <= hi => Some(n),
        _ => None,
    }
}

/// Refuse to drive a motor another process already has open.
fn other_owner() -> bool {
    let proc_entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return true,
    };
    let me = unsafe { libc::getpid() };

    for entry in proc_entries.flatten() {
        let pid: libc::pid_t = match entry.file_name().to_string_lossy().parse() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        if pid <= 0 || pid == me {
            continue;
        }
        let fd_dir = format!("/proc/{pid}/fd");
        let fds = match std::fs::read_dir(&fd_dir) {
            Ok(fds) => fds,
            Err(_) => continue,
        };
        for fd in fds.flatten() {
            let link = fd_dir.clone() + "/" + &fd.file_name().to_string_lossy();
            if let Ok(target) = std::fs::read_link(&link) {
                let target = target.to_string_lossy();
                if target == "/dev/ak-motor0" || target == "/dev/ak-motor1" {
                    eprintln!("Refusing: PID {pid} owns {target}");
                    return true;
                }
            }
        }
    }
    false
}

/// Block until the stock driver raises stop bit 4 in event[1], or time out.
///
/// The stock `read` copies 12 bytes into the buffer but returns zero, so a
/// zero-length read must not be treated as end-of-file.
fn wait_stop(fd: RawFd, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return false;
        }
        if ready > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let mut event = [0u32; 3];
            let n = unsafe {
                libc::read(
                    fd,
                    event.as_mut_ptr().cast::<libc::c_void>(),
                    std::mem::size_of_val(&event),
                )
            };
            if n < 0 {
                return false;
            }
            if event[1] & STOP_BIT != 0 {
                println!("Stop event: flags={} remaining={}", event[1], event[2]);
                return true;
            }
        }
        if interrupted() {
            return false;
        }
    }
    false
}

fn ioctl_int(fd: RawFd, request: libc::c_int, value: &mut libc::c_int) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request, value as *mut libc::c_int) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Matches C's `perror`: "what: strerror(errno)" with no extra decoration.
fn fail(what: &str, err: io::Error) -> u8 {
    let text = match err.raw_os_error() {
        Some(code) => unsafe {
            std::ffi::CStr::from_ptr(libc::strerror(code))
                .to_string_lossy()
                .into_owned()
        },
        None => err.to_string(),
    };
    eprintln!("{what}: {text}");
    1
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        println!(
            "Idle: no motor opened or moved. Usage: motorctl AXIS(0|1) DIR(0|1) STEPS(1..32) SPEED(1..10)"
        );
        return std::process::ExitCode::from(0);
    }
    if args.len() != 4 {
        return std::process::ExitCode::from(2);
    }

    let (axis, dir, steps, speed) = match (
        number(&args[0], 0, 1),
        number(&args[1], 0, 1),
        number(&args[2], 1, 32),
        number(&args[3], 1, 10),
    ) {
        (Some(a), Some(d), Some(s), Some(v)) => (a, d, s, v),
        _ => {
            eprintln!("Invalid argument; nothing opened.");
            return std::process::ExitCode::from(2);
        }
    };

    // Cooperative lock: not an atomic kernel reservation, so never run a
    // second PTZ application concurrently.
    let lock = match Lock::acquire() {
        Ok(lock) => lock,
        Err(e) => return std::process::ExitCode::from(fail("lock", e)),
    };

    if other_owner() {
        return std::process::ExitCode::from(1);
    }

    unsafe {
        libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as libc::sighandler_t);
    }

    let dev = CString::new(format!("/dev/ak-motor{axis}")).expect("device path");
    let fd = unsafe {
        libc::open(
            dev.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return std::process::ExitCode::from(fail("open motor", io::Error::last_os_error()));
    }

    let mut zero: libc::c_int = 0;
    let mut rc: u8 = 1;
    let mut speed_w = speed;
    let mut steps_w = steps;

    if let Err(e) = ioctl_int(fd, IOC_SET_SPEED, &mut speed_w) {
        rc = fail("speed", e);
    } else if interrupted() {
        // fall through to the stop path
    } else {
        println!("Axis {axis} direction {dir}: {steps} phase steps, speed {speed}");
        let _ = io::Write::flush(&mut io::stdout());

        if let Err(e) = ioctl_int(fd, if dir == 1 { IOC_MOVE_CCW } else { IOC_MOVE_CW }, &mut steps_w) {
            rc = fail("move", e);
        } else if wait_stop(fd, MOVE_TIMEOUT_MS) {
            rc = 0;
        } else {
            eprintln!("No normal completion (or interrupted); requesting stop.");
            unsafe { libc::ioctl(fd, IOC_STOP, &mut zero) };
            INTERRUPTED.store(false, Ordering::SeqCst);
            if !wait_stop(fd, STOP_TIMEOUT_MS) {
                eprintln!("STOP NOT CONFIRMED: remove motor power if still moving.");
            }
        }
    }

    unsafe {
        libc::ioctl(fd, IOC_STOP, &mut zero);
        libc::close(fd);
    }
    drop(lock);
    std::process::ExitCode::from(rc)
}

/// Cooperative exclusive lock on `/tmp/motorctl.lock`, released on drop.
struct Lock(std::fs::File);

impl Lock {
    fn acquire() -> io::Result<Lock> {
        use std::os::unix::io::FromRawFd;
        let fd = unsafe {
            libc::open(
                c"/tmp/motorctl.lock".as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Own the fd immediately so any early return still closes it.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Lock(file))
    }
}
