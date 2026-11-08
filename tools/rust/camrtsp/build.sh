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

# TLS is mbedTLS 2.28 + a tiny shim, cross-built by tools/tls/build.sh (zig cc,
# arm-linux-musleabi, -mcpu=arm926ej_s). It cannot be a Rust dependency: there
# is no TLS stack in std and no cargo for this crate. Override with TLS_ROOT
# when building somewhere the sibling tools/ tree is not present (the remote
# builder passes its own path).
TLS_ROOT=${TLS_ROOT:-$PWD/../../../tls/out}
for arch in host armv5te; do
    [ -f "$TLS_ROOT/$arch/libcamtls.a" ] || {
        echo "missing $TLS_ROOT/$arch/libcamtls.a — run tools/tls/build.sh all" >&2
        exit 1
    }
done
TLS_HOST="-L native=$TLS_ROOT/host -l static=camtls"
TLS_ARM="-L native=$TLS_ROOT/armv5te -l static=camtls"

# host build (fast, for tests and protocol work)
# shellcheck disable=SC2086
"$RUSTC" $PROFILE $TLS_HOST --edition 2021 src/main.rs -o target/camrtsp
printf 'host: %s bytes\n' "$(ls -l target/camrtsp | awk '{print $5}')"

# unit tests on the host
if [ "${SKIP_TESTS:-0}" != "1" ]; then
    "$RUSTC" --test --edition 2021 $TLS_HOST src/main.rs -o target/camrtsp-tests
    ./target/camrtsp-tests --quiet
fi

# ARMv5TE build, optional (needs the target's std)
if [ -d "$SYSROOT/lib/rustlib/armv5te-unknown-linux-musleabi" ]; then
    # shellcheck disable=SC2086
    "$RUSTC" --target armv5te-unknown-linux-musleabi \
        -C linker="$LLD" -C link-self-contained=yes $PROFILE $TLS_ARM \
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
