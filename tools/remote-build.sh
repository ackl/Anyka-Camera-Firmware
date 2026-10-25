#!/bin/sh
# Build a Rust crate from tools/rust/<name> on build-01, never on firefly.
#
# Firefly is RAM-bound and shared with the user; cargo/rustc builds there have
# cost whole afternoons of swap. build-01 is the same rustc 1.99.1 + the same
# armv5te-unknown-linux-musleabi std, so the ARM binary is bit-identical.
#
#   tools/remote-build.sh camrtsp            # build + unit tests there
#   tools/remote-build.sh camrtsp SKIP_TESTS=1
#
# Artifacts land back in tools/rust/<name>/target/.
set -eu
crate=${1:?usage: remote-build.sh CRATE [VAR=value ...]}
shift || true
here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
[ -d "$root/tools/rust/$crate" ] || { echo "remote-build: no crate tools/rust/$crate" >&2; exit 1; }

# Cheap reachability check: fail fast instead of a 60 s ssh hang.
timeout 10 ssh -o BatchMode=yes -o ConnectTimeout=8 build-01 true 2>/dev/null \
    || { echo "remote-build: build-01 unreachable (ssh build-01)" >&2; exit 1; }

# Remote path is keyed by the LOCAL absolute path, not just the crate name, so
# two agents building the same crate from different git worktrees at the same
# time cannot overwrite each other's sources or artifacts.
remote="src/anyka-build/$(printf '%s' "$root/tools/rust/$crate" | sha256sum | cut -c1-16)/$crate"
ssh -o BatchMode=yes build-01 "mkdir -p ~/$remote"

rsync -a --delete --exclude target \
    "$root/tools/rust/$crate/" "build-01:$remote/"
# shellcheck disable=SC2068
ssh -o BatchMode=yes build-01 "cd ~/$remote && $* ./build.sh"
rsync -a "build-01:$remote/target/" "$root/tools/rust/$crate/target/"
