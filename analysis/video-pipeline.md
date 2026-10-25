# Video pipeline — SOLVED: live H.264 + audio, 2026-10-24

**Status: video works.** First pixels ever out of this board, in Factory mode,
with no flash writes and no reboot.

    rtsp://192.168.1.103:554/vs0    1280x720 H.264 ~14.3 fps + 8 kHz PCM a-law
    rtsp://192.168.1.103:554/vs1     640x480 H.264 ~13.8 fps + 8 kHz PCM a-law

Frame rate, *observed* rather than declared: the SDP offers `r_frame_rate=15/1`
and the encoder is configured for 25, but counting frames gives 71 in 5.00 s on
`/vs0` and 69 on `/vs1` (~14 fps). The ISP logs `set sensor fps: 15`, so the
sensor is the ceiling, not the encoder.

Evidence in `work/video/captures/`: `main-first-frame.jpg` (a real image),
`main-5s.mp4`, `sub-5s.mp4`, `main-5s-audio.mkv` (mean −51 dB, max −17.8 dB in a
quiet room). Verified with `ffprobe`/`ffmpeg` from firefly, over TCP interleaved.

**Survives a reboot.** After a power cycle the fix is still in place and
`config.sh`'s own delayed `rtsp &` brings streaming up unattended: 20 s pull
straight after boot, 284 frames in 19.84 s = 14.3 fps at 970 kbit/s
(`main-20s.mp4`). Nothing has to be started by hand any more.

Related: day/night and the IR-cut filter are covered in `ir-daynight.md` — the
driver path is dead, the GPIOs are not.

## Root cause: one byte, read as a raw byte, compared to one byte

`libplat_vi.so` wanted a *style id* and compared it with the style byte inside
`isp_sc1346.conf`. Both sides were fine. The mistake was in the file meant to
carry the wanted value:

* `isp_cfg_file_get_style_id()` does `fopen("/etc/jffs2/styleId","rb")` then
  `fread(dst, 1, 1, f)` — it reads **one raw byte**. It parses nothing.
  `isp_cfg_file_set_style_id()` likewise writes one raw byte.
* A missing file leaves the destination at 0, i.e. style **0**.
* `check_data.constprop.3` compares that byte with header byte **0x20** of the
  config block, which is 0.

So the hand-made `styleId` files — `0\n`, then `48\n` — were read as `0x30` (48)
and `0x34` (**'4' = 52**), which is exactly the `style_id:52, 0` mismatch in the
old logs. Nothing was wrong with the ISP config, the sensor id, or the jffs2
space problem; one text-vs-binary byte was.

**Fix, and current state: `/etc/jffs2/styleId` does not exist.** That is also the
stock state (mtd7 never had one), so it is the faithful configuration, not a
workaround. If anything ever needs to set a non-zero style, it must be one raw
byte (`printf '\0xx'`), never `echo N`.

The symlink `/etc/jffs2/isp_sc1346.conf` →
`/mnt/Factory/custom/sensor/isp_sc1346.conf` stays: the 104 KB conf still cannot
live in a 64 KB jffs2 with ~12 KB free, and `libplat_vi.so` still hardcodes
`/etc/jffs2/`.

## The second wall behind the first one: it needs more than 8 seconds

With the header matching, the pipeline comes up in stages and looks broken
half-way through:

    [isp.conf]version: DV5_092602, sensor id: 0x1346, style id: 0
    [check_file:347] check isp cfg: /etc/jffs2/isp_sc1346.conf OK
    [ak_vi_open:890] register device, dev type: 0
    [libakuio]ION: alloc 25165824 bytes          <- 24 MiB PMEM, 64 MiB board
    ispsdk_lib version:V3.1.00
    sensor resolution height:720, width:1280.
    [ak_vi_get_frame:1373] get frame failed      <- 4 of these, then silence
    [capture_thread:1416] start to capture
    [start_service_work:1783] service start

Those four `get frame failed` lines at start-up are **normal and transient**;
after them capture runs continuously at the sensor's ~14 fps. An 8-second test killed it
in exactly that window and looked like a dead end. Wait at least 15 s, and read
`grep -c "get frame failed"` (4) rather than trusting a short tail.

