# Bounded motor controller — 2026-10-15

`motorctl.c` targets the exact saved stock Anyka kernel API, not the repo SDK.
Build (Mac ARM):

    ./zig-macos-aarch64-0.13.0/zig cc -target arm-linux-musleabi -mcpu=arm926ej_s -static -Os -Wall -Wextra motorctl.c -o motorctl

Deployed on camera SD: `/mnt/Factory/custom/motorctl`.
No arguments: prints usage, opens no motor and moves nothing.

    /mnt/Factory/custom/motorctl AXIS DIRECTION PHASE_STEPS SPEED

Limits: axis/direction 0 or 1, steps 1–32, speed 1–10. No homing or saved-position writes. Checks existing motor owners, takes cooperative `/tmp` lock, runs bounded relative movement and waits for exact stock stop-event bit 4. Handles stock zero-return event reads. Interrupt/timeout requests asynchronous stop. No hardware fail-safe; remove power if motion persists. Ownership check is not an atomic kernel lock, so do not launch another PTZ application concurrently.

Live changes:
- Actual SD startup preserved as `/mnt/Factory/config.before-motorctl.sh`.
- `/mnt/Factory/config.sh` now comments out old `ptz_daemon` auto-launch; all other lines retained, including custom port-2323 Telnet.
- Old daemon PID458 terminated; not restarted. No reboot or flashing.
- Controller stored on SD; NOT auto-started.

Live tests:
- ARM static binary hash on Mac and camera matched: `58e615dda876633ae6b180c55967ec8ac1bf9dd0eeb95cda78518c9aac72f7ca`.
- No-argument idle launch passed.
- `motorctl 0 0 16 5` and `motorctl 1 0 16 5`: both exit 0; stop event flags=4, remaining=0.
- These confirm driver-reported normal completion, NOT visually observed shaft movement or electrical coil-release measurements. User must confirm physical movement/axis mapping.
- Interrupted/timeout stop path has not been exercised live.
- Existing web PTZ controls target old daemon and will not control this CLI.

User confirmed motors are plugged in but axles mechanically unloaded before authorizing tests. Stock flash backups unchanged. Repo startup unmodified; deployed SD startup changed as above. Existing internal ptz.ini not reset.

## Rust port (2026-10-24)

`rust/motorctl/` is a faithful Rust port and is now the deployed
`/mnt/Factory/custom/motorctl`. The C build stays on the camera as
`motorctl.c-binary` for comparison. Build with `rust/motorctl/build.sh`, target
`armv5te-unknown-linux-musleabi` (ARM926EJ-S is v5TEJ; Rust's
`arm-unknown-linux-musleabi` is ARMv6 and SIGILLs on this SoC). Parity tests,
including the first live exercise of the interrupt/stop path, are in
`rust/motorctl/README.md`.

## Video — working as of 2026-10-24

    rtsp://192.168.1.103:554/vs0   1280x720 H.264 ~14.3 fps + 8 kHz a-law
    rtsp://192.168.1.103:554/vs1    640x480 H.264 ~13.8 fps + 8 kHz a-law

Two host wrappers do the common things so nobody retypes ffmpeg flags:

    tools/snap [OUT.jpg] [CHANNEL]        # one still (default /vs0)
    tools/rec SECONDS [OUT.mp4] [CHANNEL] # clip, video-only stream copy

Video-only is deliberate: mp4 refuses the second track (`pcm_alaw`) with
"Could not find tag for codec pcm_alaw", which is how the first attempts made
0-byte files. Use a `.mkv` and `-map 0:a -c:a copy` if the sound matters. These
are wrapped around:

    ffprobe -rtsp_transport tcp rtsp://192.168.1.103:554/vs0
    ffmpeg  -rtsp_transport tcp -i rtsp://192.168.1.103:554/vs0 -frames:v 1 out.jpg

The cause was `/etc/jffs2/styleId` being read as one raw byte: the text `48` put
`0x34` ('4') = 52 against a config whose style byte is 0. The file is now gone
(stock shipped none) and `config.sh`'s existing `rtsp &` line does the rest.
Full story, format and remaining rough edges: `../analysis/video-pipeline.md`.

### ispconf (Rust, 2026-10-24)

