#!/bin/sh
# Build led: a host binary (for analysis on firefly) and, if the target's
# std is installed, an ARMv5TE binary to run on the camera itself.
#
# Same target rule as motorctl: armv5te-unknown-linux-musleabi. ARMv6 targets
# (arm-unknown-linux-musleabi) die with SIGILL on the ARM926EJ-S.
set -eu
cd "$(dirname "$0")"

# mise shims a bare `rustc` without a pinned version here; use the toolchain
# directly. Override with RUSTC=... if the toolchain moves.
RUSTC=${RUSTC:-$HOME/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin/rustc}
[ -x "$RUSTC" ] || { echo "rustc not found at $RUSTC (set RUSTC=...)" >&2; exit 1; }

SYSROOT=$("$RUSTC" --print sysroot)
HOST=$("$RUSTC" -vV | sed -n 's/^host: //p')
LLD="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"

mkdir -p target

# host build
"$RUSTC" -O --edition 2021 src/main.rs -o target/led
printf 'host: %s\n' "$(ls -l target/led | awk '{print $5}') bytes"

# ARMv5TE build, optional
if [ -d "$SYSROOT/lib/rustlib/armv5te-unknown-linux-musleabi" ]; then
    "$RUSTC" --target armv5te-unknown-linux-musleabi \
        -C linker="$LLD" -C link-self-contained=yes \
        -O --edition 2021 src/main.rs -o target/led-musleabi
    cp target/led-musleabi target/led-arm
    if command -v llvm-strip >/dev/null 2>&1; then
        llvm-strip --strip-all target/led-arm
    fi
    printf 'armv5te: %s bytes\n' "$(ls -l target/led-arm | awk '{print $5}')"
    # Guard against the ARMv6 mix-up that cost an afternoon on motorctl:
    # Tag_CPU_arch must read v5TE.
    if command -v readelf >/dev/null 2>&1; then
        readelf -A target/led-arm | grep -q 'Tag_CPU_arch: v5TE' \
            || { echo "led-arm is not v5TE!" >&2; exit 1; }
    fi
    sha256sum target/led-arm
else
    echo "armv5te std not installed; skipping the camera binary" >&2
fi