Because `/mnt/Factory/config.sh` already starts `/mnt/Factory/custom/rtsp` after
`sleep 30`, **the camera will come up with video on the next boot with no
further changes.** Nothing new is on the flash path; the only persistent change
is the deletion of one 3-byte file in jffs2.

## The container format (recovered, and now tooled)

`isp_<sensor>.conf` is a sequence of fixed-size blocks: a 512-byte header plus a
module payload. `sc1346`'s file is 104,574 bytes = 3 blocks × 34,858 bytes
(`0x882a`), each with a 34,346-byte payload.

| off | size | field |
|---:|---:|---|
| 0x00 | 4 | version, must be 3 |
| 0x04 | 16 | variant name, `DV5_092602` |
| 0x14 | 4 | sensor id `0x1346` |
| 0x18 | 2 | build year |
| 0x1a | 5 | month, day, hour, minute, second |
| 0x1f | 1 | **block index** 0–4 (not a style id) |
| 0x20 | 1 | **style id** 0–255 — the byte `styleId` must equal |
| 0x21 | 3 | zero |
| 0x24 | 4 | block size, header included |
| 0x200 | — | payload |

The three blocks are built 2024-04-09 (index 0), 2024-04-01 (index 1) and
2022-12-12 (index 2), all sensor `0x1346`, all style 0.

Two traps in that table's consequences:

* `isp_cfg_file_load(0, …)` loads the payload of the block whose **index byte is
  0**. That is the live main-path config; 1 and 2 are alternate modes and are
  parsed by `check_data` but never loaded by this path.
* The `[isp.conf]modify time: 2022-12-12 14:27:23` line in the log is the **last**
  block's date, not the loaded block's (2024-04-09). It is a leaked loop
  variable. Do not read it as "the old config is being used" — a plausible and
  completely wrong conclusion.

`tools/rust/ispconf` now does this parsing and reproduces `check_data`'s rules;
`ispconf check ... --sensor "$(cat /sys/ak_info_dump/sensor_id)"` validates the
deployed config against the live sensor on the board. Deployed at
`/mnt/Factory/custom/ispconf` (ARMv5TE, 401,416 bytes).

## How it was found

`libplat_vi.so` ships **unstripped with DWARF** (`with debug_info, not stripped`),
so the check logic survives intact. Host binutils has no ARM support; use
`llvm-objdump`:

    llvm-objdump -d --triple=armv5te --start-address=0x8b1c --stop-address=0x8f68 libplat_vi.so

| symbol | address | role |
|---|---|---|
| `check_data.constprop.3` | `0x8b1c` | version/sensor/style/date checks, builds the block-index → offset table |
| `isp_cfg_file_load` | `0x9044` | reads sensor id from `/sys/ak_info_dump/sensor_id`, calls `check_data` |
| `isp_cfg_file_get_style_id` | `0x8fa0` | the one-byte `styleId` read |
| `isp_cfg_file_set_style_id` | `0x8ff0` | the one-byte `styleId` write |
| `isp_main_match_sensor_cfgfile` | `0x9530` | `calloc(1, 204800)`, picks the conf, loads it |
| `ak_vi_match_sensor` | `0x4f10` | `opendir` the config dir, `strstr` for `isp_`/`.conf`, `do_match` |

The version string of `libplat_vi.so` analysed here is from the deployed card;
strings used: `[%s:%d] %s is not need file!` (0xd075) and
`[%s:%d] sensor_id:0x%x, 0x%x, style_id:%d, %d` (0xd094).

## Incident: the board left the network 22:35, 2026-10-24 — unresolved

About six minutes of successful streaming (several clients on `/vs0` and `/vs1`,
five captures, never more than one at a time) ended here:

    22:31  main-5s.mp4, sub-5s.mp4, main-5s-audio.mkv all pull cleanly
    22:33  ispconf deployed (401 KB via nc), runs on board, check passes
    22:34  ispconf redeployed (hash corrected), sha256 + inspect OK
    22:35:20  next ffmpeg pull: "No route to host"
    22:35:37 → 22:40:16  ping 100% loss, :2323 unreachable, no ARP entry