`rust/ispconf/` inspects and validates the `DV5_*` ISP config container and
reimplements `libplat_vi.so`'s own header checks. Host build for analysis, plus
an ARMv5TE build deployed at `/mnt/Factory/custom/ispconf`
(401,416 bytes, sha256 `7f4d752cf1673c18853e9f1ce642c7145cd177f859c3cbbe36f09e9a0c50e9f2`).

    ispconf inspect FILE
    ispconf check   FILE [--sensor 0x1346] [--style 0]
    ispconf extract FILE --index 0
    ispconf set-style FILE --index 0 --style 0 [--out F | --in-place]

On the camera, check against the live sensor:

    /mnt/Factory/custom/ispconf check /mnt/Factory/custom/sensor/isp_sc1346.conf \
        --sensor "$(cat /sys/ak_info_dump/sensor_id)"

### irnight — day/night, ISP included (2026-10-25)

The normal way to flip night mode. Nothing is driven from the host: the value
lands in `/tmp/photo`, and the stock `rtsp` process reads it from its own 5 s
photosensitive thread and switches the ircut, the IR illuminator **and the ISP
scene** in-process, so the night frame is a proper neutral monochrome image
instead of the violet mess the raw GPIO path produces.

    tools/irnight night      # IR on, filter out, ISP night  (~10 fps)
    tools/irnight day        # IR off, filter in, ISP day    (~14 fps)
    tools/irnight            # status: photo value, pins, rtsp, last switch

Takes effect within 5 s and the stream keeps running through the switch. The
value is volatile — `/tmp/photo` does not survive a reboot, so the board always
comes up in day mode; add `echo 1 > /tmp/photo` to `config.sh` if a night boot is
ever wanted. This only works because two SD-card binaries were patched
(`libplat_drv.so`, `rtsp`) to look for the sysfs names this firmware actually
creates; `work/irpatch.py --check` verifies them, `.stock` copies sit beside them
on the card, and `../analysis/ir-daynight.md` has the offsets and evidence.

### irctl (Rust, 2026-10-24) — pin-level fallback

Driveable hardware, no ISP involvement: `/sys/user-gpio/{ircut_a,ircut_b,IR_LED}`
via `rust/irctl/`, deployed at `/mnt/Factory/custom/irctl` (394,200 bytes,
ARMv5TE). Use this to poke pins by hand; use `irnight` for actually seeing in the
dark. Do not run both at once — they move the same filter.

    tools/cam '/mnt/Factory/custom/irctl night'    # filter out + illuminator on
    tools/cam '/mnt/Factory/custom/irctl day'      # illuminator off + filter in
    tools/cam '/mnt/Factory/custom/irctl status'   # raw GPIO values
    tools/cam '/mnt/Factory/custom/irctl ir on|off'      # illuminator only
    tools/cam '/mnt/Factory/custom/irctl filter in|out'  # filter only

The filter is a latching solenoid: the tool pulses a coil for 300 ms and the
filter stays put, so its position cannot be read back — `irctl` remembers the
last mode in `/tmp/irctl.mode`. Used alone it leaves the ISP in its **day**
configuration, so the night frame is violet and blown out (RGB mean 165 161 242).
That is what `irnight` fixes (mean 116 116 116). Full detail:
`../analysis/ir-daynight.md`.

## Audio — working as of 2026-10-24

The board's speaker is driven by the stock `ak_adec_demo` CLI at
`/usr/bin/ak_adec_demo`. It opens the DAC directly, plays until the file ends,
then exits cleanly (rc=0). Usage:

    ak_adec_demo [sample rate] [channel num] [type] [audio file path]
    ak_adec_demo 16000 2 pcm /mnt/Factory/custom/audio/chime.pcm

Supported types: `mp3 amr aac g711a g711u pcm`. Raw PCM is easiest to generate
on the host and needs no container or header; AMR is 8000 Hz mono only. The demo
writes ANSI-heavy logs to stdout/stderr, so redirect them when driving it from
the flaky telnet channel.

**Boot chime.** `config.sh` used to call
`ak_adec_demo 16000 2 aac /usr/share/audio_file/common/didi.aac`, but neither
`/usr/share/audio_file/` nor `/data/audio_file/common/` exists on this board
(the real AACs live in `/data/audio_file/language/`), so that call failed
instantly and silently — there has never been a boot sound. It is now replaced
with a short two-tone chime from `/mnt/Factory/custom/audio/chime.pcm`,
backgrounded so a hung decoder can never delay rtsp. The chime plays within a
second of `config.sh` running, i.e. seconds after power-on. The pre-change config
is preserved as `/mnt/Factory/config.before-beep.sh`.

