#!/bin/sh
# Build the ARMv5TE static binary for the AK3918EN088 camera.
#
# The target is armv5te-unknown-linux-musleabi: a prebuilt std component whose
# crt/libc are compiled for v5TE, which is what the ARM926EJ-S understands.
# (arm-unknown-linux-musleabi is ARMv6 and dies with SIGILL on this SoC.)
#
# Linking uses rustc's bundled rust-lld with the self-contained musl sysroot,
# so no external ARM toolchain or zig is needed.
set -eu
cd "$(dirname "$0")"
SYSROOT=$(rustc --print sysroot)
LLD="$SYSROOT/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/rust-lld"
[ -x "$LLD" ] || { echo "rust-lld not found at $LLD" >&2; exit 1; }
mkdir -p .cargo
cat > .cargo/config.toml <<CFG
[target.armv5te-unknown-linux-musleabi]
linker = "$LLD"
rustflags = ["-C", "link-self-contained=yes"]
CFG
cargo build --release --target armv5te-unknown-linux-musleabi
BIN=target/armv5te-unknown-linux-musleabi/release/motorctl
"$SYSROOT/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/rust-objcopy" --version >/dev/null 2>&1 || true
printf '\n%s\n' "$BIN"
ls -l "$BIN"
sha256sum "$BIN"