It never came back on its own in 4.5 minutes, which is longer than a reboot would
take (telnetd is up well before the 30 s `config.sh` sleep), so this needs a
physical power cycle. Nothing was written to flash at that moment — the last
jffs2 write was the `styleId` deletion ~6 minutes earlier, which the running
system then read fine for six minutes.

Leading hypotheses, in order, none confirmed:

1. **Memory pressure.** 24 MiB ION reserved out of 36 MB total leaves ~11 MB free
   while streaming two encoder groups. Each new RTSP client re-requests encoder
   streams (`ak_venc_request_stream`, `req_ref`), so rapid client churn is the
   obvious way to push it over. An OOM kill of `wpa_supplicant` would take the
   board off the air while leaving it alive — indistinguishable from a hang by
   ping alone.
2. The ATBM6031x Wi-Fi driver or the Anyka VI driver wedged under sustained
   capture.
3. Something unrelated in the Wi-Fi AP.

**First things to do after the power cycle**, all read-only:

    tools/cam 'uptime'                      # uptime reset => it rebooted itself
    tools/cam 'dmesg | tail -40'            # look for OOM, atbm, ak_vi
    tools/cam 'dmesg | grep -iE "oom|kill"'
    tools/cam 'ps | grep -c wpa'            # survived?

Until then, treat single sustained pulls as safe (proven for 6 minutes) and
treat rapid connect/disconnect cycling as untested and suspect.

**After the power cycle (22:45):** `uptime` back to 1 minute, so it was a fresh
boot rather than a device that had merely lost Wi-Fi and recovered. `dmesg` has
**no** `oom`, `out of memory` or `killed process` lines — but the kernel ring
buffer is cleared by exactly the reboot needed to reach it, so this is **not**
evidence against the OOM hypothesis; it is an inability to test it. `rtsp` had
restarted itself via `config.sh`'s 30 s delayed `rtsp &` line and was streaming
again with `MemFree` 12.6 MB, load 1.59. A 20 s pull then ran clean:
`main-20s.mp4`, 284 frames in 19.84 s (14.3 fps), 970 kbit/s — recorded in
`work/video/captures/`.

Steady-state memory after a reboot plus four separate client sessions is flat:
`Mem:` 25,380 used / 10,972 free with 1,628 buffers and 9,844 cached, i.e.
**13,908 kB really in use**, against 13,988 kB measured three sessions earlier.
So there is no per-session leak in the connect/disconnect path — one of the
cheap versions of hypothesis 1 is now ruled out. (Read `free` with care on this
board: the `Mem:` free column swings by several MB with page cache and means
nothing; the `-/+ buffers/cache` used figure is the one to compare, and that is
the number to quote, not the headline `MemFree`.)

The incident stays **unresolved**. To make it testable next time, arm the ring
buffer before stressing the board:

    tools/cam 'dmesg -n 8; dmesg -c > /tmp/oom-watch.log'   # clear, then watch
    tools/cam 'grep -ciE "oom|killed process" /tmp/oom-watch.log'

and note that anything still only in the ring buffer dies with the next reboot.

## Remaining rough edges (none of them block video)

* ~~`[ak_drv_ir_get_input_level:129] not inited.`~~ — the driver's IR path is
  dead (photo-sensor GPIO is `-1`), but the filter and illuminator are drivable
  through `/sys/user-gpio/{ircut_a,ircut_b,IR_LED}` and now tooled as `irctl`.
  The hardware switch is verified; night images are still violet because the ISP
  stays in day mode. See `ir-daynight.md`.
* Startup produces 4 `get frame failed` lines. Harmless.
* The 24 MiB ION reservation on a 36 MB `free` total leaves ~11 MB free while
  streaming; load average ~1.3. Nothing is swapping (there is no swap).