**The amp keeper: `quiet-cue.sh`.** Every audible play goes through
`/mnt/Factory/custom/quiet-cue.sh FILE [RATE] [CHANNELS]` (repo copy
`work/quiet-cue.sh`). The AK3918 driver asserts `SPK_PA=1` while it plays and the
amp is active-low, so the script holds the pin at 0 for the whole play and
restores 1 afterwards (on success, on decoder failure, and on INT/TERM/HUP). The
keeper is sized from the file itself — `SECS = ceil(bytes / (RATE*CH*2)) + 2`,
capped at 120 s, with a wall-clock ceiling — instead of the old flat 200-iteration
cap that dropped the amp after ~10 s. Progress lands in `/tmp/spk.ticks`.

It writes **no LED**: an LED cue that stayed on for the play turned a 62 s silent
test into a minute of white lamp on the user's desk, so the `led` lines were
removed (not guarded) on 2026-10-25. The card's `led` binary is
`/mnt/Factory/custom/led.frozen` — do not restore it, and never write a lamp pin
from an audio helper. Details and the measurement table: `../analysis/audio.md`.

**On-demand chirp.** `/mnt/Factory/custom/beep` plays a 0.6 s 880 Hz PCM tone and
prints `beep rc=0` when done. From firefly:

    tools/beep                    # or: tools/cam '/mnt/Factory/custom/beep'

Files on the card:

    /mnt/Factory/custom/audio/chime.pcm   two-tone 880->1320 Hz, ~0.63 s, 16 kHz 2ch s16le
    /mnt/Factory/custom/audio/beep.pcm    880 Hz, 0.6 s, 16 kHz 2ch s16le
    /mnt/Factory/custom/beep              shell wrapper (chmod +x)
    /mnt/Factory/config.before-beep.sh    pre-chime config backup

**Speaker replaced 2026-10-25.** The user swapped the physical speaker, so every
level verdict above (−40 comfortable / −30 loud / −20 emergency, all made by ear
on 2026-10-24) is void until re-judged on the new part. Use the web panel below
or `work/leveltest.sh` to re-rank before choosing a boot-chime level.

Playback does not disturb rtsp: the video stream stayed live (PID 625, 1280x720)
before and after each beep, and the demo leaves no process behind. The DAC and
the rtsp ADC are separate device nodes (`/dev/akpcm_cdev0` vs `cdev1`), so the
beep does not break the running AEC. The demo's default ASLC volume is 6 and is
not exposed as an argument; the tone is full-scale PCM. The DAC's actual sample
rate is ~15943 Hz for a 16000 Hz request, so tones are ~0.35% flat. Full detail,
log lines and caveats: `../analysis/audio.md`.

### Web speaker test panel (2026-10-25)

**Audio is frozen as of 2026-10-25 ~13:20 host — user instruction: "stop fucking
with the sound. it's super annoying."** The panel is now the *user's* tool: they
press the buttons, we do not push more tones, re-run level ladders or touch the
boot chime unless they ask. Everything below is how the panel works, not a
backlog. Board state at the freeze: `SPK_PA=1` (muted), `IR_LED=0`,
`WHITE_LED=0`, `ircut_a=1`, `/tmp/photo=0` (day), `rtsp` up, nothing playing.

Manual speaker testing from a browser, so the user can judge levels without
waiting for a script push. Deployed CGI:
`/mnt/Factory/custom/web_interface/www/cgi-bin/webui` (shell script, 9363 B,
sha256 `00b6d66f…c69aa5`; repo copy `work/webui.new`; the stock 2944 B page is
kept beside it as `webui.stock`).

    http://192.168.1.103:8080/          # index.html redirects to the panel

Buttons: a level ladder (3 × 0.15 s clicks at −40/−30/−20/−10 dBFS), a 3 s
continuous tone (−20), the cue/warn/alarm set, `boot40`/`boot32` (the boot chime
files), Stop, Refresh status, and a stock-decoder diagnostic. The status table
shows `SPK_PA`, keeper ticks, `/tmp/photo`, rtsp count and the last six plays.

