# pump-NEEDS — changes outside client.rs / venc.rs that the frame source wants

Found while writing `client.rs` (upstream RTSP pull) and `venc.rs` (pump
socket). None of these were edited; each needs its owner.

## 1. `source::Hub` is latest-only and drops keyframes under bursts (important)

`Hub` keeps one frame. A waiter in `next(after_seq)` only sees whatever is
latest when it wakes, so two publishes in quick succession lose the first.
Upstream delivery is bursty: an IDR arrives as ~31 FU-A packets and the next
P-frame often sits right behind it in the same TCP read.

Measured end-to-end (ffmpeg `-f rtp` -> relay -> `client::pull`, 15 s, 25 fps):
the client published 375 AUs including 8 keyframes, seq contiguous (verified
through a test tap). A reader thread on `hub.next()` received 365 and
**missed all 8 keyframes**: each IDR was overwritten by the P-frame behind it
before the waiter woke. An RTSP session reading the Hub that way would never
get an IDR and could not decode.

Suggestion: a small ring (e.g. 16 frames or ~1 MiB, whichever comes first)
where `next(after_seq)` returns the *oldest* frame newer than `after_seq`, and a
reader that fell off the end resyncs at the newest keyframe. Same for audio.
Still Mutex/Condvar only.

## 2. `Hub::next` / `next_audio` can never return seq 0

Sequence numbers start at 0 and `next` wants `f.seq > after_seq` with a `u64`,
so a fresh reader cannot ask for frame 0. In practice that frame is the first
SPS+PPS+IDR. Either start seqs at 1 or take `Option<u64>`.

## 3. `media::sps_dimensions` reads one bit too many

```rust
let profile = b.bit()? * 0 + { /* reads 8 bits */ };
```
evaluates `b.bit()?` first, so 25 bits are consumed instead of 24 and every
later field is shifted. On the camera's own SPS
(`J03gKI1oBQBboQAAAwABAAADADIPFCKg`) it returns garbage, which `replay()` then
rejects, giving 0x0. It also does not strip emulation-prevention bytes.
`client.rs` carries its own parser (`dimensions()`, tested on 1280x720 camera
Main, 1920x1080 cropped High, 640x360 High 4:2:2, plus 2000 random inputs);
`media.rs` could adopt it.

## 4. `Hub.info` is fixed at creation

The client builds `StreamInfo` from the first session (SDP sprop, or in-band
SPS/PPS when the SDP has none). If a later session brings different parameter
sets (upstream restarted with a new resolution), the info cannot follow. The
frames themselves carry the in-band SPS/PPS before every IDR, so decoders cope,
but the SDP that camrtsp serves would go stale. Probably needs
`Hub::set_info()` or `info: Mutex<Arc<StreamInfo>>`.

## 5. `tools/remote-build.sh` fails on the first build from a new worktree

`rsync ... build-01:src/anyka-build/<hash>/camrtsp/` fails with
`mkdir ... failed: No such file or directory` because rsync only creates the
last path component. Worked around by hand (`ssh build-01 mkdir -p
~/src/anyka-build/<hash>`). Fix: `ssh build-01 "mkdir -p ~/$remote"` before
the rsync.

## 6. Pump wire format: integer byte order

The `venc.rs` header did not specify endianness. `venc.rs` now documents and
reads **little-endian** (the pump is a C program on the same ARM writing its
native layout). The pump implementation must match.

## 7. Wiring hint for main.rs (parent)

`client::pull(upstream, path)` wants `"host:port"` and `"/vs0"`. A spec like
`client:127.0.0.1:554/vs0` splits at the first `/`. Until the arm exists, the
non-test build shows dead-code warnings for `client.rs`.
