//! `camingest` — the receiving end of the camera's batch uploader.
//!
//! The camera (`camrtsp` on an AK3918 ARMv5TE board behind an LTE hotspot)
//! records motion segments to its SD card and pushes them out when it can reach
//! this box. The wire protocol is defined *there*, in
//! `tools/rust/camrtsp/src/upload.rs` — "cam-ingest", wire protocol v1, five
//! handlers plus an optional reset — and this is the deliberately dumb, strict
//! other half of it:
//!
//! * chunks land in `NAME.part`, at the offset we report and nowhere else;
//! * a segment only ever appears by an atomic rename after the sha256 we
//!   computed over **what we stored** matched what the camera declared;
//! * the camera deletes its only copy on that echo, so the echo is computed
//!   from the file we just wrote and never copied from the request;
//! * nothing outside `<root>/rec` is reachable from a URL, and no request is
//!   ever left to block forever.
//!
//! Pure std, no crates: `rustc --edition 2021 src/main.rs -o camingest`.
//!
//! ```text
//! camingest serve --listen 0.0.0.0:8080 --root /srv/storage/cam \
//!                 --prefix /ingest --token "$CAMINGEST_TOKEN"
//! ```

#![forbid(unsafe_code)]

mod http;
mod server;
mod sha256;
mod store;
#[cfg(test)]
mod tests;

pub const VERSION: &str = "0.1.0";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        None => {
            usage(&mut std::io::stderr());
            2
        }
        Some("serve") => cmd_serve(&args[1..]),
        Some("help") | Some("--help") | Some("-h") => {
            usage(&mut std::io::stdout());
            0
        }
        Some("version") | Some("--version") | Some("-V") => {
            println!("camingest {VERSION}");
            0
        }
        Some(other) => {
            eprintln!("camingest: unknown command {other:?}\n");
            usage(&mut std::io::stderr());
            2
        }
    }
}

fn cmd_serve(args: &[String]) -> i32 {
    let mut cfg = server::Cfg::default();
    let mut token_arg: Option<String> = None;
    let mut prefix_arg: Option<String> = None;

    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--help" | "-h" => {
                usage(&mut std::io::stdout());
                return 0;
            }
            "--version" | "-V" => {
                println!("camingest {VERSION}");
                return 0;
            }
            "--quiet" => {
                cfg.quiet = true;
                i += 1;
            }
            "--listen" | "--root" | "--prefix" | "--token" | "--max-conns" | "--max-chunk"
            | "--max-json" | "--io-timeout" => {
                let value = match args.get(i + 1) {
                    Some(v) => v.clone(),
                    None => {
                        eprintln!("camingest: {a} needs a value");
                        return 2;
                    }
                };
                i += 2;
                match a {
                    "--listen" => cfg.listen = value,
                    "--root" => cfg.root = std::path::PathBuf::from(value),
                    "--prefix" => prefix_arg = Some(value),
                    "--token" => token_arg = Some(value),
                    "--max-conns" => match digits(&value) {
                        Some(n) if n > 0 => cfg.max_conns = n as usize,
                        _ => {
                            eprintln!("camingest: --max-conns needs a positive integer");
                            return 2;
                        }
                    },
                    "--max-chunk" => match bytes(&value) {
                        Some(n) if n > 0 => cfg.max_chunk = n as usize,
                        _ => {
                            eprintln!("camingest: --max-chunk needs a byte count (e.g. 8M)");
                            return 2;
                        }
                    },
                    "--max-json" => match bytes(&value) {
                        Some(n) if n > 0 => cfg.max_json = n as usize,
                        _ => {
                            eprintln!("camingest: --max-json needs a byte count (e.g. 256K)");
                            return 2;
                        }
                    },
                    "--io-timeout" => match digits(&value) {
                        Some(n) if n > 0 => cfg.io_timeout = std::time::Duration::from_secs(n),
                        _ => {
                            eprintln!("camingest: --io-timeout needs a positive number of seconds");
                            return 2;
                        }
                    },
                    _ => unreachable!(),
                }
            }
            other => {
                eprintln!("camingest: unknown argument {other:?} (try --help)");
                return 2;
            }
        }
    }

    // The prefix is the path half of the camera's base_url, so it has to match
    // what the operator typed there or every request is a 404.
    match server::normalise_prefix(prefix_arg.as_deref().unwrap_or("/ingest")) {
        Ok(p) => cfg.prefix = p,
        Err(e) => {
            eprintln!("camingest: --prefix: {e}");
            return 2;
        }
    }

    // A receiver that faces the internet with no token is a receiver that
    // accepts anything from anyone. Refuse to start rather than be that.
    let token = token_arg.or_else(|| std::env::var("CAMINGEST_TOKEN").ok());
    let token = token.unwrap_or_default();
    if token.is_empty() {
        eprintln!(
            "camingest: refusing to start without a token: pass --token or set CAMINGEST_TOKEN\n\
             \x20          (the camera sends it as X-Cam-Token; the same value goes in its config)"
        );
        return 2;
    }
    cfg.token = token.into_bytes();

    let srv = match server::Server::bind(cfg.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("camingest: cannot listen on {:?}: {e}", cfg.listen);
            return 3;
        }
    };

    // Never the token itself in a log line.
    let where_to = srv
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| cfg.listen.clone());
    println!(
        "camingest {VERSION} listening on {where_to} root={} prefix={} token=set({} bytes) max_conns={} max_chunk={} max_json={}",
        srv.store().rec_dir().display(),
        if cfg.prefix.is_empty() { "/" } else { &cfg.prefix },
        cfg.token.len(),
        cfg.max_conns,
        cfg.max_chunk,
        cfg.max_json
    );

    match srv.serve_forever() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("camingest: listener failed: {e}");
            3
        }
    }
}

