#!/bin/sh
# Build camweb: the host-side panel (firefly only, no ARM target).
#
# camweb speaks RTSP itself and shells out to ffmpeg for H.264 -> JPEG only, so
# there is no cargo dependency and nothing to fetch: one rustc invocation, pure
# std. The `rustc` on PATH is a mise shim with no version pinned here, which is
# why the toolchain is referenced directly (override with RUSTC=...).
set -eu
cd "$(dirname "$0")"

RUSTC=${RUSTC:-$HOME/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin/rustc}
[ -x "$RUSTC" ] || { echo "rustc not found at $RUSTC (set RUSTC=...)" >&2; exit 1; }

command -v ffmpeg >/dev/null 2>&1 || {
    echo "ffmpeg not in PATH: camweb needs it to decode the H.264 it pulls" >&2
}

mkdir -p target
"$RUSTC" -O --edition 2021 src/main.rs -o target/camweb

# Optional: the RTSP client's own harness (SDP, NAL/AU rates, keepalive timing).
"$RUSTC" -O --edition 2021 src/probe.rs -o target/probe 2>/dev/null \
    && printf 'probe: %s bytes\n' "$(ls -l target/probe | awk '{print $5}')"

printf '\n%s\n' "target/camweb"
ls -l target/camweb
sha256sum target/camweb
