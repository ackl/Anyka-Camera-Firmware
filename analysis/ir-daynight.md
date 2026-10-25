# IR-cut and day/night: the driver is dead, the GPIOs are not

Status: **working automatically, ISP included, as of board clock 2026-10-24
00:40, and proven across a real cold boot on 2026-10-25 10:37 host** (see
"Cold boot, measured" at the end). `tools/irnight night|day` flips it; the stock
`rtsp` process moves the filter, the illuminator and the ISP scene within 5 s,
with no violet night frames. Still unverified in real darkness.

The sections below are in order of discovery and kept that way: the hand-driven
GPIO work (still valid, still the fallback), the diagnosis of why the built-in
path was dead, then **Fixed** at the bottom for what was actually changed.

## The broken path

Every `rtsp` start logs:

```
[ak_drv_ir_get_input_level:129] not inited
```

That is `libplat_vi.so` asking the kernel for a day/night decision and getting
nothing back. In the stock flash the photo-sensor GPIO is configured `-1`
(unset, same class of problem as the motor end-stops — the vendor's EVB wiring
is not the shipping board's), so the driver initialises no IR input at all.
Consequence: **automatic day/night has never worked on this camera**, and
Factory mode does not change that. It also means nothing in the stock software
ever moves the IR-cut filter here.

(That last sentence is wrong — see **Fixed**: the stock `rtsp` does move the
filter, the illuminator *and* the ISP, as soon as `libplat_drv.so` stops asking
for sysfs nodes this firmware does not create.)

## The path that works

The Anyka companion core exports the IR bits as plain user-gpio files, each
`-rw-rw-rw-`, containing `0` or `1`:

| File | What it does |
|---|---|
| `/sys/user-gpio/IR_LED` | IR illuminator power, `1` = on |
| `/sys/user-gpio/ircut_a` | IR-cut solenoid, coil A |
| `/sys/user-gpio/ircut_b` | IR-cut solenoid, coil B |
| `/sys/user-gpio/WHITE_LED` | the white status LED (what the web UI's LED buttons use) |
| `/sys/user-gpio/SPK_PA` | speaker amplifier enable (audio agent's territory) |

They are ordinary read/write attributes: `echo 1 > /sys/user-gpio/IR_LED`.

### The filter is a latching solenoid, not a level

The IR-cut mechanism is driven by two coils and mechanically latches. You
**pulse** a coil — raise, hold ~300 ms, drop — and the filter stays where it was
pushed with both registers back at `0`. Consequences:

- You cannot read the current filter position. `ircut_a` and `ircut_b` read `0`
  in *both* states. Asking the hardware where the filter is does not work.
- Holding a coil at `1` indefinitely is not how it is meant to be used.
- On the board as found, `ircut_a` = **filter out** (night), `ircut_b` =
  **filter in** (day). Pulsing the same coil twice is harmless and idempotent.

Because position is unreadable, `irctl` records the mode it last set in
`/tmp/irctl.mode`. That file is a convenience only: after a reboot the physical
position is whatever it physically was, so run `irctl day` or `irctl night` if
the state is in doubt.

## Evidence

Same scene, same running stream, frames pulled over RTSP and compared
(`work/video/captures/`):

| Frame | Mean RGB | Look |
|---|---|---|
| `post-reboot-frame.jpg` | 118 112 107 | natural colour, true blacks (baseline, filter in, illuminator on) |
| `ircut-a-pulsed.jpg` | 131 121 116 | washed, flat, IR-pink — filter out, illuminator flooding the scene |
| `ircut-b-restored.jpg` | 84 91 60 | green-cast transient ~2 s after switching back (AE adapting) |
| `ircut-day-settled.jpg` | 121 109 100 | back to baseline: the move is reversible |

Two things are established by that table:

1. The illuminator really emits — `IR_LED=1` with the filter **in** produces a
   perfectly normal image (IR is blocked), and the same `IR_LED=1` with the
   filter **out** floods the sensor. So both the LED and the filter mechanics
   work.
2. Switching is reversible and the AE recovers in a second or two. Do not judge
   a switch by the first frame — it is washed out or tinted.

Note `IR_LED` is already `1` at boot, left there by the stock startup. Harmless
while the filter is in; wasteful otherwise.

## What is still wrong: the ISP does not know

*(Resolved — see **Fixed** at the bottom of this file.)*

`irctl night` moves hardware, but the ISP still runs its **day** configuration:
AWB and CCM are tuned for a filter-in, colour-accurate scene. With the filter
out and IR pouring in, the result is a badly tinted, blown-out image
(`night-mode-frame.jpg`: RGB 165 161 242 — violet, highlights clipped). Usable to
see that *something* is there, ugly to look at.

The library does have a scene/mode switch:

- `isp_switch_mode` at `0x86f8` (316 B) — not a GPIO driver: it copies a packed
  ISP attribute block (`isp_module_check_cfg` → `isp_set_attr` per attribute,
  then `isp_set_effect`, `init_fps_info`, `set_exptimemax_by_curfps`) and stores
  it into the ISP context at `0x15fd8`.
- Strings `switch to day indoor` (`0xc54b`), `switch to day indoor success`,
  `switch to day outdoor` (`0xc591`), `switch to day outdoor success` exist in
  `.rodata`, and `ak_vi_switch_mode` (`0x6330`) wraps `isp_switch` (PLT
  `0x4634`). No literal-pool reference to those strings exists in `.text`, so
  they are reached by PC-relative arithmetic — finding their caller needs a real
  disassembler, not a grep.
- Nothing in this application calls any of it. There is no automatic day/night.

Doing this properly means calling `ak_vi_switch_mode` (or setting AWB/CCM/denoise
for IR) **from inside the process that owns the VI/ISP** — i.e. from the
streaming app, not from a separate tool. `rtsp` is a prebuilt binary; sources sit
in `Anyka-Camera-Firmware/src/rtsp/` but rebuilding needs the uClibc/ARM toolchain
and the vendor libs' exact ABI. That is the next step if night vision matters
beyond "I can see something".

## Correction (2026-10-24 ~23:55): the code is already there — three wrong names

Re-read `Anyka-Camera-Firmware/src/rtsp/ak_rtsp_demo.c` now that live video works.
The day/night switching we were about to rebuild **already exists and is already
started** by the running binary:

- `main()` → `init_other_platform()` → `ak_drv_ir_init()` +
  `ak_misc_start_photosensitive_switch_ex(HARDWARE_PHOTOSENSITIVE, SET_AUTO_MODE)`
  (the `2` in the source is `SET_AUTO_MODE`).
- That spawns `photosensitive_switch_th_ex`: every 5 s `ir_val =
  ak_drv_ir_get_input_level()`, and when it changes calls
  `ak_misc_set_video_day_night(vi_handle, !ir_val, DAY_LEVEL_HH)`, which does
  `camera_set_ir()` on the ircut/IR files **and** `ak_vi_switch_mode(vi_handle,
  VI_MODE_DAY|VI_MODE_NIGHT)` — in process, with the real VI handle. Exactly the
  call this note says is missing.
- The source matches the deployed binary exactly: `src/rtsp/rtsp.out` sha256
  `7cbce393…80e6d9`, 20 949 B = `/mnt/Factory/custom/rtsp`. No rebuild needed to
  *get* the feature; it is dead for one reason only:

`libplat_drv.so` (26 594 B, deployed in `custom/lib`) hardcodes sysfs names that
this firmware does not create:

| Symbol / string | Address | What it is | Exists on board? |
|---|---|---|---|
| `/sys/user-gpio/gpio-ircut_a` | `.rodata` `0x3473` | stat'd by `ak_drv_ir_init` | **no** (board has `ircut_a`) |
| `/sys/user-gpio/gpio-ircut_b` | `.rodata` `0x348f` | stat'd by `ak_drv_ir_init` | **no** (board has `ircut_b`) |
| `/sys/user-gpio/gpio-rf_feed` | `.rodata` `0x33bb` | photo input, read by `ak_drv_ir_get_input_level` | **no** |
| `/sys/kernel/ain/ain0` | `.rodata` `0x33d9` | ADC fallback for the same read | no |

`ak_drv_ir_init` (`0x1820`, 420 B): `stat()`s ircut_a and ircut_b; both fail →
`r6 == 3` → "Ircut a & b interface can't access" → returns -1 and never sets
`ir_ctrl[8]` (the inited flag). `ak_drv_ir_get_input_level` (`0x14a4`, 428 B):
first thing it does is `ldr r3,[ir_ctrl,#8]; cmp #0` → `set_error_no(6)`,
`ak_print("…%d] not inited.")` with line 129, **return -1** — verbatim the log
line we have been reading all evening. Then (if inited) `fopen` rf_feed, `fread`
2 bytes, `atoi`; on failure the `ain0` path with a 4-byte read and thresholds
1000/2000 selected by check mode.

So the fix is not a toolchain: **same-length string patches on the SD card**, no
flash, reversible by restoring the two files.

1. `libplat_drv.so` `.rodata`:
   `gpio-ircut_a` → `ircut_a`, `gpio-ircut_b` → `ircut_b` (27 → 24 bytes, NUL
   padded, file size unchanged), `gpio-rf_feed` → a writable path we control
   (e.g. `/data/photo`), same-length-or-shorter.
2. `rtsp` `.rodata`: `gpio-ircut_a` → `ircut_a` (27 → 24); its `IR_LED` path is
   already correct, so the app's own `camera_set_ir()` writes then work too.
3. Result: the built-in 5 s thread reads our file — `1` = night (filter out, IR on,
   `VI_MODE_NIGHT`), `0` = day — and the ISP switches itself. Keep `irctl` for
   manual override, but only one of the two may pulse the filter.

Unverified and worth saying out loud: `VI_MODE_NIGHT` on a live stream may glitch
or drop the encoder for a moment, and the night mode still uses whichever ISP
block `styleId` selects (see below), not a verified night parameter set.

**Both of those turned out fine** — no encoder glitch, and the picture is a
normal neutral night frame. Done with `/tmp/photo` rather than `/data/photo`, for
the reason given in **Fixed**.

The three `DV5_` blocks in `isp_sc1346.conf` (see `video-pipeline.md` and
`tools/rust/ispconf/`) are plausibly per-scene parameter sets — indices 0, 1, 2 —
but nothing has verified that block 1 is "night". That is a cheap experiment for
someone with the ISP debug tooling, not something to guess at.

## Commands

```
tools/cam '/mnt/Factory/custom/irctl night'     # filter out + illuminator on
tools/cam '/mnt/Factory/custom/irctl day'       # illuminator off + filter in
tools/cam '/mnt/Factory/custom/irctl status'    # raw GPIO values + remembered mode
tools/cam '/mnt/Factory/custom/irctl ir on'     # illuminator only
tools/cam '/mnt/Factory/custom/irctl filter out'
```

The tool is `tools/rust/irctl/` (Rust, std only, no deps), built for
`armv5te-unknown-linux-musleabi` by `tools/rust/irctl/build.sh`, deployed at
`/mnt/Factory/custom/irctl` (394,200 bytes). It works on any Linux against a
fake directory too — `ANYKA_GPIO_DIR=/tmp/fakegpio irctl night` — which is how it
was smoke-tested before it ever touched the board.

## Safety notes

- Moving the filter and switching the illuminator are non-destructive and
  instantly reversible. This is not a flash write and not a motor command.
- Do not leave the illuminator on with the filter out in a bright room for long;
  it achieves nothing and the AE fights it.
- Opening `/dev/ak-motor*` is a completely different subsystem — nothing here
  touches it.

## Fixed (board clock 00:28–00:40, 2026-10-24): three strings, ISP included

The plan above was right, and it was the whole job. No toolchain, no rebuild, no
flash — two files on the SD card, four string slots.

### What changed

| File | Offset | Was | Now | Bytes |
|---|---|---|---|---|
| `custom/lib/libplat_drv.so` | `0x33bb` | `/sys/user-gpio/gpio-rf_feed` | `/tmp/photo` | |
| `custom/lib/libplat_drv.so` | `0x3473` | `/sys/user-gpio/gpio-ircut_a` | `/sys/user-gpio/ircut_a` | |
| `custom/lib/libplat_drv.so` | `0x348f` | `/sys/user-gpio/gpio-ircut_b` | `/sys/user-gpio/ircut_b` | 49 total |
| `custom/rtsp` | `0x1d18` | `/sys/user-gpio/gpio-ircut_a` | `/sys/user-gpio/ircut_a` | 12 |

Each replacement sits inside its own 28-byte slot, NUL-padded. The next string in
`.rodata` starts exactly at the slot boundary (`0x348f`, `0x34ab`, `0x1d34`), so
nothing beyond the slot moved. Sizes are unchanged: `26 594` and `20 949` bytes.

Checked mechanically before it went near the board:

- `llvm-objdump -d` of both patched files is **byte-identical** to the originals
  — the strings are data, not code.
- `readelf -SW` and `-d` identical: same sections, same `NEEDED` list.
- The byte diff consists of exactly four runs, all inside the four slots.
- The `rtsp` literal includes the `/sys/user-gpio/` prefix, so its slot starts at
  `0x1d18`, not at the `gpio-ircut_a` text (which begins at `0x1d27`). Getting
  that wrong is how the first patch attempt failed its own check.

| | original sha256 | patched sha256 |
|---|---|---|
| `libplat_drv.so` | `383d7d710f3c24aa18cd852a94bdfabe751cf6d7db4806f53ddf198fcb9e9e72` | `cdbd35a321c0baf938aae42225b9685337b0639c73085b69fcbc22680c0dcafe` |
| `rtsp` | `7cbce393cf2452a23f41e71c03c00ab7c98cb0ac8d6062e18a42a621a980e6d9` | `fb375089d8b6c1ecce4a16d7fa6db7df66337faef53503249669dcd2c77e1c6b` |

Originals are on the card as `custom/lib/libplat_drv.so.stock` and
`custom/rtsp.stock` (sha256 verified after copying). Rollback: copy them back over
the live names, kill `rtsp`, start it again. The patcher is `work/irpatch.py`:

```
work/irpatch.py --check FILE       # patched / unpatched / unknown build
work/irpatch.py --apply IN OUT     # refuses anything but the two known builds
```

### The control surface: `/tmp/photo`, two bytes

`ak_drv_ir_get_input_level` does `fopen(path)`, `fread(buf,1,2,fp)`, `atoi(buf)` —
two bytes into **a stack slot it never zeroes** (it is the saved `r3` from the
prologue, at `sp+12`). So the file must be exactly two bytes ending in a newline:

```
printf '1\n' > /tmp/photo
```

The newline is the trick: `atoi` stops there, so whatever the leftover stack byte
is cannot leak into the parse. A one-byte file lets it through.

`ak_drv_ir_init` sets check mode **4** (not 0–3) with thresholds 1000/2000 and
`[+0x18]=1`, `[+0x1c]=2`, `[+0x20]=618`. Mode 4 is the hysteresis branch at
`0x15e0`:

| value read | level returned |
|---|---|
| `> 1000` | 1 |
| `== 1` | 1 |
| `== 0` | 0 |
| anything else | 0, or −1 if it equals neither `[+0x18]` nor 0 |

So only `0` and `1` are meaningful: **1 = night, 0 = day**. (A value of `2`
returns −1 and the thread ignores it — harmless but pointless.)

### Evidence, both directions, mid-stream

Started `rtsp` by hand exactly as `config.sh` does it (`cd /mnt/Factory/custom &&
LD_LIBRARY_PATH=/mnt/Factory/custom/lib nohup ./rtsp >/tmp/rtsp.log 2>&1 &`) with
`/tmp/photo` holding `1\n`:

```
[photosensitive_switch_th_ex:324] Thread start, id: 2980
prev_state=-1, ir_val=1
now set to night
[isp_switch:152] switching isp mode -> night
```

No `not inited` line anywhere in the log — that message is gone for good now,
because `ak_drv_ir_init` finally finds both ircut nodes. Pins:
`IR_LED=1 ircut_a=0 ircut_b=0`. Frame `captures/night-isp-fit.jpg`: mean RGB
**116 116 116**, neutral monochrome, whiteboard text legible, exposure sane.
Compare the old hand-driven frame `captures/night-mode-frame.jpg` at **165 161
242** — violet and clipped. That gap is what the ISP scene switch buys.

Then `printf '0\n' > /tmp/photo` with the stream running:

```
prev_state=1, ir_val=0
now set to day
[isp_switch:152] switching isp mode -> day
```

Pins `IR_LED=0 ircut_a=1 ircut_b=0`; frame `captures/day-isp-fit.jpg`, mean RGB
**150 126 110** (warm, colour back, the red hearts are red). `rtsp` never dropped:
**69 frames / 4.974 s ≈ 13.9 fps** after the pair, against **50 frames / 4.940 s
≈ 10.1 fps** in night mode — longer exposure at night, as expected, with the
sensor's 15 fps ceiling unchanged.

`prev_state` starts at `-1`, so the first read after start always triggers a
switch. The mode is applied, not assumed.

### Why `/tmp` and not `/data`

`/tmp` is per-boot, so the board always comes up in **day** mode and nothing
silently persists. A missing `/tmp/photo` is *not* harmless, though — see
"Boot LED state" below: the read falls back to `/sys/kernel/ain/ain0` and
resolves to **night**, so `config.sh` now creates the file with `0` at boot. For a
night-default boot, put `echo 1 > /tmp/photo` in `config.sh` before the 30 s
sleep; the earlier `/data/photo` idea would have made the mode sticky across power
cycles, which is the wrong default for a camera nobody is watching.

### Caveats that are real

- **`ircut_a` is left at `1` all day.** `camera_set_ir()` (vendor code, untouched)
  writes `1` at day and `0` at night and never clears the other coil. If the
  mechanism really is the latching solenoid it behaves like, one coil stays
  energised for as long as the camera thinks it is day. Every stock YI unit with a
  working photo sensor does the same, so it is probably by design — but if the
  board gets warm near the lens, or the filter sticks, this is the first suspect,
  and `irctl`'s pulse-then-release style is the alternative.
- **Do not run `irctl` and `irnight` at once.** Both move the same filter; the
  auto thread re-asserts its idea of the mode every 5 s only on *change*, so a
  manual `irctl` flip can leave pins and ISP disagreeing. `tools/irnight` is the
  normal control; `irctl` is for poking pins by hand.
- **Which ISP block night uses is still unknown.** The `DV5_` blocks in
  `isp_sc1346.conf` (indices 0/1/2) remain plausible day/indoor/outdoor, but the
  log line is generic (`switching isp mode -> night`), so the mapping is decided
  inside `isp_switch` in `libplat_vi.so`. The picture is good; that is the only
  claim made here.
- **Untested in darkness.** Everything above ran in a lit room, where the
  illuminator is not actually needed. Range and noise at 1–3 m with the lights off
  are unmeasured.

## Boot LED state (host 2026-10-25 ~00:00–00:10, board clock 00:00–00:06)

After the user's power cycle (cold boot from `/mnt/Factory/config.sh`, chime
audible), the pins **one minute in** were:

| pin | value | who |
|---|---|---|
| `IR_LED` | **1** | driver default, then re-written by the day/night thread |
| `WHITE_LED` | 0 | `service.sh` → `close_white_led()` (off) |
| `ircut_a` | 0 | driver default; night write is also 0 |
| `ircut_b` | 0 | untouched |
| `SPK_PA` | 1 | amp muted (active-low) |

Two separate contributors, both verified:

1. **The GPIO default is `IR_LED=1`.** At +1 min the pin read 1 while its sysfs
   attribute mtime was still `00:00`, i.e. no userspace write had happened yet.
   This is the "stock leaves `IR_LED=1` at boot" observation from before the
   patch, and it is why the illuminator is on the moment the kernel comes up.
2. **The patched day/night thread then decides "night".** At board `00:02` both
   `IR_LED` and `ircut_a` mtimes moved to `00:02` while `WHITE_LED`, `ircut_b`
   and `SPK_PA` stayed `00:00` — the only code that writes `IR_LED` *and*
   `ircut_a` together is `ak_misc_set_video_day_night()`. (The sysfs attribute
   mtimes on this kernel do track writes: my own writes at `00:04:42` moved them
   to `00:04`, a concurrent `led on` moved `WHITE_LED` to `00:05`.)

**Why it thinks it is night with no `/tmp/photo`:** `ak_drv_ir_get_input_level`
tries `/tmp/photo` first and, when `fopen` fails, falls back to the second path
hardcoded in `libplat_drv.so`, `/sys/kernel/ain/ain0`. That AIN is **not a light
sensor** — it reads a constant **1171** here (`ain1`=430, `ain/bat`=3389; an ADC
mux, while the real photo input is the GPIO the stock kernel configures as −1),
so the level resolves to a fixed `1` = night. Reproduced on demand: `rm -f
/tmp/photo` → within 10 s `IR_LED=1`, `ircut_a=0`; `printf '0\n' > /tmp/photo` →
within 10 s `IR_LED=0`, `ircut_a=1`, ISP day. The thread is alive on a cold boot
(rtsp pid 700 had a `PS_switch` task), it was never "idle".

Timed probe at board `00:07:51` (day, `IR=0 A=1`), file deleted at `00:07:52`, no
file ever recreated:

```
00:07:52 photo=MISSING IR=1 A=1     <- IRLED written first, as in the vendor code
00:07:55 photo=MISSING IR=1 A=0     <- then ircut_a: full night switch, no file
   ...    photo=MISSING IR=1 A=0     stable night, forever, off a dead ADC
```

`ircut_a` moving is the tell: only `ak_misc_set_video_day_night()` writes it, the
shell never does. So a missing file does **not** mean "thread does nothing" (an
earlier version of this note claimed that) — it means **night**, from the AIN.

**The visible window at cold boot.** `IR_LED` reads 1 from the moment the
user-gpio driver registers until `config.sh` reaches the guard, because the
driver's inherited default is 1 and nothing in the stock boot path writes
`IR_LED` at all. Measured on the `00:00` cycle: at board `00:01:11` (≈ +60 s)
`IR_LED` was still 1 while its mtime was still `00:00`, i.e. zero userspace
writes so far; `config.sh` only starts after `rcS → rc.local → camera.sh setup`
(module loads, flash mounts) and `service.sh` (`cmd_serverd`, `close_white_led`,
`killall telnetd`, `printk 3`), so the guard lands around **+60…80 s**. That
whole stretch is stock boot on read-only squashfs — not reachable without flash,
so the window stays; it shrinks from *forever* (pre-patch) to *stock boot only*.
After the next power cycle `ls -l /sys/user-gpio/IR_LED` gives the guard's write
time and `uptime` the boot age, which measures the window exactly.

**What the user actually sees:** the only pin that is high at boot is `IR_LED`,
the IR illuminator — a ring of LEDs whose deep-red glow is visible. The user has
since confirmed the boot-time complaint is that ring, **not** the white lamp.
`WHITE_LED` was 0 throughout boot; `service.sh`'s `close_white_led()` is the only
stock writer and it writes 0 (`/etc/jffs2/hw.conf` byte 24 = `0`, the
non-inverted branch). There is **no** `/sys/class/leds` on this box: all LEDs are
the five `/sys/user-gpio` pins, and `led_status.sh` (which would drive
`AP_LED`/`PWR_LED`) never runs in Factory mode and refers to pins that do not
exist here.

Separately, at board `00:06:20–00:06:5x` the **white** lamp *was* lit
(`WHITE_LED=1`, `IR_LED=0`, `SPK_PA` held at 0) and the user reported "the LED is
on atm … oh it just turned off" — that window is a `quiet-cue.sh` cue from the
audio work (`$LED on` … `led off` around a play), not a boot path and not a
day/night path. Two useful facts fall out: `WHITE_LED=1` is **on** (active-high,
the opposite of `SPK_PA`), and any interrupted cue leaves it lit until something
writes 0 — which the new boot guard also does.

**One more boot-time LED write, ours:** the startup chime runs through
`quiet-cue.sh`, which does `$LED on` before `ak_adec_demo` and `$LED off` after
(`rust/led` → `WHITE_LED`). That is a ~0.4 s **white** flash at boot, and if that
`ak_adec_demo` call ever hangs or the script is killed mid-play the pin stays on.
On this cold boot it cleaned up (WHITE_LED read 0 at +1 min and +2 min), but if a
white LED is still seen after the fix, that cue is the first suspect — it is the
audio agent's script, not this guard.

**Fix (SD-card only, no flash):** the first lines of `/mnt/Factory/config.sh`
after the `LD_LIBRARY_PATH` export now read

```sh
printf '0\n' > /tmp/photo
echo 0 > /sys/user-gpio/IR_LED
echo 0 > /sys/user-gpio/WHITE_LED
```

The `/tmp/photo` write is the real fix: it makes the thread perform a genuine
day switch (`IR_LED=0`, `ircut_a=1`, ISP day) about 5 s after rtsp starts and
then hold, instead of latching night off the dead AIN. The two `echo`s are
belt-and-braces for the window before rtsp starts; `ircut_a`/`ircut_b` are
deliberately not touched (latching solenoid, no pin memory).

Consequence to remember: the AIN fallback is now permanently out of play, so
**auto** day/night no longer happens at all — night is `tools/irnight night`.
That is the intended trade: the AIN was never a light sensor, so "auto" only ever
meant "always night".

Verified by hand at board `00:04–00:09` (no reboot): guard applied → `t+10 s`
`IR_LED=0 WHITE_LED=0 ircut_a=1`, still there at `t+28 s`; `ircut_a` 0→1 is the
proof the *thread* switched, since the script never writes that pin. Stream
unaffected (`tools/snap` → `work/video/captures/post-ledoff.jpg` 30 245 B and
`post-ledoff-day.jpg` 31 784 B, RGB 83/123/91 and 138/136/145, normal colour
frames). `tools/irnight` still drives it afterwards: `irnight night` →
`photo=1 IR=1 A=0`, back to `irnight day` → `photo=0 IR=0 A=1` within 8 s, so the
manual control survives the change and nothing pins the board to day.

Files: `config.sh` 905 B `0d97c887…` → 1603 B `c0d13ddb…` (md5 `a7855b69…`,
mode 0755, `sh -n` clean), old copy kept on the card as
`/mnt/Factory/config.before-ledoff.sh` = `0d97c887…`.

**Unproven:** the real cold boot. Only the user power-cycles this board, so the
next power cycle is the test. One-liner from the host:

```sh
tools/cam 'uptime; echo photo=$(cat /tmp/photo 2>/dev/null || echo MISSING); for g in IR_LED WHITE_LED ircut_a ircut_b; do printf "%s=%s " $g $(cat /sys/user-gpio/$g); done; echo; ls -l /sys/user-gpio/IR_LED | awk "{print \$6,\$7,\$8}"; ps | grep -c "[r]tsp"'
tools/snap work/video/captures/coldboot.jpg   # then: ffmpeg -i ... -vf scale=1:1 -f rawvideo -pix_fmt rgb24 -
```

Pass = `photo=0`, `IR_LED=0`, `WHITE_LED=0`, `ircut_a=1`, `rtsp=1`, a colour frame
(not violet, not IR-bright) — and no red glow at the lens once the guard has run
(compare the `IR_LED` mtime with `uptime` to see when that was).

Two tooling facts found on the way: `service.sh` runs `echo 3 >
/proc/sys/kernel/printk` on every boot (no `/backup/logon`), so `ak_print` lines
never reach `dmesg`; and busybox `dmesg` on this kernel *blocks* (streams
`/proc/kmsg`) and wedges the telnet session — don't call it over `tools/cam`. Use
`ls /proc/PID/task/*/comm` to see whether `PS_switch` exists instead.

## Cold boot, measured: the guard holds (host 2026-10-25 10:37, board 00:15:29)

The first boot observed live instead of sampled afterwards. `sync; reboot -f` from a
telnet shell (instructed; no hand on the plug). Board clock restarts from ~00:00 —
no RTC — so uptime is the only time base.

| uptime | observation |
|---|---|
| ~31 s | `telnetd -p 2323` up, so `config.sh` had already run its guard lines |
| 41 s → 145 s | 45 samples, every 2 s: `photo=0 IR_LED=0 WHITE_LED=0 ircut_a=1 SPK_PA=1` — **not one deviation** |
| 4 min | `rtsp` pid 683, thread list includes **`PS_switch`**; still day |

So the two contributors from the section above are both addressed: the guard writes
`/tmp/photo=0` + `IR_LED=0`/`WHITE_LED=0` before the 5 s thread gets its first turn,
and the thread — being alive and holding day for 145 s — never runs the
`ain0`(1171)=night fallback that lit the illuminator on 10-24. Cold boot ends in a
colour day image (`work/video/captures/coldboot-verified.jpg`, RGB 128/144/152), and
the stream runs 14.4 fps at 720p straight out of `config.sh`.

**What is still not covered:** the ~first 30 s. Nothing was sampled before 31 s
(kernel + squashfs rootfs, SD not yet mounted), so a brief flash of the stock
`IR_LED=1` default in that sub-30 s window cannot be ruled out from the card. Only
the serial console (`ttySAK0` 115200) could settle it. This also retires the
"60–80 s unreachable window" figure written earlier — it is ~30 s, and it is where
telnet first answers, not where the glow appeared.

**Two measurement traps this boot exposed**, both now in `analysis/startup.md`:

* `dmesg | grep "not inited"` is useless here — `printk` is `3 4 1 7`, so `ak_print`
  never reaches dmesg. Absence of that line proves nothing; presence of `PS_switch`
  does.
* `df /mnt` reports a shadowed fstab tmpfs (31 GB, 0 %), not the exFAT card mounted
  over it. `/mnt`, `/mnt/Factory` and the binaries all sit on `dev=b301` = 179:1.
* sysfs attribute mtimes on `/sys/user-gpio/*` read `00:00` for everything, boot
  writes included, so they cannot be used to attribute a write on this board.
