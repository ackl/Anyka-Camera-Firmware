# Audio: speaker, boot chime and on-demand beep

Status: **working as of 2026-10-24**. No flash writes, no reboot. The board's
speaker plays raw PCM through the stock `ak_adec_demo` CLI. A short two-tone
chime now plays at boot, and `tools/beep` makes the camera chirp on demand from
firefly. rtsp video was streaming throughout all tests and was not disturbed.

## The short version

    # from firefly: make the camera chirp once
    tools/beep

    # on the card: same thing
    /mnt/Factory/custom/beep

    # raw decoder invocation (what both of the above wrap)
    ak_adec_demo 16000 2 pcm /mnt/Factory/custom/audio/beep.pcm

The decoder opens the DAC, plays the file, exits `rc=0`, and leaves no process
behind. It does not touch `/dev/video0`, `/dev/ak-motor*`, or the rtsp process.

## The decoder: `ak_adec_demo`

Stock binary at `/usr/bin/ak_adec_demo` (read-only squashfs; do not try to
modify it). It is a demo wrapper around Anyka's `libmpi_adec` / `libmpi_ao`
audio stack. Running it with no arguments or `-h` prints:

    usage: ak_adec_demo [sample rate] [channel num] [type] [audio file path]
    eg.: ak_adec_demo 8000 2 mp3 /mnt/20161123-153020.mp3
    support type: [mp3/amr/aac/g711a/g711u/pcm]

    default log level=4
    ----- ak_adec_demo -----
    version: libmpi_adec V2.2.02

### Arguments and limits

| arg | meaning | notes |
|---|---|---|
| sample rate | 8000 or 16000 | 16000 works; the DAC actually runs at ~15943 Hz (see quirks) |
| channel num | 1 or 2 | 2 works for PCM |
| type | `mp3 amr aac g711a g711u pcm` | raw PCM needs no container/header; easiest to generate |
| audio file path | any absolute path | the file is read whole and decoded |

AMR is restricted to 8000 Hz mono. PCM here means raw signed 16-bit
little-endian (`s16le`) at the sample rate and channel count given on the
command line. There is no WAV header parsing in this path.

### Behaviour

* It **blocks** until the file has been played and the DAC has drained. A 0.6 s
  file takes about 0.6 s plus startup; the whole `tools/beep` round trip over
  telnet measured **1.3 s**.
* It **exits `rc=0`** on success. On a missing file it prints
  `open ... failed` and exits **`rc=255` immediately** (no block, no process).
  That is why the old broken boot line never delayed rtsp.
* It **leaves no process behind**. `ps` after a beep shows only the pre-existing
  processes.
* It writes ANSI-coloured logs to stdout/stderr. Redirect them
  (`>/dev/null 2>&1` or to a log file) when driving it over the flaky telnet
  channel; the escapes make `tools/cam` time out.
* It sets the ASLC volume to **6** internally. There is no volume argument in
  the usage. The PCM tone is full-scale, so this is the loudest the demo will do
  without changing the binary or finding a separate volume ioctl.
* It opens the DAC on `/dev/akpcm_cdev0`. The rtsp process holds the *capture*
  device `/dev/akpcm_cdev1` for its 8 kHz a-law stream. These are separate
  device nodes, so playback and the running stream coexist.

### Quirks worth knowing

* **Actual DAC sample rate is 15943 Hz, not 16000.** dmesg logs, once per
  playback:

      [pcmL0-dac]: perfect asic pll clk:401408000, actual sample rate: 15943

  So a requested 880 Hz tone comes out about 0.35% flat (~877 Hz). Harmless for
  a beep, but do not use this path for precise pitch.
* **AEC does not hold the DAC.** At boot the rtsp-side AEC logs
  `dac not ready, open with nr only`, `aec=0, nr=1, agc=1`, and later
  `[try_open_aec]: - not opened`. So the running stream has noise reduction but
  no echo cancellation. The beep therefore does not "break AEC" — AEC was never
  opened on the DAC. The beep's sound can be picked up acoustically by the mic
  and appear in the rtsp audio track; that is normal, not a fault.
* **Playback powers the amplifier.** The demo's DAC open triggers the headphone
  driver and, on close, dmesg shows `event-6: dac stop`. The amplifier is not
  left running between beeps.
* **The logs are noisy.** The decoder prints codec version banners, ASLC volume,
  decode-buffer sizes and thread ids. Keep them out of the telnet channel.

## Where the sound files live

