# motorctl, in Rust — 2026-10-24

Port of `tools/motorctl.c` to Rust. Now the deployed `/mnt/Factory/custom/motorctl`
on the camera; the C build is kept alongside as `motorctl.c-binary`.

Same contract as the C version: bounded relative movement, no homing, no saved
position, no hardware fail-safe.

## Toolchain (the one thing that matters)

    rustc --version      # 1.99.1
    ./build.sh

Target is **`armv5te-unknown-linux-musleabi`**, not `arm-unknown-linux-musleabi`.

`arm-unknown-linux-musleabi` is Rust's Raspberry Pi Zero target and is built for
**ARMv6**. The AK3918EN088 is an **ARM926EJ-S (v5TEJ)**, so everything built for
that target dies immediately with `Illegal instruction` (SIGILL, rc=132) — the
ELF even advertises `Tag_CPU_arch: v6` and `Tag_CPU_unaligned_access: v6`.

`armv5te-unknown-linux-musleabi` does ship a prebuilt `rust-std` component, and
its `crt1.o` is genuinely `Tag_CPU_arch: v5TE`, so no nightly, no `-Z build-std`,
and no external ARM toolchain is needed. Linking uses the `rust-lld` bundled in
the rustc sysroot plus the self-contained musl archive:

    linker = "<sysroot>/lib/rustlib/<host>/bin/rust-lld"
    rustflags = ["-C", "link-self-contained=yes"]

That was also the reason `zig cc -target arm-linux-musleabi` worked for the C
build: the same v5TE/musl target.

Sizes: C `40,320` bytes, Rust `361,060` bytes static (mostly std + fmt). Plenty of
room on the 29.7 GB SD card. If size ever matters, `no_std` would cut it to tens
of KB, at the cost of hand-rolled `getdents`/`poll`/signal plumbing.

## What is deliberately identical to the C

* ioctl requests `0x40046d0b` (speed), `0x40046d0d`/`0x40046d0e` (move), `0x40046d10` (stop).
* Cooperative `flock` on `/tmp/motorctl.lock`, released on drop.
* `/proc/*/fd` readlink scan refusing to run if any other PID owns
  `/dev/ak-motor0` or `/dev/ak-motor1` — still not an atomic kernel reservation,
  so a second PTZ program must never run concurrently.
* 8 s move wait, 1.5 s post-stop confirmation, 50 ms poll interval, stop bit 4 in
  event word 1.
* `read()` returning 0 is **not** treated as EOF: the stock driver copies 12 bytes
  into the buffer and reports a zero-length read.
* `perror`-style messages (`lock: Resource temporarily unavailable`), so the
  output is byte-identical to the C binary.
* Interrupt handling on SIGINT/SIGTERM/SIGHUP sets a flag checked in the wait
  loop; on abort the stop ioctl is issued and re-confirmed.

## Parity tests run on the camera (2026-10-24)

| Case | C | Rust |
| --- | --- | --- |
| no args (idle, opens nothing) | rc 0, same text | rc 0, same text |
| 3 args / wrong count | rc 2 | rc 2 |
| out-of-range or non-numeric args | rc 2, "Invalid argument; nothing opened." | same |
| `0 0 16 5` | `flags=4 remaining=0` rc 0 | identical |
| `1 0 16 5` | `flags=4 remaining=0` rc 0 | identical |
| `0 1 12 5` (reverse) | — | `flags=4 remaining=0` rc 0 |
| `0 0 32 1` (slowest, ~6.4 s) | — | completes normally, rc 0 |
| lock held by another process | `lock: Resource temporarily unavailable` rc 1 | identical |
| `/dev/ak-motor0` held by `sleep` | `Refusing: PID nn owns /dev/ak-motor0` rc 1 | identical |
| **SIGINT during `1 0 32 1`** | never tested live | "No normal completion (or interrupted); requesting stop." then `flags=4 remaining=27`, rc 1 |

The interrupt case is the first live verification of the stop path for either
implementation: the motor aborted mid-move and the stop event was confirmed with
27 steps outstanding. Motors were mechanically unloaded throughout; shaft motion
and axis mapping still rest on the driver's own reporting.

## Still true from the C notes

Web PTZ buttons target the old `ptz_daemon` and do not drive this CLI. There are
no end-stops, so nothing here knows where the head is pointing.