How it works: every audible button calls `quiet-cue.sh`, i.e. the same amp-holding
path as the boot chime — so what you hear here is what boot will sound like.
Plays are backgrounded (the decoder needs 1–3 s to open the device) and land in
`/tmp/web-spk.log`, which the page tails. Names are whitelisted by a `case` list;
a path is never built from the query string — `spk:../../config.sh` is refused.

Two findings while building it (both now handled in the CGI):

* The stock decoder path (`ak_adec_demo` with no amp hold) leaves `SPK_PA` at
  **0** after it exits — the driver's teardown enables the pin. So a diagnostic
  play leaves the amp powered and idle, and the page now labels that state
  (`amp left enabled, press Stop to mute`) instead of just showing `idle`. It
  also re-mutes at the end of the play.
* The stock PTZ buttons were removed from the page: they wrote directions to
  `/tmp/ptz.daemon`, whose daemon is disabled, so they never moved anything.
  Motor control stays on the command line (`motorctl`).

## Live panel — video, PTZ, LEDs (Rust, 2026-10-25)

`tools/camweb serve` puts the camera in a browser: live video, PTZ arrows, the two
LEDs, day/night, a snapshot button and a live read-out of the board's GPIO pins.

    tools/rust/camweb/build.sh          # -> target/camweb (host build, no ARM)
    tools/camweb serve                  # http://<firefly>:8090/
    tools/camweb probe                  # SDP/tracks/throughput of the stream
    tools/camweb snap [OUT.jpg] [--ch vs1]     # one frame, ~2 s
    tools/camweb rec SECONDS [OUT.mp4] [--ch vs1]   # stream copy, no re-encode
    tools/camweb sh 'board command'     # same job as tools/cam, in Rust

**Why the panel lives on firefly, not on the camera.** The board is a single
ARM926 with 36 MB of RAM, has no JPEG path of its own, and is only reachable over
tailscale — and no browser can play RTSP. So firefly pulls the stream and re-serves
it as MJPEG; the board is left doing what it is good at (capturing). Nothing in
this feature writes to the camera's filesystem: no pushes, no SD changes, no flash.

**Architecture** (`tools/rust/camweb/src/`):

* `rtsp.rs` — pure-std RTSP/RTP-over-TCP client (51.6 KB, 8/8 unit tests incl. a
  fake server that answers with garbage bytes, RTSP text mid-stream, RTCP, torn
  writes). It does OPTIONS/DESCRIBE/SETUP/PLAY itself, strips the interleaved
  framing (`$`, channel, u16 length), parses RTP and depacketizes H.264 (single
  NAL, STAP-A, FU-A), and pulls SPS/PPS out of the SDP `sprop-parameter-sets` so
  the decoder can start mid-GOP. No crates: the base64 decode and byte parsing are
  hand-rolled, because nothing is vendored for this project. It has an inner
  `#![allow(dead_code)]`, so it must be pulled in with `mod rtsp;` (`include!`
  breaks). All protocol quirks — nginx in front, TCP-interleaved `SETUP` only,
  no auth, unenforced `timeout=60`, no AUD so frames are counted by the RTP marker
  bit — are in `work/rtsp-rust-notes.md` and summarised in
  `analysis/video-pipeline.md`.
* `telnet.rs` — the board shell, a Rust port of `tools/cam`'s protocol (IAC
  negotiation, `printf` marker carrying `$?`). It turns the pty echo off with
  `stty -echo` first, otherwise half of every reply is the echoed command; the
  marker is sent as two adjacent strings so it never appears whole in that echo.
* `main.rs` — HTTP server, JPEG fan-out and the endpoints below.
* `panel.html` — the page (dark, responsive, no build step, no external assets).

ffmpeg is used **only as the decoder**: the Rust client writes the Annex-B
bitstream into `ffmpeg -f h264 -i pipe:0 ... -f image2pipe -vcodec mjpeg` and reads
whole JPEGs back (SOI…EOI). `--source ffmpeg` inverts it and lets ffmpeg fetch the
RTSP stream itself; that path also works and is the fallback if the Rust client
misbehaves on some stream.

