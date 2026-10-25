# Roadmap — YI CB211 / AK3918EV200

Status as of 2026-10-25 ~13:45 (host time; board clock runs ~+18 min).
Nothing here has ever written raw flash. The rollback set, all on firefly in `backups/`:

| file | what | size / sha256 |
|---|---|---|
| `stock-192.168.1.103/` (dir) | flash mtd0–7 + `stock-files.tar` + `SHA256SUMS` | 25 MB, 9 hashes verified |
| `stock-backup-2026-10-25.tar` | the card's `/mnt/stock-backup/` tree as one artifact (16 entries) — byte-identical to the dir above, only the 0-byte `.tar.gz` is unique | 26,045,952 B `2ecd7efb…f9104e` |
| `factory-sd-2026-10-25.tar` | **current** `/mnt/Factory` (82 entries) — patched `rtsp`+`libplat_drv.so`, boot-LED-guard `config.sh`, final `quiet-cue.sh` | 7,580,672 B `6b29ab48…56ba75` |
| `factory-sd-2026-10-24.tar` | superseded `/mnt/Factory` (78 entries, pre day/night patch) — keep, it is the pre-patch reference | 7,046,144 B `830173d2…be9d5d` |

Big pulls go through `work/pull-big.py REMOTE LOCAL SHA256` — `tools/get` buffers in
RAM under a 30 s `cam` timeout and **dies at ~18–20 MB with a hash that looks like a
clean EOF**.

## Source control

`~/lab/anyka` is a git repo: `git@git.basestar.tech:andrew/anyka.git`
(private, Forgejo on basestar), branch `master`, first commit `df55179`
2026-10-25. 120 files / 2 MB — notes, scripts and Rust sources, plus the
`.jpg` evidence frames.

