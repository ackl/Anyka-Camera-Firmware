# A pure-std Rust RTSP client for the Anyka stock server — what the wire actually does

Empirical notes from writing `tools/rust/camweb/src/rtsp.rs` against the live
camera (`rtsp://192.168.1.103:554/vs0`, `/vs1`) on 2026-10-25, host time
~13:20–13:45. Everything below was measured on the board's own `rtsp` process
(`ak_rtsp_demo`, deployed binary `fb375089…`, see `README.md`), not taken from a
spec.

Read-only: RTSP sessions only. No file was written to the camera, no motor or
audio command was sent, no reboot.

## Build & run

```sh
RUSTC=$HOME/.rustup/toolchains/1.99.1-x86_64-unknown-linux-gnu/bin/rustc

# unit + fake-server tests (no camera needed)
$RUSTC --test --edition 2021 -O tools/rust/camweb/src/rtsp.rs -o tools/rust/camweb/target/rtsp-tests
tools/rust/camweb/target/rtsp-tests

# probe harness
$RUSTC -O --edition 2021 tools/rust/camweb/src/probe.rs -o tools/rust/camweb/target/probe
tools/rust/camweb/target/probe rtsp://192.168.1.103:554/vs0 --secs 15 --out /tmp/vs0.h264
ffprobe -v error -f h264 -show_entries stream=width,height -of default=nw=1 /tmp/vs0.h264
```

`rustc` on `PATH` is the broken mise shim ("No version is set for shim: rustc") —
always use the 1.99.1 toolchain above. `rtsp.rs` needs no crates; it must be
pulled in as a module (`mod rtsp;` or `#[path=…] mod rtsp;`), not with
`include!`, because it carries one inner attribute (`#![allow(dead_code)]`).
Verified: a foreign crate root with `#[path=…] mod rtsp;` compiles with **no
warnings**, and the required API signatures typecheck exactly as specified.

## The handshake the camera accepts

Everything is optional except the request-line form. Measured tolerance:

| request | server answer |
|---|---|
| `OPTIONS <url> RTSP/1.0` | 200, `Public: SETUP,OPTIONS,DESCRIBE,PLAY,TEARDOWN,GET_PARAMETER,SET_PARAMETER` |
| `DESCRIBE` as the *first* request (no OPTIONS) | 200 + SDP — OPTIONS is not required |
| `DESCRIBE` with no `Accept:` | 200 + SDP — the header is not required |
| `DESCRIBE` with no `CSeq:` | 200 + SDP — not required either |
| `DESCRIBE <url>` without the `RTSP/1.0` version | **connection reset** |
| `OPTIONS * RTSP/1.0` (asterisk form) | **connection reset** — use the full URL |
| `GET_PARAMETER <url>` | 200 — works as an alternative keepalive |
| `DESCRIBE rtsp://…/nope` | `404 Not Found` + `Server: nginx/1.17.8` |
| `SETUP …/camera` with `Transport: RTP/AVP;unicast;client_port=…` (UDP) | `404 Not Found` + `Server: nginx/1.17.8` |

Two things fall out of that table:

* **There is an nginx in front.** 200-class answers carry no `Server` header at
  all (they come from `ak_rtsp_demo`); the 404s carry `Server: nginx/1.17.8`.
  nginx only routes the requests it knows, which is why the UDP SETUP form is
  *not* a "400 unsupported transport" but a flat 404. **TCP-interleaved SETUP is
  the only usable transport**, which is what we want anyway.
* No authentication exists: no `WWW-Authenticate`, no 401 anywhere, on any
  method. (Nothing to send, nothing to leak.)

The exchange that works, in order:

```
C: OPTIONS  rtsp://H:554/vs0                                 RTSP/1.0  CSeq: 1
S: 200  Public: SETUP,OPTIONS,DESCRIBE,PLAY,TEARDOWN,GET_PARAMETER,SET_PARAMETER

C: DESCRIBE rtsp://H:554/vs0   Accept: application/sdp       CSeq: 2
S: 200  Content-Type: application/sdp
        Content-Base: rtsp://H:554/vs0/          <- note: base, ends in '/'
        Content-Length: 345..346
        <SDP>

C: SETUP    rtsp://H:554/vs0/camera                          CSeq: 3
            Transport: RTP/AVP/TCP;unicast;interleaved=0-1
S: 200  Transport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=9312188
        Session: 00000007; timeout=60            <- id AND timeout in one value

C: SETUP    rtsp://H:554/vs0/micphone                         CSeq: 4
            Transport: RTP/AVP/TCP;unicast;interleaved=2-3
            Session: 00000007
S: 200  Transport: RTP/AVP/TCP;unicast;interleaved=2-3;ssrc=9312236
        Session: 00000007; timeout=60            <- same id, per connection

C: PLAY     rtsp://H:554/vs0/   Session: 00000007  Range: npt=0.000-   CSeq: 5
S: 200  Range: npt=0.000-   Session: 00000007; timeout=60
        == then '$' interleaved frames, immediately ==

C: TEARDOWN rtsp://H:554/vs0/   Session: 00000007                      
S: 200
```