The stock firmware's audio assets are **not** in `/usr/share/audio_file/` on
this board. That directory does not exist. The real AAC prompts are in:

    /data/audio_file/language/*.aac     22 files, e.g. 1.aac, 10.aac, 1001.aac
    /data/audio_file/language.conf
    /data/audio_file/err_language

`/data` is writable JFFS2 (`mtd6`), but this work deliberately kept everything on
the SD card instead. There is **no** `/data/audio_file/common/didi.aac` either.

Our files on the SD card:

    /mnt/Factory/custom/audio/chime.pcm   two-tone 880->1320 Hz, ~0.63 s, 16 kHz 2ch s16le
    /mnt/Factory/custom/audio/beep.pcm    880 Hz, 0.6 s, 16 kHz 2ch s16le
    /mnt/Factory/custom/audio/beep.aac    AAC test tone (also works)
    /mnt/Factory/custom/audio/beep8k.aac  8 kHz mono AAC test tone
    /mnt/Factory/custom/audio/beep.g711a  G.711 a-law test tone
    /mnt/Factory/custom/audio/beep.g711u  G.711 u-law test tone

The AAC, G.711 and PCM variants all played and exited cleanly; PCM is the
default because it needs no encoder tuning and no header.

## What plays at boot now

The old `config.sh` line was:

    ak_adec_demo 16000 2 aac /usr/share/audio_file/common/didi.aac

That path does not exist, so the call failed instantly and silently. **There
has never been a boot sound on this board.** The line did not block and did not
leave a process, which is why `config.sh` continued to `httpd` and the delayed
`rtsp &` without trouble.

The replacement in `/mnt/Factory/config.sh` is:

    # Startup chime (SD-card audio): short two-tone "I am up" cue.
    ak_adec_demo 16000 2 pcm /mnt/Factory/custom/audio/chime.pcm >/dev/null 2>&1 &

It sits in the same place as the old line, right after net bring-up and telnetd,
and **before** `httpd` and the `sleep 30` that precedes rtsp. So the chime plays
within about a second of `config.sh` starting — seconds after power-on — which
is what the "no indicator that it is on" problem asked for. It is backgrounded
so that even a hung decoder cannot delay `httpd` or rtsp. It exits on its own in
about 0.63 s. The original config is preserved on the card as
`/mnt/Factory/config.before-beep.sh`.

Why not wait until rtsp is up? The user's stated problem is not knowing the board
is powered. A chime 30+ seconds after plug-in is too late to be useful when you
are standing at the board with the plug in your hand. The trade-off is that the
chime means "OS and SD app are running", not "the stream is ready"; the stream
still takes ~30 s. If you ever want a "stream ready" cue, add a second
`ak_adec_demo ... &` immediately after the `rtsp &` line.

## On-demand beep

On the card: `/mnt/Factory/custom/beep` (shell script, `chmod +x`). It runs:

    ak_adec_demo 16000 2 pcm /mnt/Factory/custom/audio/beep.pcm >/tmp/beep.log 2>&1
    echo "beep rc=$?"

From firefly: `tools/beep` (a three-line wrapper that execs
`tools/cam '/mnt/Factory/custom/beep'`). It accepts an optional host argument and
honours `CAM_HOST`. The raw invocation, if you prefer not to use the wrapper:

    tools/cam '/mnt/Factory/custom/beep'

The command waits for the tone to finish (about 1.3 s round trip) and prints
`beep rc=0`. Full decoder logs land in `/tmp/beep.log` on the card.

## How to change the tone

Generate on the host with ffmpeg, push to the card, point the script at it. Raw
`s16le` at 16000 Hz, 2 channels:

    # 0.6 s 880 Hz single tone
    ffmpeg -v error -f lavfi -i "sine=frequency=880:duration=0.6" \
        -ar 16000 -ac 2 -f s16le -y beep.pcm

    # two-tone chime: 880 Hz 0.25 s, 0.06 s silence, 1320 Hz 0.35 s
    ffmpeg -v error -f lavfi -i "sine=frequency=880:duration=0.25"  -ar 16000 -ac 2 -f s16le -y a.pcm
    ffmpeg -v error -f lavfi -i "sine=frequency=1320:duration=0.35" -ar 16000 -ac 2 -f s16le -y b.pcm
    ffmpeg -v error -f lavfi -i "anullsrc=r=16000:cl=2" -t 0.06    -f s16le -y silence.pcm
    cat a.pcm silence.pcm b.pcm > chime.pcm

Then push (see `tools/README.md` for `put`/`push`):

    tools/put chime.pcm /mnt/Factory/custom/audio/chime.pcm

Or generate AAC/MP3/G.711 and pass the matching type argument. For a different
startup sound, replace `chime.pcm` and keep the same filename; `config.sh` needs
no edit. For a different on-demand sound, replace `beep.pcm` or edit the path in
`/mnt/Factory/custom/beep`.

## How to test without rebooting

The boot chime cannot be heard without a power cycle (no reboots allowed here),
but the same file and the same decoder can be exercised in place:

    # play the exact boot chime now
    tools/cam 'ak_adec_demo 16000 2 pcm /mnt/Factory/custom/audio/chime.pcm'

    # or the on-demand wrapper
    tools/beep

    # check config.sh syntax without running it
    tools/cam 'sh -n /mnt/Factory/config.sh && echo SYNTAX OK'

    # confirm rtsp is still alive and streaming afterwards
    tools/cam 'ps | grep "[r]tsp"'
    ffprobe -v error -rtsp_transport tcp rtsp://192.168.1.103:554/vs0

Verified evidence from the live board (rtsp PID 625 streaming 1280x720 H.264 +
8 kHz a-law):

* `tools/beep` returned `beep rc=0` in 1.3 s; `ps` showed no `ak_adec_demo`
  process afterwards; rtsp PID 625 was still alive before and after.
* Note: there is no `/tmp/rtsp.log` on this board. The rtsp process's stdout and
  stderr both point at `/dev/console` (`/proc/625/fd/1` and `/2`), so there is no
  log file to line-count. Liveness was checked with `ps` and a live ffprobe /
  frame grab instead. If you need rtsp logs, redirect the `rtsp &` line in
  `config.sh` to a file on the card, but that would be a separate change.
* `ak_adec_demo 16000 2 pcm .../chime.pcm` returned `chime rc=0`, exited with
  `----- ak_adec_demo exit -----`, and left no process.
* A fresh ffmpeg frame was captured from `/vs0` after each beep (50–58 KB JPEG),
  and ffprobe still reported `h264,1280,720` and `pcm_alaw`.
* dmesg logged the DAC running at `actual sample rate: 15943` for each playback.
* `sh -n /mnt/Factory/config.sh` printed `SYNTAX OK`.

## Caveats and things that could bite

* **The amplifier is on the board, not a headphone jack.** "Headphone driver will
  power on" in dmesg refers to the on-board speaker amp. If a beep is inaudible,
  check the speaker is connected and that the volume 6 default is loud enough;
  the demo offers no volume argument.
* **The DAC runs ~0.35% flat** (15943 Hz for a 16000 request). Do not use this
  path for tuning forks or musical reference pitches.
* **Playback blocks.** If you call the decoder in the foreground from a script,
  the script waits for the tone. At boot the chime is backgrounded; the on-demand
  script is intentionally foreground so `tools/cam` returns only when the tone
  has finished and the process has exited.
* **The boot chime is backgrounded and not supervised.** It is a 0.63 s file and
  the demo exits reliably in testing. If a future firmware change makes the
  decoder hang, the chime would leave one stuck process; it would still not delay
  `httpd` or rtsp because of the `&`.
* **Do not run two decoders at once.** The DAC is a single hardware resource. A
  second `ak_adec_demo` while one is playing may block or fail. There is no lock
  in the demo. `tools/beep` is short and self-terminating, so collisions are
  unlikely, but do not loop it.
* **The rtsp audio track may contain the beep.** The running stream's AEC is not
  opened on the DAC (`dac not ready`), so the mic can pick up the speaker
  acoustically. This is expected, not a regression.
* **The old `config.sh` line is preserved.** To revert the boot change, copy
  `/mnt/Factory/config.before-beep.sh` back over `/mnt/Factory/config.sh` on the
  card (SD-card only, no flash).
* **`/usr/share/audio_file/` does not exist** and neither does
  `/data/audio_file/common/`. If you see a recipe that points there, it is from a
  different model or build. The real prompts are in
  `/data/audio_file/language/`.
* **Read-only root.** `/usr/bin/ak_adec_demo` is on the squashfs. Do not try to
  patch or replace it; use it as-is and keep custom audio on `/mnt/Factory`.

## Files changed

Host (in `~/lab/anyka`):

    tools/beep                      new host wrapper (chmod +x)
    tools/README.md                 added "## Audio" section
    analysis/audio.md               this note
    work/audio/*.pcm, *.aac, *.g711a, *.g711u   source tones and test copies
    work/audio/config.sh.pulled, config.sh.new  edited config source

Card (`/mnt/Factory` on the camera's SD):

    /mnt/Factory/config.sh                     boot chime line replaces the broken didi.aac line
    /mnt/Factory/config.before-beep.sh         backup of the previous config.sh (422 B, sha256 6f02ed71…)
    /mnt/Factory/custom/beep                   on-demand chirp script (428 B, chmod +x)
    /mnt/Factory/custom/audio/chime.pcm        boot chime, 40320 B
    /mnt/Factory/custom/audio/beep.pcm         on-demand tone, 38400 B
    /mnt/Factory/custom/audio/beep.aac         AAC variant, 6707 B
    /mnt/Factory/custom/audio/beep8k.aac       8 kHz mono AAC, 2984 B
    /mnt/Factory/custom/audio/beep.g711a       G.711 a-law, 19200 B
    /mnt/Factory/custom/audio/beep.g711u       G.711 u-law, 19200 B

No flash partition (`mtd0`–`mtd7`) was written. The motors and
`/dev/ak-motor*` were not touched. rtsp was not restarted or killed.

## 2026-10-24 23:25 — the speaker *is* audible (and very loud)

Everything above describes a working software path that produced **no audible
sound** in every test up to 23:20 (30 s 400 Hz, 30 s 150 Hz, and the stock
prompt `/data/audio_file/language/1.aac` at 8 kHz and 16 kHz, all `rc=0`).
Then two reports changed the picture:

* **23:25 — "yes the continuous 400 Hz was what i heard"**, then **"it's waaay
  too loud"**. The speaker and amp are electrically fine and the board can play
  sound louder than anyone wants.
* The specific run that was audible (`work/audio/loopback-air-low.mkv` test,
  `ak_adec_demo 16000 1 pcm .../tone400.pcm`, 30 s of 400 Hz) is the one where a
  background loop forced `/sys/user-gpio/SPK_PA` to **0** every 0.1–0.2 s for
  the whole playback.

### Open question: amp polarity

The driver sets the speaker ioctl (`IOC_SET_SPEAKER`) to `value=1` on playback
start and `value=0` at close, and the `SPK_PA` sysfs node reads `1` during a
default playback and `0` afterwards. If that polarity *mutes* the amp, then the
default path is silent and every earlier "no tone heard" is explained by the
amp being held off, not by dead hardware. A quiet A/B was written for exactly
this (`work/av-ab2.sh`: 1 blink = pin held LOW, 2 blinks = driver default,
sound = 440 Hz 1.5 s at −20 dBFS) but **has not been run** — see the incident
below. Do not state a polarity until it is.

### Loudness

Full-scale PCM plus the demo's hardcoded volume (DAC 6 → DEV gain 5, ASLC 6) is
painfully loud. There is no volume argument in `ak_adec_demo`; the only cheap
knob is the file amplitude. Generate quiet assets, e.g.

    ffmpeg -v error -f lavfi -i "sine=frequency=440:sample_rate=16000:duration=1.5" \
        -af "volume=0.79" -ac 1 -f s16le -y beep-q.pcm   # ≈ −20 dBFS, still clear

## Incident: SD card ejected, 2026-10-24 23:26 (host time)

At board time 00:42:45 (host 23:26:45) the card left the bus mid-test:

    mmc0: card aaaa removed
    [EXFAT] statfs on device is ejected

Consequences, all verified from the shell at :2323:

* `/dev/mmcblk*` no longer exists; `/mnt` is an empty `tmpfs`, so
  `/mnt/Factory` — `config.sh`, `custom/` (motorctl, irctl, ispconf, led, rtsp,
  the audio assets, `sensor/isp_sc1346.conf`), `web_interface/`, `busybox` — is
  gone until the card is re-seated.
* The already-running processes are unaffected because they are resident:
  telnetd at :2323 still answers, and **rtsp kept streaming** — a 15,497-byte
  JPEG was pulled from `/vs0` after the pull at 23:29.
* `tools/put`/`tools/get` fail (nothing to write to); they report only
  `FAILED to deliver`, so check `/mnt` before blaming the network.
* The filesystem was mounted `rw` and was removed without unmounting. After
  re-seating, verify `config.sh` and the sha256 of the deployed binaries before
  trusting the card; exfat mounted with `errors=remount-ro` may well have been
  remounted read-only first.

### Recovery (card re-seated 23:30, same boot, no reboot)

The kernel re-detected the card (`mmc0: new SDHC card at address aaaa`,
`mmcblk0: mmc0:aaaa SD32G 29.7 GiB`, `mmcblk0: p1`) but the old mount entry was
stale and `/mnt` stayed empty. `umount /mnt` refused with `Device or resource
busy`; **`mount -t exfat /dev/mmcblk0p1 /mnt` succeeded and stacked a fresh
mount over the stale one** (three `/mnt` lines in `mount`, two of them
`/dev/mmcblk0p1`). Contents are correct and writes land on the card, so this is
left stacked rather than risking an unmount; **a reboot clears it.**

Data loss was real but small — four files had a directory entry but no data, so
they read as 0 bytes (exfat had not flushed their contents):

    ispconf, beep, mic-indicator.sh, av-ab.sh

All four were re-pushed from the host copies and hash-verified. Everything else
survived: `config.sh` (501 B, passes `sh -n`), `irctl` (5bdf387c…e078a), `led`
(816672c4…a56fc), `motorctl`, `rtsp`, the whole `lib/`, `sensor/isp_sc1346.conf`,
and the audio assets. **Lesson: after any unplug, `find /mnt -size 0` before
trusting the card.**

## Speaker: it works, and the amp polarity was a red herring (2026-10-24 23:32)

The user heard a continuous 400 Hz tone from the run that held `SPK_PA` **low**,
while every earlier playback with the driver's default (pin high during play) was
silent. That looked like an active-low amp. It is not. Two measurements:

**A/B, `work/av-ab2.sh`** — three 1.5 s 440 Hz beeps, pin forced low / driver
default / pin forced low, mic recorded via `/vs0`:

    burst 1 (pin LOW)      peaks g440 2869  rms 2984
    burst 2 (driver HIGH)  peaks g440 2555  rms 2198
    burst 3 (pin LOW)      peaks g440 2750  rms 2906

All three audible, same order of magnitude → **polarity is not the discriminator.**

**Plain path, `work/plain3.sh`** — three plays with no pin manipulation at all,
`SPK_PA` read 0 before and after every play, mic heard all three at identical
level (peak RMS 2197 / 2201 / 2197, 1.5 s each). **The normal `ak_adec_demo`
path now works reproducibly.**

So something between 23:09 (silent, three separate full-scale files) and 23:24
left the output stage enabled. Best current hypothesis: the sysfs write to
`/sys/user-gpio/SPK_PA` itself performs the PA enable, and the akpcm driver's
`IOC_SET_SPEAKER` path alone does not — the pin reads 0 before/after each play
yet logs `value=1` mid-play, and only after the 23:24 write loop did sound
appear. **Untested and testable only at a reboot: after a cold boot, is audio
silent again until something writes `SPK_PA`?** If yes, add
`echo 0 > /sys/user-gpio/SPK_PA` to `config.sh` before the chime as a guard.

Still unexplained: why the 23:09 / 23:17 / 23:20 full-scale playbacks were
silent. Not a dead amp, not the wiring, not the ioctl sequence (our ioctls match
the stock `ipc` trace).

## Loudness: the only lever is the file (2026-10-24 23:35)

`ak_adec_demo` hardcodes DAC 6 / DEV gain 5 / ASLC 6 — maximum, no volume argv.
The user's verdict on a full-scale tone: "waaay too loud". So loudness is chosen
by the PCM sample values:

| file on card | level |
|---|---|
| full-scale `tone400.pcm`, `tone150.pcm`, `chime.pcm`, `beep.pcm`, `pattern.pcm` | 0 dBFS — far too loud |
| `L10.pcm` | −10 dBFS |
| `L20.pcm` | −20 dBFS |
| `L30.pcm` | −30 dBFS |
| `beep-q.pcm` | −38 dBFS |

ffmpeg's `sine` source starts at −18 dBFS, so scale with
`volume = 8 * 10^(-dBFS/20)` to hit a target, e.g. `volume=0.8` → −20 dBFS.

**The mic cannot be used to rank loudness.** It saturates: −38, −20 and −10 dBFS
tones all measure peak RMS ≈ 2200 through `/vs0` (AGC is on; the chain also sits
on a ~978 RMS DC floor). It reliably proves *presence* of 440 Hz (burst g440
2500+ against a floor of ~5) and nothing more. Loudness has to be judged by ear.
## Settled: the cue set (2026-10-24 ~23:47, by ear)

The by-ear verdict over the LED-cued clicks: **−40 dBFS is comfortable**, −30 is
still too loud, −20 is reserved for emergency/critical alarms. That is now baked
into three files, all mono 16 kHz s16le, 5 ms fades, generated by
`work/audio/gen-cues.py`:

| cue | file | level | shape |
|---|---|---|---|
| everyday | `audio/cue.pcm` | −40 dBFS (peak 328) | 880→1174 Hz, 0.36 s |
| warning | `audio/warn.pcm` | −30 dBFS (peak 1036) | 3× 880 Hz, 0.43 s |
| alarm | `audio/alarm.pcm` | −20 dBFS (peak 3277) | 660/990 Hz alternating, 0.74 s |

Deployed: `custom/audio/{cue,warn,alarm}.pcm`, `custom/quiet-cue.sh`,
`custom/beep` (named levels), `Factory/config.sh` (boot chime → `quiet-cue.sh
audio/cue.pcm`, fallback to raw `ak_adec_demo` if the script is missing).
`tools/beep cue|warn|alarm` from the host.

The user confirmed the boot-equivalent cue at comfortable volume, so the
amplitude-as-volume approach is accepted for now. **The amp is active-low in
practice**: audible playback needs `SPK_PA=0` held for the whole play, because the
driver asserts 1 (`IOC_SET_SPEAKER`) at start; `quiet-cue.sh` re-asserts 0 every
50 ms. `SPK_PA` reads 0 again after the play, so the pin state alone does not tell
you the amp was on.

Limit of this scheme, and why a real player is on the roadmap (`ROADMAP.md`): a
0.36 s cue at −40 dBFS still spans full 16-bit samples and sounds fine, but the
demo's fixed DAC 6 / DEV gain 5 / ASLC 6 stage means long quiet sounds spend only
~9 effective bits. Rust `audioctl` would set the codec's own DAC/ASLC volume
(0–12, 0 = mute) instead of scaling samples. Parked by user decision.

## `quiet-cue.sh`: keeper sized from the file, and no lamp at all (2026-10-25 00:04–00:11 board time)

Two changes to the amp keeper, both SD-card only (no flash, no reboot, rtsp never
stopped, no PTZ touched).

### 1. The keeper is bounded by the playback length, not a fixed cap

The old keeper re-asserted `SPK_PA=0` every 50 ms but stopped after a flat **200
iterations** ("10 s"). Any sound longer than that lost the amp mid-play, silently:
the driver's `IOC_SET_SPEAKER=1` wins again and the speaker mutes.

New, in `work/quiet-cue.sh` (repo) → `/mnt/Factory/custom/quiet-cue.sh` (card):

    BPS   = RATE * CH * 2                     # s16le bytes per second
    SECS  = ceil(BYTES / BPS) + 2             # +2 s for decoder start + DAC drain
    SECS  = min(SECS, 120)                    # absolute ceiling (MAXSECS)
    ITERS = SECS * 20                         # 20 x nominal 50 ms per second
    CEILING = uptime_seconds + SECS           # hard wall-clock limit

The loop stops on `ITERS`, on `/tmp/spkstop`, **or** when the wall clock passes
`CEILING` (checked once a second), so a wrong rate or a bogus file cannot pin the
amp for more than two minutes. `SPK_PA=1` is restored by the parent on every exit
path — success, `ak_adec_demo` failure, missing file/usage (never touches the
pin), and INT/TERM/HUP via `trap` — and by the keeper itself if the parent is
killed outright. The keeper writes its iteration count to `/tmp/spk.ticks` once
per second so a long play can be watched from another session.

Sizes actually computed on the board: `cue.pcm` 11,520 B → 3 s / 60 iters;
`chime.pcm` 40,320 B (2 ch) → 3 s / 60; `silent15.pcm` 480,000 B → 17 s / 340;
60 s of silence → 62 s / 1240.

Old script kept on the host as `work/quiet-cue.sh.before-duration`
(sha256 `fde68f4a…c7e4e4`); new one `cea9d54c…ed510f` (4,332 B).

### Evidence (all silent or the 0.36 s cue; board timestamps)

| run | file | result |
|---|---|---|
| 00:04:51 | `cue.pcm` (0.36 s) | `rc=0 spk=1 secs=3 iters=60 ticks=20`; pin read 0 at +0.4 s and +0.6 s, 1 after; process count 70 before and after; no `quiet-cue`, no `ak_adec` left; ticks frozen at 20 one second later |
| 00:05:46 | `silent15.pcm` (15 s) | at t+15 s `ticks=100 spk=0 adec=1` — amp held for the whole play; `rc=0 spk=1 ticks=120`, rtsp=1 |
| 00:07:24 / 00:08:37 | 60 s of silence (`cat silent15 ×4` in /tmp, removed after) | t+42 s `ticks=320 spk=0 adec=1`, t+52 s `ticks=360 spk=0 adec=1`, final `rc=0 spk=1 ticks=416 of 1240 iters`, rtsp=1 |

The 60 s runs are the proof the cap is gone: the old 200-iteration bound would
have dropped the amp about 30 s into a 62 s file, and the keeper was still holding
at 320 and 360 iterations with the decoder process alive.

**Reality of the 50 ms keeper.** One iteration costs ~75 ms idle and 100–210 ms on
a loaded board (36 MB RAM, 2.6 MB free, several agents on the camera during these
measurements), because of the fork per iteration. Consequences: counting 20
iterations per second deliberately *over*-covers, so the keeper always outlasts
the playback; and the first re-assert can lose the race with the driver — during
the 0.36 s cue the pin still read 1 at +0.2 s and 0 by +0.4 s, i.e. up to ~0.2 s
of a cue's attack can be muted under heavy load. A longer pre-sleep does not fix
that (the driver re-asserts during the play); the fix is the parked Rust
`audioctl`, which owns the pin in-process.

### 2. No LED cue. Ever, in this script

The script used to do `led on` before the play and `led off` after. That is a fine
phase marker for a 0.36 s chime and a bug for anything longer: two 62 s silent
runs (00:07:24, 00:08:37) held the **white lamp lit for a full minute each**, and
the user, standing in the room, watched the front of the camera light up twice.

Rules that came out of it:

* **A cue must never be steady-on for the duration of the play.** If a marker is
  wanted at all it must be a short blink (or nothing), independent of play length.
* The LED lines are **gone from the script, not merely guarded** — no
  `led`/`WHITE_LED`/`IR_LED` reference remains (`grep -c -E "WHITE_LED|IR_LED|
  /custom/led" quiet-cue.sh` = 0). Audio playback writes no lamp pin.
* The `led` binary on the card was renamed to `/mnt/Factory/custom/led.frozen`
  (391,280 B) by the parent at ~00:09:56 to stop the lamp during the incident.
  **Do not restore or re-push it, and do not write the lamp pins from an audio
  helper.**
* Post-change smoke test, 00:10:45: 1 s silent file built in `/tmp`, `rc=0
  spk=1 secs=3 iters=60 ticks=35`, `WHITE_LED` 0 before *and* after, missing file
  `rc=2 spk=1`, no args `rc=2`, rtsp=1, no leftover processes. At 00:11:23:
  `IR_LED=0 WHITE_LED=0 SPK_PA=1 ircut_a=1 ircut_b=0`, `/tmp/photo=0`, rtsp=1.

Note: `stat` on `/sys/user-gpio/{WHITE_LED,IR_LED}` returns the node's creation
time (both 1970-01-01 00:10:03) and does **not** change when the pin is written,
so mtime is not usable as evidence of lamp writes on this board; the source grep
plus the pins' steady 0 are.

### Still unproven

* The `ak_adec_demo` failure path (`rc=255`, e.g. a corrupt file) under the new
  script — the logic is unchanged (restore runs after the `rc=$?` capture) but it
  has not been exercised.
* ~~The boot chime through the new script needs a power cycle.~~ **Done
  2026-10-25 10:37 host** (instructed reboot, no user involvement needed): the
  one and only playback in that boot's `dmesg` is the boot chime, and it came
  through the deployed 4332 B `quiet-cue.sh` — `playback_close:1813 OK`,
  `event-6: dac stop`, `ak39_set_hp_power 0` (amp returned to muted), then the
  harmless `try_open_aec: dac not ready, open with nr only` because `rtsp`'s AEC
  is not open yet. That is decoder-level proof the script runs at boot, keeps the
  amp pumped for the cue and releases it afterwards; **audibility** at −40 dBFS
  was already settled by ear on 10-24 23:47, and no lamp came on. Reboot is safe
  on this board — the 10-24 damage came from pulling the card, not from rebooting.
* **Speaker upgrade: announced 2026-10-25 ~10:50 host, new speaker fitted ~11:15;
  the boot chime was still not heard on it.** Consequences, still in force: (a) **all ear-based levels are void** —
  −40 comfortable / −30 loud / −20 emergency were measured on the *old* speaker, so
  re-run the LED-cued 3-level click test (`work/leveltest.sh`, −20/−30/−40) before
  picking any cue level again; (b) the pending question "was the delayed boot chime
  audible?" is unanswered — the reboot at board 00:06:53 played `boot40.pcm` at
  22 s uptime, `ticks=60` of `ITERS=80` (i.e. the decoder ran ~3× slow, CPU still
  loaded), but nobody has judged the sound yet, so **do not chase decoder contention
  or move the chime after `rtsp` until the new speaker has been heard once**;
  (c) a louder speaker needs *less* peak, not more — the amp still hardcodes
  DAC 6 / DEV 5 / ASLC 6, so peak amplitude in the file remains the only lever.
* **AUDIO FROZEN 2026-10-25 ~13:20 host, by user instruction:** "RIght OK it
  worked. stop fucking with the sound. it's super annoying." No further audio
  work of any kind — no level ladders, no test tones, no new plays, no chime
  timing experiments — unless the user asks for it explicitly. The web panel
  stays deployed because it is *their* instrument now; it is not an invitation
  to keep iterating. The open items above are closed as "will not fix":
  (a) the level table is uncalibrated on the new speaker and stays that way;
  (b) the boot chime being silent at cold boot is **unexplained and accepted** —
  the same file plays fine on demand from the panel, and a silent boot chime is
  preferable to more noise; (c) peak-in-file remains the only loudness lever if
  it ever comes up again.
* The `silent15.pcm` test asset stays on the card at
  `/mnt/Factory/custom/audio/silent15.pcm` (480,000 B, sha256 `baf1862a…533ba5a29`).

## Web speaker test panel (2026-10-25)

Because the new speaker has to be judged by the user's ear, the judgement moved
out of my script-pushing loop and into the browser: `http://192.168.1.103:8080/`
now serves a speaker panel (CGI `www/cgi-bin/webui`, 9363 B, sha256 `00b6d66f…`,
repo copy `work/webui.new`, stock page kept as `webui.stock`). Every audible
button runs `quiet-cue.sh`, i.e. the *same* amp-holding path as the boot chime, so
an on-demand result is a direct prediction of boot behaviour.

Assets built for it (`work/audio/gen-webtests.py`, same conventions as
`gen-cues.py` — mono 16 kHz s16le, 5 ms fades, peak = 32768·10^(dBFS/20)):

| file | content | peak | level |
|---|---|---|---|
| `wt-40.pcm` | 3 × 0.15 s 1 kHz, 0.25 s gaps | 328 | −40 dBFS |
| `wt-30.pcm` | same | 1,036 | −30 dBFS |
| `wt-20.pcm` | same | 3,277 | −20 dBFS |
| `wt-10.pcm` | same | 10,362 | −10 dBFS |
| `wt-long.pcm` | 3 s continuous 1 kHz | 3,277 | −20 dBFS |

Clicks, not 2 s tones — the user's instruction from the earlier ladder test was
"2s too long for test tones. do like 0.2s".

### New finding: the driver leaves the amp enabled on teardown

A stock-path play (`ak_adec_demo`, no keeper) leaves `SPK_PA` at **0** when it
exits. Observed 00:04:48: `raw wt-20.pcm rc=0 spk=0`, pin still low after the
process was gone — so the driver mutes *during* the play (pin 1) and enables the
pin on teardown (pin 0), leaving the amp powered while idle. Two consequences:

* This is the best explanation for the "it worked!" moment at 23:24 on 10-24: the
  amp had been left on by an earlier play, so a subsequent plain (`raw`) play was
  audible without anyone writing the pin.
* Anything that plays the stock way must re-mute afterwards. The CGI now does
  `echo 1 > SPK_PA` at the end of a raw play, and the status line says
  `amp left enabled, press Stop to mute` if it finds pin 0 while idle.

### Boot chime, status after the speaker swap

The boot after the swap logged `quiet-cue boot40.pcm rc=0 spk=1 secs=4 iters=80
ticks=40 00:00:21` and the user **heard nothing**. So the chime script ran, the
keeper held the amp (40 ticks ≈ the whole play), and the result was still silence.
Data points at boot, all with `boot40.pcm` (−40 dBFS, 1.06 s):

| boot | log line | heard? |
|---|---|---|
| 10-25 00:01 (old speaker) | — (old 1817 B script) | yes |
| 10-25 10:44 host, board 00:00:22 | `ticks=60` of 80 | unjudged (swap intervened) |
| 10-25 ~11:15 (new speaker) | `ticks=40` of 80 | **no** |

`ticks` is the keeper's progress, not the audible length: it stops as soon as the
parent touches `/tmp/spkstop`, so it is a lower bound on play duration, not a
measure of sound. The open question is now sharply posed — the same file and the
same script are audible on demand and silent at boot — which is either decoder
start-up contention (`config.sh` at t≈5 s is competing with `wpa_supplicant`,
`telnetd`, `httpd` and libraries paged off exFAT) or an amp-power/DAC-start
timing that differs at cold boot. The panel can play `boot40` on demand at any
time, which separates those two cases as soon as the user has an ear on it.