* Audio is 8 kHz mono a-law with the usual ~360 ms dropped at start.
* ONVIF (`/mnt/Factory/custom/onvif`, 230,406 bytes) **not tried, deliberately**.
  It is the vendor demo (`ak_onvif_demo.c`, "onvif demo V1.0.01"): it carries
  its own motion-detection XML, `set_net_param_to_file`/`set_video_param_to_file`
  and `cp %s %s` config plumbing, and it registers its own encoder stream
  handlers (`register video handle for onvif failed`). A full ONVIF device
  normally owns port 554 with its own RTSP server, so starting it next to the
  working `rtsp` is a good way to lose the stream that took all day. Test it as
  a unit — stop `rtsp` first, run `onvif` alone, put `rtsp` back — never as an
  addition, and never while another agent is using the board.
* RTSP is plain and open, so anything on the LAN can view it. Same trust level
  as the root telnet shell on 2323 — trusted LAN only.
* Frame is mirrored/rotated relative to the physical mount (a portrait whiteboard
  in a landscape frame). That is mounting/`flip-mirror`, not a pipeline fault;
  `Ak_ISP_Set_Flip_Mirror` is available if it ever matters.

## Reproduce from scratch

    tools/cam 'rm -f /etc/jffs2/styleId'
    tools/cam 'cd /mnt/Factory/custom && LD_LIBRARY_PATH=$PWD/lib ./rtsp > /tmp/rtsp.log 2>&1 &'
    sleep 15
    ffprobe -rtsp_transport tcp rtsp://192.168.1.103:554/vs0
    ffmpeg  -rtsp_transport tcp -i rtsp://192.168.1.103:554/vs0 -frames:v 1 out.jpg

## Original investigation, kept for the record (2026-10-24, earlier)

Before the style-byte discovery the state was: `anyka_ipc` never runs in Factory
mode by design (`service.sh` sees `/mnt/Factory` and runs `config.sh` instead);
`libplat_vi.so` hardcodes `/etc/jffs2/` and wants a 104 KB `isp_<sensor>.conf`
plus a `styleId`, while `/etc/jffs2` is 64 KB with ~12 KB free; the conf was
moved to the SD card and symlinked in; the sensor id matched but `check_data`
reported a style mismatch and `check_file` said "read sensor fail". Two candidate
next steps were written down — patch the header's style-id field (layout unknown)
or try `anyka_ipc` instead. Neither was needed: the layout is in *this* file now,
and the mismatch was on the `styleId` side, not in the conf.

Servers: 21 ftpd, 2323 telnetd root shell, 554 rtsp (new), 8080 httpd
(`web_interface/www`, no video), 8782 `cmd_serverd` (loopback only).

## The stream, measured from our own client (2026-10-25)

`tools/rust/camweb/src/rtsp.rs` is a pure-std RTSP/RTP-over-TCP client written for
this camera; it makes the same numbers available without ffmpeg in the path. On
`rtsp://192.168.1.103:554/vs0`, 120 s continuous:

* SDP offers **H.264 (clock 90000, `sprop-parameter-sets` 24 B SPS + 5 B PPS)** and
  **PCMA 8000 Hz**; only the video track is interleaved (channels 0/1) when it is
  the sole track set up. `Public: SETUP,OPTIONS,DESCRIBE,PLAY,TEARDOWN,GET_PARAMETER,SET_PARAMETER`,
  **`Session: …; timeout=60`** — mid-stream `OPTIONS` answers in ~90 ms and
  keepalives every 20 s kept a 120 s session alive.
* **14.7 access units/s** (RTP marker bit), **15.3 NAL/s**, 5.9 KB/s×10 ≈ **475 kbit/s**,
  NAL sizes 7 B … 60 KB.
* SPS **and** PPS are repeated **in-band at every IDR** (36 of each in 120 s), so a
  client that joins mid-GOP recovers within one keyframe interval (~3 s); the SDP
  copies are still worth sending first.
* The stream itself is **not** a clean 25 fps: the encoder is configured for 25 and
  delivers ~14.7 in daylight (~10 at night), which is why every fps claim in these
  notes is a measurement, not the SDP.

That client now feeds the browser panel (`tools/camweb serve`, see
`tools/README.md` → "Live panel"), where ffmpeg is only the H.264→JPEG decoder.

### What the server demands and what it will not do

Established while writing that client (full detail, including the fake-server test
matrix, in `work/rtsp-rust-notes.md`):