Full handshake (connect + 5 requests) took **183 / 193 / 405 ms** on three runs
from this host over tailscale; the first byte of the stream arrives within a few
ms of the `PLAY` reply.

### Session and transport quirks

* **`Session:` value is `"<id>; timeout=60"`.** The id has to be split off at
  `';'` before it is echoed back. The server tolerates the whole value, but
  sending only the id is correct.
* **Session ids are per TCP connection and just count up**: `00000005`,
  `00000006`, `00000007`, … `00000013` over my runs (8 hex digits). A second
  SETUP carrying a *deliberately wrong* Session id was still answered with the
  connection's own id — the header is effectively ignored, one session per
  socket.
* **Interleaved channel numbers are chosen by the client and honoured
  verbatim.** `interleaved=2-3` for audio was accepted even in a session where
  video was never set up (audio then lives on 2/3, nothing on 0/1). Do not
  assume channel 2 = audio; take the numbers from the SETUP reply.
* **A trailing `/` on the request URL doubles the slash in `Content-Base`**
  (`DESCRIBE …/vs0/` → `Content-Base: …/vs0//`). Harmless, but it is why the
  module builds control URLs from the URL it was given instead of from
  `Content-Base`.
* The SDP `o=` line names **192.168.1.79** while `c=` says 192.168.1.103: the
  origin address is a stale/compiled-in artefact (the board has had other DHCP
  leases). Never use either SDP address; use the URL's host.
* `i=luo_zhanzhao` is a developer credit in the SDP, not a device name. Nothing
  to do with this bubble's identity.

## SDP as served (both channels, byte-identical apart from the SPS)

```
v=0
o=- 11 1 IN IP4 192.168.1.79
s=/vs0
i=luo_zhanzhao
c=IN IP4 192.168.1.103
t=0 0
a=control:*
a=range:npt=0-
m=video 0 RTP/AVP 96
a=rtpmap:96 H264/90000
a=fmtp:96 sprop-parameter-sets=J03gKI1oBQBboQAAAwABAAADADIPFCKg,KO4C0kg=
a=recvonly
a=control:camera
m=audio 0 RTP/AVP 8
a=rtpmap:8 PCMA/8000/1
a=recvonly
a=control:micphone
```

* Control names are relative: video `camera`, audio `micphone`, base `*`.
* Video payload type 96, clock 90000. Audio payload type 8, `PCMA/8000/1` — note
  the extra `/1` channel count, which is not standard for audio but parses fine.
* No framerate is declared, and `m=video 0` + `c=IN IP4 <host>` (port 0) is
  normal for interleaved transport.
* `sprop-parameter-sets` is present: SPS 24 B / PPS 5 B on `/vs0`, SPS 23 B /
  PPS 5 B on `/vs1` (both start with NAL type 7 / type 8).

## Wire framing

Interleaved framing is exactly `'$' (0x24) | channel u8 | length u16 BE | payload`.
Payload length is capped at 65535, so a NAL bigger than that has to arrive
through FU-A — and it does.

| channel | content | observed |
|---|---|---|
| 0 | video RTP | ~15–20 packets/s… ish (see rates) |
| 1 | RTCP | sparse: 2 packets per 6–8 s, first is an SR (`81 ca 00 09 …`) |
| 2 | audio RTP | 25/s, PCMA, 320-byte payloads |
| 3 | RTCP (audio) | not observed in the samples |

**RTCP starts with `0x81` = RTP version 2.** A client that parses "any packet on
the socket" as RTP will happily feed an SR into the H.264 depacketiser (payload
type 9 payloads look like AUD-ish garbage NALs). Demultiplex on the channel
number first; that is what `rtsp.rs` does.

RTP packet shape: version 2, no padding, no extension, no CSRC, payload type 96,
marker bit set on the last packet of an access unit. Bulk video packets are
**1258-byte interleaved frames** (12-byte header + 1246-byte payload); the tails
of fragments are short (17, 36, 418, 425, 451 B…).

Audio: `PCMA`, 8000 Hz, mono, 320 B per packet, RTP timestamps step by 320
samples = **40 ms per packet**.

## H.264 as produced

