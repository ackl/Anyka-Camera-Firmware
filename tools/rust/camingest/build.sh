#!/bin/sh
# Build camingest and run its tests. Host-only: this crate is the *receiver*,
# which lives on x86_64 (storage-01), so there is no ARM target to chase here —
# unlike motorctl/ispconf/camrtsp, nothing in this crate runs on the camera.
#
#   ./build.sh            # binary + tests
#   SKIP_TESTS=1 ./build.sh
#
# The binary is what an operator runs:
#   ./camingest serve --root /srv/storage/cam --prefix /ingest --token "$TOKEN"
set -eu
cd "$(dirname "$0")"

RUSTC=${RUSTC:-$HOME/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin/rustc}
[ -x "$RUSTC" ] || { echo "rustc not found at $RUSTC (set RUSTC=...)" >&2; exit 1; }
mkdir -p target

# Release profile: opt-level 2, LTO, one CGU, stripped. panic=unwind is
# deliberate — the connection handler is wrapped in catch_unwind so a bug in one
# request logs a line instead of killing the archive's only server.
"$RUSTC" --edition 2021 -C opt-level=2 -C lto=fat -C codegen-units=1 \
    src/main.rs -o target/camingest
printf 'host: %s bytes\n' "$(ls -l target/camingest | awk '{print $5}')"

if [ "${SKIP_TESTS:-0}" != "1" ]; then
    # opt-level=1 keeps the compile under a second; the tests are socket-bound.
    "$RUSTC" --test --edition 2021 -C opt-level=1 src/main.rs -o target/camingest-tests
    ./target/camingest-tests --quiet
fi