Deliberately **not** in it (`.gitignore` has the reasons): `backups/` (the
rollback set, ~64 MB, stays on firefly + elisa + the SD card),
`analysis/rootfs/` and `analysis/base-rootfs/` (extracted stock filesystems
that contain the camera's real `p2pPwd` and root password), the
`Anyka-Camera-Firmware/` upstream clone at `823cb55`, the zig toolchain
tarball, `tools/rust/*/target/`, compiled ARM binaries and `.mp4`/`.pcm`
captures.

So: a fresh clone gives you every note and every source file, but you still
need the host's copies of `backups/` to restore the card or re-derive the
patched binaries.

## Working now (verified on the board, survives reboot)

| Capability | How | Evidence |
|---|---|---|
| Live video | `rtsp://192.168.1.103:554/vs0` — 1280×720 H.264 ~14.3 fps + 8 kHz a-law PCM; `/vs1` 640×480 ~13.8 fps | `analysis/video-pipeline.md`, `work/video/captures/` |
| Snap / record | `tools/snap`, `tools/rec` | `tools/README.md` |
| Audio cue | `tools/beep cue\|warn\|alarm` → `quiet-cue.sh` → `ak_adec_demo` | `analysis/audio.md` |
| Day/night, automatic | `tools/irnight day\|night\|status` — stock `rtsp` moves ircut + illuminator **and** switches the ISP scene in-process (neutral night frames, ~10 fps at night vs ~14 day) | `analysis/ir-daynight.md` |
| Motors | `motorctl AXIS DIR STEPS SPEED`, bounded, no end-stops | `analysis/motor-interface.md` |
| File transfer | `tools/put` :9996, `tools/get` :9995 | `tools/README.md` |
| Browser panel | `tools/camweb serve` -> `http://firefly:8090/` — MJPEG live view, PTZ, LEDs, day/night, pins. Rust RTSP client + Rust board telnet; ffmpeg only decodes H.264 to JPEG | `tools/README.md` -> "Live panel" |

Audio levels are by ear, not meters (the mic saturates): **−40 dBFS = comfortable**,
−30 = still loud, −20 = emergency only. Deployed `audio/cue.pcm` (880→1174 Hz,
0.36 s, −40 dBFS) was confirmed comfortable by the user at 23:47.

> **AUDIO IS FROZEN — user instruction, 2026-10-25 ~13:20 host: "stop fucking
> with the sound. it's super annoying."** No more audio work, no more level
> tuning, no more test tones, no new plays pushed to the board, unless the user
> explicitly asks. The panel stays up because it is the user's tool now, not
> ours: they drive the tests themselves. Everything below is a *record*, not a
> to-do list.
>
> **New speaker fitted 2026-10-25 ~11:15 host, and the boot chime was still not
> heard on it.** Every level above was judged on the *original* speaker, so
> −40/−30/−20 are **uncalibrated on the new one** — and per the freeze above,
> that stays uncalibrated. Do not re-run the level ladder.
>
> Verdict from the panel (user, 2026-10-25 ~13:10): *"yeah boot40 plays from the
> panel. all the panel triggers work."* So on the new speaker the file, the
> script, the keeper and the amp are all confirmed good **on demand** — the only
> remaining audio anomaly is that the same cue was silent at the 00:00:21 cold
> boot (log `rc=0 spk=1 secs=4 iters=80 ticks=40`, i.e. it played). Cause
> (decoder/CPU contention vs amp/DAC cold-boot timing) is **unknown and now
> untested by choice**. Leave it; a silent boot chime is not worth the noise.
> Deployed `config.sh` (`32bd5cfb…`, 2406 B) still plays `audio/boot40.pcm` 5 s
> after start; that stays as-is unless the user asks for it removed.
>
> **Manual testing is now in the browser:** `http://192.168.1.103:8080/` — level
> ladder (−40/−30/−20/−10), a 3 s tone, the cue set, `boot40`/`boot32` (the boot
> chime as `config.sh` plays it), Stop and status. Every audible button runs
> `quiet-cue.sh`, so an on-demand result predicts boot behaviour. Built from
> `work/webui.new` → `www/cgi-bin/webui` (9363 B, `00b6d66f…`); stock page kept as
> `webui.stock`. New assets `wt-40/30/20/10.pcm`, `wt-long.pcm`
> (`work/audio/gen-webtests.py`). Details: `analysis/audio.md` → "Web speaker test
> panel".
>
> The open question is now narrow: **same file, same script, audible on demand and
> silent at boot.** Candidates are decoder start-up contention (`config.sh` at
> t≈5 s competes with `wpa_supplicant`/`telnetd`/`httpd`, libraries paged off
> exFAT) or amp-power/DAC timing at cold boot. Don't chase contention until the
> panel's `boot40` button has been judged by ear; if that is audible, the fix is
> placement/timing, not level.

## Next (small, unblocked, no flash)

1. ~~**Boot-state LED guard**~~ — **proven by a real cold boot 2026-10-25 10:37 host
   (board 00:15:29 reboot → up 0 min).** Polled every 2 s from 41 s uptime to 145 s:
   `photo=0 IR_LED=0 WHITE_LED=0 ircut_a=1 SPK_PA=1`, unchanged, no glow at all — the
   old failure was `/tmp/photo` absent → driver falls back to `/sys/kernel/ain/ain0`
   (constant 1171) → **night**, and the PS thread lit the illuminator ~5 s after
   `rtsp` started. `config.sh` (1603 B, `c0d13ddb…`, backup
   `/mnt/Factory/config.before-ledoff.sh`) writing `0\n` to `/tmp/photo` plus the two
   echo lines fixes it. **Corrected number:** the unreachable window is ~the first
   **30 s** (kernel + squashfs until `config.sh` runs), not 60–80 s — `telnetd` was
   answering 31 s after boot and the pins were already correct at 41 s. Nothing was
   sampled in 0–30 s, so a sub-30 s flash of the stock `IR_LED=1` default cannot be
   ruled out from the card; only serial could.
2. ~~**Chime through the new `quiet-cue.sh`.**~~ **Proven at the same cold boot:** the
   one and only playback in that boot's dmesg is the boot chime, run through the
   deployed 4332 B script — `playback_close:1813 OK`, `event-6: dac stop`,
   `ak39_set_hp_power 0`, then the AEC complaint `dac not ready, open with nr only`
   (normal: the chime is on DAC cdev0, rtsp AEC is not open yet). Decoder-level proof
   the chime played; whether it was *audible* at −40 dBFS is the user's ear, and it
   was confirmed comfortable on 2026-10-24 23:47.
3. ~~**Day/night across a cold boot.**~~ **Proven:** `rtsp` started from `config.sh`
   (pid 683 by 41 s uptime), the thread list contains **`PS_switch`** — so the
   patched automatic path is live from power-on — and the board sat in day
   (`ircut_a=1`, `IR_LED=0`, `photo=0`) for the whole 145 s watch with no night flip.
   Video on the same boot: 85 frames / 5.907 s = **14.4 fps** 1280×720
   (`work/video/captures/coldboot-6s.mp4`, still `coldboot-verified.jpg` RGB
   128/144/152 = day). Note: `dmesg | grep "not inited"` is **not** usable as proof —
   `printk` is `3 4 1 7`, so `ak_print` output never reaches dmesg. Use the thread list.
   Still a **user call**: night-default (`echo 1 > /tmp/photo` in `config.sh`).
4. ~~**Day/night in actual darkness.**~~ **Parked 2026-10-25 until tonight** (user
   call: revisit when the room is actually dark). Everything so far was verified in
   a lit room. Range/noise with the lights off is unmeasured, as is whether the
   `DV5_` block the ISP picks at night is the right one. The illuminator makes its
   own light, so this needs the room dark, not just the stream in night mode — the
   protocol is: user kills the lights and says so, then frames + a `DV5_` block
   comparison. Do not run it in a lit room and call it done.
5. **Card-yank hygiene — archives and rescue kit done, verified; one habit left.**
   `backups/factory-sd-2026-10-25.tar` is the current deployed `/mnt/Factory` (82
   entries) and `/data/recovery/` is a self-contained rescue kit on the board's jffs2:
   9 files, 102,880 B, hashed on `/mnt` and `/data` (8/8 match) and matching the host
   copies in `work/data-recovery/` — patched `rtsp` `fb375089…`, `libplat_drv.so`
   `cdbd35a3…`, `config.sh` `c0d13ddb…`, `beep`, `quiet-cue.sh` `cea9d54c…`, three
   cues, `RESTORE.txt` `6e624457…` (telnet-only restore drill, 9486 B). `/data` is at
   82 % (388 kB free), so nothing else goes there and the kit needs re-hashing after
   any edit to one of its files. **The kit is stale as of 2026-10-25 ~11:15**: its
   `config.sh` copy is `c0d13ddb` while the deployed one is `32bd5cfb` (chime delayed
   5 s, plays `boot40.pcm`), and it predates the web panel. Re-stamp the kit —
   `cp -p` the deployed `config.sh` into `/data/recovery/`, add `custom/webui.new`
   → `cgi-bin/webui` if wanted, then fix the hashes in `RESTORE.txt` — **after** the
   audio verdict, so it is not stale again tomorrow. Left: the zero-byte sweep after any unplug — a yank
   once truncated four files to 0 bytes and exfat kept the directory entries. **Scope
   it: `find /mnt/Factory -type f -size 0`.** Bare `find /mnt -size 0` returns ~21
   false positives from macOS `.Spotlight-V100`/`.fseventsd` metadata at the card
   root. Verified 0 hits under `/mnt/Factory` after the 2026-10-25 reboot.
   A dead card also means **no telnet** (`service.sh` stock branch kills `telnetd`):
   recovering needs a minimal card `config.sh` with network + `busybox telnetd -p 2323`,
   or serial. Keep `/etc/jffs2/styleId` deleted after any restore.
   Restoring the tar: `cd /mnt && mv Factory Factory.pre-restore && tar -xf
   /mnt/factory-sd-2026-10-25.tar -C /mnt`. busybox here has **no gzip applet**
   (`tar -z` silently writes 0 bytes) — always plain `tar` on this board, and pull
   anything big with `work/pull-big.py` (`tools/get` dies at ~18–20 MB with a hash
   that looks like a clean EOF). After restoring from the 10-24 tar, re-apply the
   day/night patch (`work/irpatch.py --apply`) — the 10-25 tar already carries it.
6. ~~**Boot guard for `SPK_PA`.**~~ **Not needed** (2026-10-25): amp is active-low,
   the driver asserts `SPK_PA=1` (mute) during playback, `quiet-cue.sh` holds it at 0
   and restores 1 on every exit path. A cold boot with `SPK_PA=1` played the chime
   fine — the pin starts inert, it is only a mute during playback.

7. **Browser panel (done 2026-10-25, two loose ends).** `tools/camweb serve` →
   `http://firefly:8090/` — MJPEG live view, PTZ arrows, LED buttons, day/night,
   snapshot, live pin read-out. Rust does RTSP (own client, no crates) and the
   board telnet; ffmpeg only decodes H.264 → JPEG. Loose ends: the panel is not
   started on boot yet (`tools/rust/camweb/camweb.service`, `systemctl --user
   enable --now camweb`) and **ufw defaults to DROP, so port 8090 needs
   `sudo ufw allow 8090/tcp` once** — that needs the user's password, so it was
   not done here. The PTZ buttons have never been pressed: directions 0/1 are
   unverified against physical movement, and per the rules the shafts need a
   check before the first real move. Worth knowing from the client work: an
   nginx/1.17.8 fronts `ak_rtsp_demo` and **only the TCP-interleaved `SETUP` is
   routed** (UDP `client_port=` → 404), so a stock player left in UDP mode shows
   nothing; 554 has no authentication, and the SDP's `timeout=60` is advertised
   but never enforced. Protocol detail: `work/rtsp-rust-notes.md`.

## Parked deliberately

### ONVIF (`/mnt/Factory/custom/onvif`, 230406 B)
Anyka's `ak_onvif_demo` — SOAP `GetStreamUri`, own :554 RTSP server, `/onvif/ptz_service`.
Would *squat the port the working stream uses*, so it must be tested alone (stop
`rtsp`, restore after). Value is discovery + control for a recorder; pixels still
come over RTSP. Not worth risking the working stream yet.

### Rust audio engine — `audioctl` (parked 2026-10-24, user decision)
Today's loudness control is amplitude-in-the-file, because `ak_adec_demo` hardcodes
DAC 6 / DEV gain 5 / ASLC 6 and takes no volume argv. That means a "quiet" cue uses
~1% of 16-bit range, and any *long* quiet sound is effectively 9-bit — audible hiss
in principle. A real volume knob needs our own player:

- Target `armv5te-unknown-linux-musleabi` (ARMv6 = SIGILL 132), `build.sh` copied
  from `tools/rust/irctl` / `ispconf`, deployed to `/mnt/Factory/custom/audioctl`.
- Open `/dev/akpcm_cdev0` and drive the documented ioctl sequence
  (`ENABLE_OUTPUT 0x40045081`, `RESET_BUF 0x50e3`, `SET_MODE 0x40045030`, `SET_SOURCE
  0x40045010=4`, `SET_CLOCK 0x40045070=1`, `POWER_CTRL 0x50e2`, `COMMIT_PARAMS
  0x401c50f2`, `STREAM_ENABLE 0x50e0`, `PORT_SYNC 0x50e1` per write) — the same path
  the stock `ipc` and OpenIPC `play_akpcm.c` use.
- Volume via the codec's own DAC/ASLC registers (`ak_ao_set_volume` semantics:
  0–12, 0 mute; ≥6 → dac 6 + aslc vol−6) instead of scaling samples, so files can
  stay near full scale.