Observed NAL types across the captures: **1, 5, 7, 8, 12**. Nothing else — no
STAP-A (24), no AUD (9), no FU-B (29). Fragmented slices use FU-A (28).

Consequences worth knowing:

* **There is no AUD**, so an access unit cannot be found by NAL type. Count
  frames with the **RTP marker bit** (`nal_with_marker()` returns it). For this
  encoder AU == final NAL.
* **SPS and PPS are repeated in-band before *every* IDR**: 35/35 and 36/36 IDRs
  in the two 120 s captures were immediately preceded by SPS, PPS. A client that
  starts mid-stream can always decode within one GOP, and the very first NALs of
  a fresh session are already `SPS, PPS, IDR` (measured: first IDR *start* 70 /
  108 / 123 ms after `PLAY`; first complete AU 0.25 / 0.50 / 0.93 s).
* GOP ≈ **52 access units ≈ 3.3–3.5 s** at the measured rate.
* Sizes: IDR NAL 34–45 KB, P-slice 254 B – 21 KB, average NAL 2.4–2.9 KB on
  `/vs0`; filler NALs (type 12) are 3–12 B and sporadic.

## Measured rates

`probe --secs 120 --keepalive 20` on `/vs0`, two independent runs plus 15 s runs:

| channel | wall | NALs | NAL/s | AUs (frames) | AU/s | bytes/s | bitrate |
|---|---:|---:|---:|---:|---:|---:|---:|
| /vs0 120 s (run A) | 120.03 s | 1835 | 15.29 | 1763 | 14.69 | 59,335 | 475 kbit/s |
| /vs0 120 s (run B) | 120.03 s | 1807 | 15.06 | 1737 | 14.47 | 35,479 | 284 kbit/s |
| /vs0 130 s, **no keepalive** | 130.04 s | 1949 | 14.99 | 1873 | 14.40 | 43,300 | 346 kbit/s |
| /vs0 15 s (typical) | 15.09 s | 241 | 16.02 | 231 | 15.35 | 47,098 | 377 kbit/s |
| /vs1 15 s (typical) | 15.04 s | 259 | 17.22 | 249 | 16.55 | 7,102 | 57 kbit/s |
| /vs1 15 s (earlier) | 15.04 s | 243 | 16.16 | 233 | 15.49 | 11,582 | 93 kbit/s |

* **Frame rate is ~15 fps, not the 25 the encoder is configured for** — matching
  `analysis/video-pipeline.md` (ISP `set sensor fps: 15`). Across all runs the AU
  rate ranged **14.2 – 18.7 AU/s**; short runs skew high (a fresh session starts
  on a keyframe burst), the 120 s runs settle at 14.4–14.7. Bitrate varies 4×
  with scene content (284–751 kbit/s on the same channel) — quote the frame rate,
  not the bitrate.
* `/vs1` is 640×480 and roughly 5–8× cheaper in bytes, same frame rate.
* ffprobe decoded 229/231, 232/233, 247/249 and 229/231 AUs across captures
  (≥99 %), with **zero** decoder errors: the depacketiser is not dropping or
  corrupting data.
* Longest inter-NAL gap in a healthy 120 s run: 0.41–0.62 s (one slow frame);
  `>1 s` gaps: none.

## Session timeout behaviour — advertised 60 s, *not enforced*

* SETUP replies advertise `Session: <id>; timeout=60`, i.e. **60 s**.
* **Observed: the timeout is not enforced while streaming.** A 130 s run with
  *zero* RTSP requests after `PLAY` kept receiving NALs at full rate to the end
  (1949 NALs, last AU at t≈130 s, no error, connection still open). So the
  advertised timeout is decorative on this build.
* With a keepalive every 20 s the session is also fine for 120 s (5 keepalives,
  0 errors). There is nothing to distinguish — both survive — so keepalives are
  *cheap insurance*, not a requirement: other RTSP servers do enforce their
  timeout, and this one may start enforcing under conditions I did not create.
* `keepalive()` also works with `GET_PARAMETER` on this server (200), but
  `OPTIONS` is what the API uses and it is answered the same way.

## Keepalive: a mid-stream OPTIONS is sometimes never answered

This cost me the only real bug in the end-to-end path, so it is worth recording
precisely.

* Mid-stream `OPTIONS` normally comes back in **37 ms average, 54 ms max** (5
  keepalives in one 120 s run) — or **91 ms** for a hand-rolled python probe.
* In the *first* 120 s run, one of 5 keepalives got **no reply at all** (0 of 5 in the next run, so ~1 in 10 overall — rare, but it happens). The
  client was waiting for the matching `CSeq` with a 3 s deadline, so the video
  path stalled for exactly `max_gap = 3.000 s` — the deadline, to the
  millisecond. The stream itself was healthy the whole time.
