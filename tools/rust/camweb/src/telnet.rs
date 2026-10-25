//! Run shell commands on the camera over its stock unauthenticated root telnet
//! (port 2323). Rust port of `tools/cam`'s protocol, so the panel has no Python
//! dependency:
//!
//! * drain the banner and the `#` prompt,
//! * send the command, then a `printf` marker carrying `$?`,
//! * read until the marker, answering telnet IAC negotiation with refusals.
//!
//! The channel is flaky on ANSI-heavy output (the busybox prompt repaints), so a
//! read that times out is reported as such rather than silently truncated: call
//! `run` again instead of assuming the command failed.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub const DEFAULT_HOST: &str = "192.168.1.103";
pub const DEFAULT_PORT: u16 = 2323;

const MARK: &str = "__CAM_EOF__";
const IAC: u8 = 255;
const DONT: u8 = 254;
const WONT: u8 = 252;

/// One connection target on the board's telnet shell.
pub struct Board {
    pub host: String,
    pub port: u16,
}

impl Board {
    pub fn new(host: &str, port: u16) -> Board {
        Board {
            host: host.to_string(),
            port,
        }
    }

    pub fn from_env() -> Board {
        let host = std::env::var("CAM_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());
        let port = std::env::var("CAM_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        Board::new(&host, port)
    }

    fn connect(&self, timeout: Duration) -> io::Result<TcpStream> {
        let addr = format!("{}:{}", self.host, self.port);
        let mut last = io::Error::new(io::ErrorKind::NotFound, "no address resolved");
        for a in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&a, timeout) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Run `cmd` as a shell line. Returns the exit status and the output text.
    pub fn run(&self, cmd: &str, timeout: Duration) -> io::Result<(i32, String)> {
        let mut s = self.connect(Duration::from_secs(5))?;
        s.set_read_timeout(Some(Duration::from_millis(200)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;

        let mut scratch = Vec::new();
        // Banner + prompt. Whatever arrives in the first ~1.2 s is discarded.
        drain(&mut s, &mut scratch, Instant::now() + Duration::from_millis(1200), true)?;

        // telnetd runs a pty, so every command is echoed back before it runs and
        // the reply would be half echo. Turn the echo off first (the stty line
        // itself is still echoed, hence the second drain) so callers get output
        // only. Harmless if busybox has no stty applet.
        s.write_all(b"stty -echo 2>/dev/null\n")?;
        s.flush()?;
        let mut junk = Vec::new();
        drain(
            &mut s,
            &mut junk,
            Instant::now() + Duration::from_millis(250),
            false,
        )?;

        // The pty echoes the command back, so the marker must not appear whole
        // on the command line: it is sent as two adjacent strings, exactly like
        // tools/cam does, and only the *printed* "__CAM_EOF__ <rc>" matches.
        let (mark_a, mark_b) = MARK.split_at(6);
        let script = format!(
            "{}\nprintf \"\\n%s%s %s\\n\" \"{}\" \"{}\" \"$?\"\n",
            cmd.trim_end_matches('\n'),
            mark_a,
            mark_b
        );
        s.write_all(script.as_bytes())?;
        s.flush()?;

        let deadline = Instant::now() + timeout;
        let needle = format!("{} ", MARK);
        let mut text: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            if let Some(pos) = find(&text, needle.as_bytes()) {
                let digits: String = text[pos + needle.len()..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit() || **c == b'-')
                    .map(|c| *c as char)
                    .collect();
                let rc = digits.parse().unwrap_or(-1);
                text.truncate(pos);
                let clean = String::from_utf8_lossy(&text)
                    .replace('\u{1b}', "");
                // The pty also echoes the marker plumbing itself; drop it.
                let clean: Vec<&str> = clean
                    .lines()
                    .filter(|l| !(l.contains("__CAM_") && l.contains("EOF__")))
                    .filter(|l| !(l.trim_end().ends_with('#') && l.trim().len() <= 4))
                    .collect();
                return Ok((rc, clean.join("\n").trim_end().to_string()));
            }
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("telnet timeout after {:?}", timeout),
                ));
            }
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => text.extend_from_slice(&strip_iac(&mut s, &buf[..n])),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    continue
                }
                Err(e) => return Err(e),
            }
        }

        let clean = String::from_utf8_lossy(&text)
            .replace('\u{1b}', "")
            .trim_end()
            .to_string();
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("telnet closed before the marker: {:?}", tail(&clean, 200)),
        ))
    }
}

fn tail(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

/// Read for a fixed window, discarding (used to swallow the login banner).
fn drain(s: &mut TcpStream, out: &mut Vec<u8>, until: Instant, stop_at_prompt: bool) -> io::Result<()> {
    let mut buf = [0u8; 4096];
    loop {
        if Instant::now() > until {
            return Ok(());
        }
        if stop_at_prompt && out.contains(&b'#') {
            return Ok(());
        }
        match s.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => out.extend_from_slice(&strip_iac(s, &buf[..n])),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Drop telnet IAC sequences, refusing every option the daemon offers.
fn strip_iac(s: &mut TcpStream, buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == IAC && i + 2 < buf.len() {
            match buf[i + 1] {
                251 => {
                    let _ = s.write_all(&[IAC, DONT, buf[i + 2]]);
                    i += 3;
                    continue;
                }
                253 => {
                    let _ = s.write_all(&[IAC, WONT, buf[i + 2]]);
                    i += 3;
                    continue;
                }
                252 | 254 => {
                    i += 3;
                    continue;
                }
                _ => {
                    i += 2;
                    continue;
                }
            }
        }
        out.push(buf[i]);
        i += 1;
    }
    out
}