* **An nginx/1.17.8 sits in front of `ak_rtsp_demo`.** Every 200-class answer
  carries no `Server` header; the 404s do. Because nginx only routes request
  shapes it knows, **only the TCP-interleaved `SETUP` form works** — the classic
  UDP `Transport: RTP/AVP;unicast;client_port=…` returns `404`, which is the
  usual reason a stock RTSP client (VLC in UDP mode, gstreamer defaults) shows
  nothing on this camera. Interleaved channel numbers *chosen by the client* are
  honoured verbatim.
* **`DESCRIBE` works as the very first request** — no `Accept:`, no `CSeq`
  ordering needed. What is mandatory is the full URL (`OPTIONS *` → connection
  reset by peer) and the request-line version (`… RTSP/1.0`; omitting it → reset).
  There is **no authentication** on 554.
* `Session:` comes back as `"<id>; timeout=60"` in one header, so the id has to be
  split off; ids are a per-connection counter (`0000001A`, `00000031`, …).
* **The advertised `timeout=60` is not enforced**: 130 s with zero requests kept
  streaming at full rate. Keepalives are insurance here, not a requirement — and
  a mid-stream `OPTIONS` is occasionally never answered (1 in 10). Waiting 3 s for
  that reply caused a real 3.000 s video stall, so the client waits ≤400 ms and
  treats a miss as success.
* **There is no AUD and no STAP-A**, so an access unit cannot be identified by NAL
  type. Frames are counted with the **RTP marker bit**; every IDR is preceded by
  in-band SPS+PPS (35/35, 36/36 observed) and the stream opens `SPS, PPS, IDR`
  with the first IDR 70–123 ms after `PLAY`. RTCP (SR) arrives on the odd channel
  and also starts `0x81`, so demultiplex **on channel first** — otherwise SR
  packets are fed to the H.264 depacketiser as garbage NALs.
* SDP `o=` still names the old address `192.168.1.79` while `c=` says `.103`;
  both are ignored in favour of the URL actually dialled.

### The stock server is multi-session (2026-10-25, evening)

Checked because the plan is to run our own RTSP server *on* the camera and feed it
by pulling from the stock one on loopback: if the stock `ak_rtsp_demo` allowed a
single client, that design would have broken firefly's live view. It does not.

Two concurrent ffmpeg pulls from firefly, deliberately overlapping (B starts 1 s
after A, both `-rtsp_transport tcp`, 9–10 s windows):

| client | decoded frames | bytes | size |
|---|---|---|---|
| A | 171 | 355,580 | 1280x720 |
| B | 175 | 360,818 | 1280x720 |

Both got a full-rate stream, so sessions are independent and adding a second
consumer on the board costs the encoder another client, not the stream. Two
caveats worth remembering: the *encoder* is still single-owner (only `rtsp` opens
`/dev/video0`), and this is a driver-level observation of frames arriving, not a
measurement of what the sensor/ISP dropped under the extra load.

### The SPS in our own code (2026-10-25)

`camrtsp`'s `media::sps_dimensions` reported the 1280x720 stream as **16x512**,
which we had papered over with a plausibility guard in the source builder. Two
bugs, one behind the other:

1. the bit reader ran over the raw EBSP, so the emulation-prevention bytes
   (`00 00 03 xx`, two of them in this 24-byte SPS) shifted every field by a byte;
   `to_rbsp()` now strips only real escapes (`00 00 03` followed by `00..03`);
2. the profile field was read as `b.bit()? * 0 + { 8 bits }`, which consumed one
   bit and discarded it, so *every* `ue`/`se` after it decoded one bit late. The
   profile, constraint flags and level are byte fields: the bit reader now starts
   at `rbsp[4]`.

Ground truth came from `ffmpeg -bsf:v trace_headers` on `/tmp/probe-vs0.h264`:
profile 77 (Main), level 40, `log2_max_frame_num_minus4=12`, `pic_order_cnt_type=2`,
`max_num_ref_frames=1`, `pic_width_in_mbs_minus1=79`, `pic_height_in_map_units_minus1=44`,
`frame_mbs_only=1`, no cropping → exactly 1280x720. That SPS is now a unit test, so
the parser cannot silently drift again.
