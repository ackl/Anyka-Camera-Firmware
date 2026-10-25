#!/bin/sh
# Build camrtsp: a host binary (for protocol work/tests on firefly) and the
# ARMv5TE static binary that runs on the camera.
#
# Same target rule as motorctl/ispconf: armv5te-unknown-linux-musleabi, never
# the ARMv6 arm-unknown-linux-musleabi (SIGILL before main on ARM926EJ-S).
#
# On firefly: RAM-bound, so prefer build-01 for the cross build:
#   rsync -a --exclude target src Cargo.toml build.sh build-01:src/anyka/tools/rust/camrtsp/
#   ssh build-01 'cd ~/src/anyka/tools/rust/camrtsp && ./build.sh'
set -eu
cd "$(dirname "$0")"

RUSTC=${RUSTC:-$HOME/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin/rustc}
[ -x "$RUSTC" ] || { echo "rustc not found at $RUSTC (set RUSTC=...)" >&2; exit 1; }

SYSROOT=$("$RUSTC" --print sysroot)
HOST=$("$RUSTC" -vV | sed -n 's/^host: //p')
LLD="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"
mkdir -p target

# Cargo's release profile (opt-level=s, LTO, panic=abort, one CGU) is what keeps
# the static binary small — a bare -O leaves every reachable std path in and
# costs ~5 MB. Same flags on both builds so host and camera behave alike.
PROFILE="-C opt-level=s -C lto=fat -C panic=abort -C codegen-units=1"

# host build (fast, for tests and protocol work)
# shellcheck disable=SC2086
"$RUSTC" $PROFILE --edition 2021 src/main.rs -o target/camrtsp
printf 'host: %s bytes\n' "$(ls -l target/camrtsp | awk '{print $5}')"

# unit tests on the host
if [ "${SKIP_TESTS:-0}" != "1" ]; then
    "$RUSTC" --test --edition 2021 src/main.rs -o target/camrtsp-tests
    ./target/camrtsp-tests --quiet
fi

# ARMv5TE build, optional (needs the target's std)
if [ -d "$SYSROOT/lib/rustlib/armv5te-unknown-linux-musleabi" ]; then
    # shellcheck disable=SC2086
    "$RUSTC" --target armv5te-unknown-linux-musleabi \
        -C linker="$LLD" -C link-self-contained=yes $PROFILE \
        --edition 2021 src/main.rs -o target/camrtsp-musleabi
    cp target/camrtsp-musleabi target/camrtsp-arm
    if command -v llvm-strip >/dev/null 2>&1; then
        llvm-strip --strip-all target/camrtsp-arm
    fi
    printf 'armv5te: %s bytes\n' "$(ls -l target/camrtsp-arm | awk '{print $5}')"
    if command -v readelf >/dev/null 2>&1; then
        readelf -A target/camrtsp-arm | grep -q 'Tag_CPU_arch: v5TE' \
            || { echo "camrtsp-arm is not v5TE!" >&2; exit 1; }
    fi
fi