- Own the amp pin in-process (no 50 ms keeper fork), and give alarms priority over
  cues.
- AAC/stock prompts would need `dlopen("libmpi_adec.so")`; `libmpi_ao.so` is not on
  the device, so the AO side must be raw ioctls. PCM is the easy first target.
- Parity requirement, as with `motorctl`: identical output to `ak_adec_demo` for the
  existing cue files before it replaces anything.

**Cheap intermediate if volume ever becomes urgent:** copy `ak_adec_demo` and patch
the two hardcoded `#6` gain immediates (`0xa490` DAC, `0xa4a0` ASLC) to a lower
constant → `ak_adec_demo_q`, ~20 minutes of work, no new player.

### Web PTZ buttons
`webui` echoes directions into `/tmp/ptz.daemon` for the dead daemon. Reconciling
that with `motorctl` is possible but the axes have **no end-stops**, so nothing may
drive them unattended. Needs an explicit decision from the user first.

### Notes on the day/night patch (so it is not a mystery in a month)
The change is four strings in two SD-card binaries — `libplat_drv.so`
(`gpio-ircut_a|b` → `ircut_a|b`, `gpio-rf_feed` → `/tmp/photo`) and `rtsp`
(`gpio-ircut_a` → `ircut_a`). Originals sit next to them as `.stock`; the patcher
is `work/irpatch.py` with sha256 guards and a `--check` mode. If either file is
restored from the tar above, the patch goes with it — re-apply with
`work/irpatch.py --apply`, push with `tools/put`, restart `rtsp`. Details,
offsets and evidence: `analysis/ir-daynight.md` → **Fixed**.

## Not on the roadmap
Flashing anything, u-boot, replacing the stock kernel, or any change that needs the
board's flash to be rewritten. Stock backups stay untouched.