fn digits(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse().ok()
}

/// `256K`, `8M`, `1G`, or a plain byte count.
fn bytes(s: &str) -> Option<u64> {
    let t = s.trim();
    let (num, mult) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => match c.to_ascii_uppercase() {
            'K' => (&t[..i], 1 << 10),
            'M' => (&t[..i], 1 << 20),
            'G' => (&t[..i], 1 << 30),
            _ => return None,
        },
        _ => (t, 1u64),
    };
    digits(num).map(|n| n.saturating_mul(mult))
}

fn usage(out: &mut dyn std::io::Write) {
    let _ = writeln!(
        out,
        "\
camingest {VERSION} — the receiving end of the camera's batch uploader
(cam-ingest wire protocol v1, defined by camrtsp/src/upload.rs)

Usage:
  camingest serve [options]
  camingest --help
  camingest --version

serve options:
  --listen ADDR       address to bind (default 0.0.0.0:8080)
  --root DIR          storage root; segments land in DIR/rec
                      (default /srv/storage/cam)
  --prefix PATH       URL prefix the camera's base_url ends with (default /ingest)
  --token T           token every request must carry, as X-Cam-Token or
                      Authorization: Bearer. Defaults to $CAMINGEST_TOKEN.
                      Required: the server will not start without one.
  --max-conns N       concurrent connections (default {mc})
  --max-chunk BYTES   largest chunk body accepted (default 8M)
  --max-json BYTES    largest sidecar accepted (default 256K)
  --io-timeout SECS   socket read/write timeout (default 30)
  --quiet             no per-request lines (startup and fatal errors still print)

Handlers:
  HEAD   PREFIX/NAME            -> 200 X-Stored-Sha256, X-Stored-Bytes | 404
  HEAD   PREFIX/NAME.part       -> 200 X-Upload-Offset | 404
  PUT    PREFIX/NAME.part       -> 200 X-Upload-Offset | 409 X-Upload-Offset
  POST   PREFIX/NAME.finalize   -> 200 X-Stored-Sha256
  PUT    PREFIX/NAME.json       -> 200 X-Stored-Sha256
  DELETE PREFIX/NAME.part       -> 200

Storage:
  <root>/rec/NAME.h264        a segment, only after its sha256 checked out
  <root>/rec/NAME.json        its sidecar, written whole then renamed
  <root>/rec/NAME.h264.part   bytes in flight",
        mc = server::DEFAULT_MAX_CONNS
    );
}