Measured at the defaults (960 px wide, jpeg q6, no fps cap): **960×540, ~13 KB per
frame, ~200 KB/s** of panel traffic, running at the camera's own rate — the client
counted **14.7 access units/s, 475 kbit/s** of H.264 on `/vs0`, and the panel
publishes what it decodes. Tunables: `--width` (0 = native 1280×720), `--quality`,
`--fps` (an optional cap; default 0 = the stream's rate), or `CAMWEB_WIDTH` /
`CAMWEB_QUALITY` / `CAMWEB_FPS`; `/channel?ch=vs1` switches to the 640×480
sub-stream and restarts the pipeline.

**Three ffmpeg traps, all hit while building this** — a raw H.264 pipe carries no
timestamps, so ffmpeg guesses:

* `-probesize 500000` made the decoder buffer 500 KB before emitting a frame. The
  pipe holds 64 KB, so `write` blocked, the RTSP reader stopped draining the
  socket, and the panel sat at "connecting" forever. Now 32 KB, and NALs are handed
  to a writer thread (a full queue drops a NAL rather than stalling the reader).
* ffmpeg's `fps=12` filter paces by the assumed 25 fps timebase, so on a 14.7 fps
  stream it emitted `12 × 14.7/25 ≈ 7` fps. The cap is applied in Rust instead.
* Without `-use_wallclock_as_timestamps 1`, a 6 s recording muxed to **2.2 s** and
  played back fast. With it, 6 s → 5.75 s and ~17 fps.

Endpoints: `/` panel, `/stream.mjpg` live multipart stream, `/snap.jpg` single
frame, `/snap` writes `work/video/captures/web-HHMMSS.jpg`, `/ptz?axis&dir&steps&speed`,
`/led?name=white|ir&action=on|off|blink`, `/daynight?mode=day|night`,
`/channel?ch=vs0|vs1`, `/pins` (JSON), `/status` (JSON: online, fps, frames,
dimensions, viewers, last error).

**Moving hardware from a web page needs care, so:**

* PTZ parameters are validated strictly and a bad one is refused with 400 — it is
  never treated as "use the default", because the motors have no end-stops and
  nothing knows where the head is pointing.
* Moves are serialised by a mutex in the panel and, underneath, by `motorctl`'s
  own cooperative `/tmp/motorctl.lock` and `/proc` owner scan. Never run a second
  PTZ writer alongside the panel.
* The page itself says it: steps are phase updates, not degrees, and directions
  0/1 have not been verified against physical movement.
* LED buttons write `/sys/user-gpio/WHITE_LED` and `/sys/user-gpio/IR_LED`
  directly (the frozen `led` binary is not involved). The white LED is the bright
  illuminator; blink is 3 × 300 ms and ends off.
* Day/Night writes `/tmp/photo` (`1` night, `0` day). rtsp's own 5 s thread does
  the ircut, IR and ISP switch; the file is volatile, so the camera still boots
  day.

**Audio is not here and does not belong here** while the audio freeze stands — the
speaker lives in the camera-local panel on :8080 (`analysis/audio.md`).

Two bugs worth remembering from building it, both now fixed: a response whose
headers were never terminated by a blank line produced **0-byte** MJPEG streams
(the browser shows an empty image and no error), and `read()` on ffmpeg's stdout
must happen on its own thread — reading it inline stalls the NAL feed exactly when
the decoder wants more input.

To keep it running: `tools/rust/camweb/camweb.service` is a user unit
(`systemctl --user enable --now camweb`). ufw on firefly defaults to DROP, so the
port needs one hand-added rule: `sudo ufw allow 8090/tcp`.

## Tooling

`cam` runs shell commands on the camera over telnet :2323 (`cam 'cmd'`,
`cam -t 60 -` reads a script from stdin). Since the stock FTP server wants
credentials, files move over `nc` on the board:

    tools/put LOCAL REMOTE     # send a file, retries until the board's sha256 matches
    tools/get REMOTE LOCAL     # retrieve a file, retries until the sha256 matches

`put`/`get` exist because the raw dance is easy to get wrong: the listener takes
a second to appear, probing its port *consumes* the connection, and a stalled
telnet session silently leaves nothing listening. Both use their own ports
(`PUT_PORT` 9996, `GET_PORT` 9995) so parallel work on one board does not
collide. `push LOCAL REMOTE` is the underlying single-shot primitive.

**Anything bigger than a few MB needs `work/pull-big.py`:**

    work/pull-big.py /mnt/SOMETHING.tar backups/SOMETHING.tar SHA256 [PORT]

`tools/get` reads the whole file into RAM and lives inside a 40 s window while
`cam` kills its session at 30 s, so a 26 MB pull dies at ~18–20 MB with a hash
mismatch that looks exactly like a clean EOF (it did, repeatedly). `pull-big.py`
streams to a pid-unique `.part`, uses a 180 s idle timeout and `CAM_TIMEOUT=600`,
and renames only when the sha256 matches. ~690 KB/s on a good card.

See `../ROADMAP.md` for what is working, what is next, and what is parked (ONVIF, Rust `audioctl`).

SD-card rollback, in `backups/` on firefly:

| tar | contents | sha256 |
|---|---|---|
| `factory-sd-2026-10-25.tar` | **current** `/mnt/Factory`, 82 entries — patched `rtsp` + `libplat_drv.so`, boot-LED-guard `config.sh`, final `quiet-cue.sh`, all cues | `6b29ab48…56ba75` (7,580,672 B) |
| `factory-sd-2026-10-24.tar` | superseded, 78 entries, pre day/night patch | `830173d2…be9d5d` |
| `stock-backup-2026-10-25.tar` | the card's `/mnt/stock-backup/` dump tree, 16 entries; contents identical to `stock-192.168.1.103/` | `2ecd7efb…f9104e` |

Restore with `cd /mnt && mv Factory Factory.pre-restore && tar -xf
/mnt/factory-sd-2026-10-25.tar -C /mnt`. Restoring the **10-24** tar also rolls the
day/night patch back — re-apply with `work/irpatch.py --apply` and restart `rtsp`.
busybox on this board has **no gzip**, so `tar -z` quietly produces 0 bytes — use
plain tar.

Card-independent rescue kit: `/data/recovery/` on the board's jffs2 holds the
critical set (patched `rtsp` `fb375089…`, `libplat_drv.so` `cdbd35a3…`, boot-guard
`config.sh` `c0d13ddb…`, `beep`, `quiet-cue.sh` `cea9d54c…`, the three cues) plus
`RESTORE.txt` `6e624457…` — 9 files, 102,880 B, `/data` at 82 % (388 kB free), so an
SD pull does not strand the camera. Telnet-only recovery drill, host copies and the
manifest are in `work/data-recovery/`. Caveat: **editing any kit file makes the hash
line in `RESTORE.txt` stale** — re-hash and re-copy after changes.

A dead/absent card means **no telnet at all**: `service.sh` takes the stock branch and
kills `telnetd`. To get back in you need a minimal `/mnt/Factory/config.sh` (network +
`busybox telnetd -p 2323`) that `service.sh` will run, or the serial console. Also
keep `/etc/jffs2/styleId` deleted — a restore that recreates it breaks video again.

### Rebooting is safe; `df /mnt` and `find /mnt -size 0` are not to be trusted

`tools/cam 'sync; reboot -f'` is a legitimate operation on this board (confirmed
2026-10-25: `[EXFAT] unmounted successfully`, clean remount, every Factory hash
unchanged, video back at 14.4 fps within ~40 s). The 10-24 file damage came from
physically pulling the card mid-write, not from rebooting. A ping/telnet "UP" reply
within a second of issuing it is the **old** kernel still tearing down — wait for
`uptime` to reset before believing the board rebooted.

Two measurement traps on the SD card:

* **`df /mnt` reports the wrong filesystem.** `/etc/fstab` mounts `tmpfs` at `/mnt`,
  then `service.sh` mounts `/dev/mmcblk0p1` (exFAT) over it; `/proc/mounts` keeps both
  lines and busybox `df` resolves the first — a 31 GB tmpfs at 0 %. The card is the
  live one: `stat -c '%n dev=%D' /mnt /mnt/Factory` → `dev=b301` = 179:1.
* **`find /mnt -size 0` gives ~21 false positives** from macOS `.Spotlight-V100/` and
  `.fseventsd/` metadata at the card root. The zero-byte sweep that catches a yank's
  truncation must be `find /mnt/Factory -type f -size 0`.

Also: `dmesg | grep "not inited"` proves nothing here — `printk` is `3 4 1 7`, so
`ak_print` output never reaches dmesg. To check the day/night path on a running
`rtsp`, look for a **`PS_switch`** thread in `/proc/$PID/task/*/comm`.