* Fix in `rtsp.rs`: the keepalive waits at most `KEEPALIVE_WAIT = 400 ms` for its
  reply, and if the reply is late or lost it just returns `Ok(())` (counting a
  miss). The unclaimed reply, whenever it shows up, is swallowed by `nal()` as
  stray `RTSP/1.0 …` text between `'$'` frames. After that change: 5/5
  keepalives, `max=54 ms`, `max_gap=0.615 s`, no stalls, over a second 120 s run.

Design rule that follows: **the RTSP control plane may never block the video
plane.** Text replies and `'$'` frames are demultiplexed on the same byte
stream, and leftover text is discarded by `nal()`.

## Cost

* CPU: **0.1 % of one core** — 10–20 ms of CPU per 15 s of `/vs0`, 180 ms per
  120 s (`utime+stime` from `/proc/self/stat`). `/vs1` is immeasurable at this
  resolution.
* Memory: RSS **2.7–2.9 MB** and flat over 120 s (2552 → 2856 KB across the run,
  plateauing); file descriptors constant at 5. No unbounded growth: the inbound
  buffer only ever holds an incomplete frame, and a single NAL is capped at
  2 MiB.

## Verification

* `/vs0` 15 s → `ffprobe -f h264` = **1280x720**, 229 frames decoded, 0 errors.
* `/vs1` 15 s → **640x480**, 247 frames decoded, 0 errors.
* `--media video,audio` (both tracks set up, audio on channels 2/3) → identical
  video NAL stream, 1280x720, audio frames silently skipped.
* `--media audio` (video never set up) → `tracks[0].interleaved = None`, zero
  NALs, `nal()` returns `Ok(None)` about once a second (the internal skip
  counter after 32 audio frames), **no error and no hang**.

Scratch captures used for the numbers above live in `/tmp` (`/tmp/probe-vs0.h264`,
`/tmp/pr/long2.h264`, …) and are deliberately **not** committed: the SD card and
`backups/` hold the real artefacts, and ffprobe reproduces the verdict in one
command.

## Pitfalls that cost time (each one produced a plausible-looking wrong answer)

1. **The NAL header is at offset 4 of an Annex-B blob, not 3.** My first probe
   read `nal[3]` and reported "236 NALs, all type 1, no IDR, no in-band SPS/PPS"
   — a perfectly believable histogram of a broken depacketiser. Index 4 is right;
   index 3 is the last byte of `00 00 00 01`.
2. **RTCP looks exactly like RTP** (`0x81` = v2). Demultiplex by channel first.
3. **`Session:` carries `; timeout=60`** — splitting at `';'` is not optional if
   you echo the value back anywhere that parses it.
4. **`Content-Base` from a trailing-slash URL has a doubled slash.**
5. **Frame counting needs the marker bit**: no AUD NALs exist in this stream.
6. **A malformed CSRC/extension claim must be rejected, not trusted**: an RTP
   packet whose extension length field says 772 words with 31 bytes on the wire
   is what my own fake-server test produced by accident, and `rtp_split` correctly
   returned `None` instead of panicking. Any length that comes off the wire needs
   a bounds check before it is used as a slice.
7. `df /mnt`-class traps do not apply here, but the general rule does: trust the
   measured AU rate over the declared 25 fps.

## Using it from `src/main.rs` (the web panel)

* One `Session` per stream URL; put it in the video thread (it is `Send`, not
  `Sync`). Poll `nal()` in a loop, call `keepalive()` every 20 s from that same
  thread.
* Set `io_timeout` to 1–1.5 s. `Ok(None)` is the normal "nothing yet" answer and
  must not be treated as an error; only `Err` means the socket is gone.
* **`session_id()` and `tracks()` return borrows of the session.** Clone what you
  need (`s.session_id().to_string()`) before calling `nal()`/`keepalive()`, which
  take `&mut self`, otherwise the borrow checker stops you — as it should.
* First pixels arrive fast (~0.1 s to the first IDR start, <1 s to a complete
  AU), and SPS/PPS are both in the SDP and in-band, so a mid-stream join never
  needs a "wait for the next keyframe" delay.
* Watchdog: if no AU (no marker-carrying NAL) arrives for ~10 s, tear the session
  down and reconnect. Do not rely on an error being raised — an idle session is
  silent, not closed (proven above for 130 s).
* The camera tolerates a handful of client attach/detach cycles (I opened ~20
  sessions today, sequentially, plus RTCP churn) — but `analysis/video-pipeline.md`
  records one unexplained network loss under client churn, so keep **one
  session per stream** and cache frames for multiple browser viewers rather than
  opening a session per viewer.
