# Encoder ownership: can we replace the stock `rtsp` binary?

Recon for the plan "a small C shim owns VI + VENC and writes frames to a unix
socket in our own format, so a pure-Rust server is the only network-facing
process". Written 2026-11-08 on `venc-toolchain` off `integ`. Nothing was
written to the camera; every device fact below comes from the extracted
stock filesystems in `analysis/` or from `analysis/video-pipeline.md` and
`analysis/ir-daynight.md`, which were measured on the board.

**Verdict: it is feasible, and the toolchain was obtained and exercised today.**
`arm-anykav200-crosstool.zip` (45 MB, GitHub) is gcc 4.8.5 + binutils 2.24 +
uClibc **0.9.33.2** — the exact triplet the device was built with — and a
program linked with it against the board's own `libmpi_venc.so` /
`libplat_vi.so` comes out with `Flags: 0x5000202, Version5 EABI, soft-float
ABI`, `NEEDED … libc.so.0` and interpreter `/lib/ld-uClibc.so.0`, i.e.
byte-for-byte the same ABI class as `rtsp.stock`. See §1.2 for the link that
proves it. What is *not* free: the shim has to reproduce the encoder's
two-channel setup, its keyframe behaviour and the day/night work that currently
lives in four patched string slots, and audio would come with it if we ever want
sound back.

---

## 1. The toolchain

### 1.1 What the device actually requires (measured, not guessed)

| property | value | evidence |
|---|---|---|
| CPU | ARM926EJ-S, `Tag_CPU_arch: v5TEJ` | `readelf -A analysis/base-rootfs/lib/libuClibc-0.9.33.2.so` |
| ELF flags | `0x5000202` = ARM EABI **v5**, **soft-float** ABI | same, and `readelf -h Factory/custom/rtsp` |
| loader | `/lib/ld-uClibc.so.0`, SONAME `ld-uClibc.so.0` | `readelf -l/-d` on `rtsp`, on `analysis/base-rootfs/lib/ld-uClibc-0.9.33.2.so` |
| libc | uClibc **0.9.33.2**, SONAME `libc.so.0` | `analysis/base-rootfs/lib/libuClibc-0.9.33.2.so`, `readelf -d` |
| symbol versioning | **none**: `readelf -V` → "No version information found in this file" | same file |
| threads | `__pthread_mutex_lock` … exported from libc; `libpthread-0.9.33.2.so` present | `strings`, `ls analysis/base-rootfs/lib` |
| C++ runtime (if ever needed) | `libstdc++.so.6.0.19` = gcc 4.8.5 | `analysis/base-rootfs/lib` |
| headers | the vendor headers in `src/rtsp/include/` are the ones the stock app was compiled against; a shim needs no kernel headers, only these plus libc | `src/rtsp/ak_rtsp_demo.c` compiles against them unmodified |

Two consequences that matter more than the version numbers:

* **No symbol versioning** means the dynamic loader cannot detect a libc
  mismatch. A wrong uClibc links happily and then corrupts memory somewhere
  later. There is no "it refused to start" safety net — only crashes that look
  like our bugs.
* **The loader and the libc are a pair.** `ld-uClibc.so.0` and `libc.so.0` share
  private structures (`struct link_map`, TLS block layout). We do *not* control
  `/lib/ld-uClibc.so.0`, because that is in the read-only squashfs and we are not
  flashing. Therefore the toolchain has to be the **same uClibc 0.9.33.2**, not
  merely "uClibc".

The vendor SDK confirms the name of the thing: the kernel in the AK3918 sample
logs was built with `gcc version 4.8.5 (anyka (gcc-4.8.5 + binutils-2.24 +
ulcibc-0.9.33.2)(20170223))`
(<https://github.com/ricardojlrufino/arm-anykav200-crosstool>), and the same
build string appears in our own card's `libmpi_venc.so`
(`/home/wumingjin/buildroot/trunk/buildroot-2015.11.1/output/build/host-gcc-final-4.8.5/build/arm-anykav200-linux-uclibcgnueabi/libgcc`,
`strings Factory/custom/lib/libmpi_venc.so`). So: **`arm-anykav200-linux-uclibcgnueabi`,
gcc 4.8.5, uClibc 0.9.33.2, soft-float, arm926ej-s** — and this camera's
firmware was built with buildroot **2015.11.1**.

### 1.2 Lead A — obtainable, and verified working on firefly today

<https://github.com/ricardojlrufino/arm-anykav200-crosstool> (public, 13 stars,
created 2022-02-20)

| | |
|---|---|
| file | `arm-anykav200-crosstool.zip`, **45,217,158 B** (stored, no compression) |
| also | `arm-anykav200-gdb-7.12.tar.gz`, 13,542,074 B |
| contents | 2,546 files / 129,264,447 B unpacked; prefix `arm-anykav200-linux-uclibcgnueabi-`; gcc 4.8.5, binutils 2.24, gdb, fakeroot |
| sysroot | `usr/arm-anykav200-linux-uclibcgnueabi/sysroot`, **1,160 files** — uClibc **headers** (`sysroot/usr/include`) *and* the **shared stubs**: `libc.so.0 → libuClibc-0.9.33.2.so`, `libm.so.0`, `libpthread.so.0`, `ld-uClibc-0.9.33.2.so` (28,631 B) with `ld-uClibc.so.0` symlink |
| configured | `--with-cpu=arm926ej-s --with-float=soft --with-abi=aapcs-linux --with-mode=arm --enable-tls --disable-multilib`, `'anyka (gcc-4.8.5 + binutils-2.24 + ulcibc-0.9.33.2)(20170223)'` |
| host binaries | **i386** ELF (`toolchain-wrapper: ELF 32-bit LSB … Intel i386`), so an x86_64 host needs 32-bit runtime; on firefly they run as-is |
| licence | **none declared** — the GitHub API returns `"license": null`. It is a redistribution of gcc/binutils/uClibc (GPL) built by Anyka. Using it locally is one thing; **do not vendor it into this repo, the fork, or a CI image**. Record the URL, not the tarball. |

**The proof.** Downloaded, unpacked to `/tmp/xc`, and used to build a shim-shaped
program against the *deployed* vendor libraries — no camera involved:

```sh
TC=/tmp/xc/arm-anykav200-crosstool/usr/bin
export LD_LIBRARY_PATH=/tmp/xc/arm-anykav200-crosstool/usr/lib   # see gotcha below
$TC/arm-anykav200-linux-uclibcgnueabi-gcc -O2 -o shim-arm shim.c \
  -I src/rtsp/include/libplat/include -I src/rtsp/include/libmpi/include \
  -L Factory/custom/lib -L src/rtsp/lib \
  -Wl,--start-group -lmpi_venc -lplat_vi -lplat_common -lplat_thread \
      -lplat_vpss -lplat_ipcsrv -lakispsdk -lakuio -lakstreamenc -lakmedia \
  -Wl,--end-group
```

`shim.c` did nothing but declare the handles and call
`ak_vi_match_sensor → ak_vi_open → ak_venc_open(struct encode_param) →
ak_venc_request_stream → ak_venc_get_stream/release_stream`. Result:

```
shim-arm  6,361 B
Flags: 0x5000202, Version5 EABI, soft-float ABI   (identical to rtsp.stock)
Tag_CPU_name: "ARM926EJ-S"   Tag_CPU_arch: v5TEJ
NEEDED: libmpi_venc.so libplat_vi.so libplat_common.so libplat_thread.so
        libplat_vpss.so libplat_ipcsrv.so libakispsdk.so libakuio.so
        libakstreamenc.so libakmedia.so libc.so.0
[Requesting program interpreter: /lib/ld-uClibc.so.0]
```

Three gotchas found while doing it, all cheap but none obvious:

1. **`cc1` dies with `libmpc.so.3: cannot open shared object file`.** The
   toolchain ships `libmpc.so.3`, `libmpfr.so.4`, `libgmp.so.10` in its own
   `usr/lib` but the wrapper does not point at them. `LD_LIBRARY_PATH=` that
   directory, or patch the wrapper.
2. **`-Wl,--start-group/--end-group` is required.** The vendor `.so`s are not
   linked `--no-undefined`: `libplat_vi.so` has undefined `AK_ISP_*`,
   `libmpi_venc.so` has undefined `akuio_*` and `ak_thread_*`. Link them as a
   group, exactly as the stock binary does (its `NEEDED` list has 18 shared
   libraries plus `libc.so.0`).
3. **Three of those libraries are not on the SD card.** `libakispsdk.so`,
   `libakmedia.so` and `libakaudiocodec.so` live only in `/usr/lib` *on the
   device*; the card's `Factory/custom/lib` holds the other fifteen. To link offline we borrowed the
   copies in `src/rtsp/lib` — but see §3.6, they are **not the same build** as
   the board's.

### 1.3 Lead B — a crosstool-ng archive, but the wrong libc

<https://github.com/medevil84/ipcd>, file `src/x-tools.tar.bz2`, **46,300,143 B**
(public, repo is MIT). Same directory holds the crosstool-ng `.config`:
`CT_ARCH_CPU="arm926ej-s"`, `CT_ARCH_ARM_EABI=y`, `CT_ARCH_FLOAT="soft"`,
`CT_LIBC="uClibc-ng"`, `CT_THREADS="nptl"`, `CT_BINUTILS_VERSION="2.26.1"`,
`CT_TARGET_VENDOR="unknown"` → `arm-unknown-linux-uclibcgnueabi`. Built for an
AK39**EV330**, not our EV200.

It is a real, complete toolchain and the vendor is the most honest of the
community projects (it documents that a recompile of Kalkan's server against a
"fresh toolchain that matched our uClibc versions" linked **and then segfaulted
on the first call into `libapp_video`**, <https://github.com/medevil84/ipcd>).
But it is **uClibc-ng**, and per §1.1 the loader/libc pair on our board is
uClibc 0.9.33.2. A uClibc-ng binary would be run by the stock
`/lib/ld-uClibc.so.0`. Unverified on this board and I would not bet the boot on
it; keep as plan C in case Lead A ever disappears. Useful regardless: its README
is the only public write-up of an Anyka SDK bring-up sequence, and it warns that
the DMA pool must be created by the low-level `libplat_mem` API, not
`libapp_mem_ex` — on our v200 that whole layer is *inside* `libplat_vi.so`/
`libmpi_venc.so`, so it is an analogy, not a step we must perform.

### 1.4 Lead C — the vendor's own SDK, public, but no compiler in it

Tange (TiRTC/Nano) publishes an AK3918EV200 SDK built with exactly our
toolchain, on a Nexus repo with **no authentication**:

* docs: <https://docs.tange.ai/products/tirtc/download.html> lists
  "Anyka AK3918EV200 → `arm-anykav200-linux-uclibcgnueabi`" (and separately
  EV300 → `arm-anykav500-…`, EV331 → `arm-anycloud-…`, "AK3918V200" →
  `arm-2009q3-arm-none-linux-gnueabi`, so the naming is not unique to one part).
* files: `curl -s 'https://repo-sdk.tange-ai.com/service/rest/v1/search/assets?repository=tirtc-sdks&q=anyka-ak3918ev200'`
  → 50 assets; a v2.5.0 release tarball is **1,118,262 B**, HTTP 200 without
  credentials.

Downloaded and inspected: it contains `include/tirtc/*.h`, `lib/libTiRTC.a` and
`manifest/build-info.env` with `TARGET=anyka-ak3918ev200`, `ABI=eabi`,
`LIBC=uclibc`, `BUILD_TOOLCHAIN=GCC`, `BUILD_TOOLCHAIN_VERSION=4.8.5`,
`SSL=disabled`. **No compiler, no sysroot** — so it is not a route to a shim, but
it is independent confirmation that gcc 4.8.5 / uClibc / EABI is the vendor
baseline for this exact SoC, and its headers + static lib are a (proprietary,
P2P-oriented) alternative audio/video transport if we ever wanted one.

### 1.5 Leads that turned out to be nothing

* **`upstream` (MuhammedKalkan/Anyka-Camera-Firmware)** — `git ls-tree -r
  upstream/main | grep -i toolchain` → no hits; the repo carries `Factory/`,
  `rootfs/`, `src/` and no toolchain. (ipcd's README says Kalkan's repo "has a
  working copy of `arm-anykav*-linux-uclibcgnueabi-gcc`" — that claim is
  unverifiable from what we have; the tree does not contain one.)
* **Nemobi/Anyka** — docs only (`README.md`, `FILESYTEM.md`, `ROOT_ACCESS.md`,
  `device/`, `start_message`).
* **michelostojski/anyka-ak3918-jxf37-rtsp** — `LICENSE README.md docs images
  scripts src` at top level; no toolchain archive in the root listing
  (contents of `scripts/` not inspected).
* **Gerge/Anyka_ak3918_hacking_journey** (gitea.raspiweb.com) — not fetched in
  this pass. **Unverified.**
* **OpenIPC** — `openipc.org/supported-hardware/full-list` marks AK3918EV200
  "not yet"; the `br-ext-chip-anyka` configs are EV300 with an external
  toolchain. No v200 toolchain there.
* **`gtxaspec/atbm-wifi` `Makefile.vendor`** — references the toolchain *name*
  only (it is the Wi-Fi vendor's makefile).

### 1.6 So: is it obtainable? Yes.

Lead A, 45 MB, verified end-to-end today, exact uClibc match, no licence to rely
on. Lead B exists as a fallback with an ABI caveat. Lead C is confirmation, not a
compiler. Nothing else in public reach provides a v200 toolchain.

---

## 2. Fallbacks, and what "matching" would actually mean

### (a) Build our own with crosstool-ng / buildroot on uClibc 0.9.33.2

Cheapest correct version of this is not "any armv5 uClibc" but *this* uClibc.
crosstool-ng can still fetch `uClibc 0.9.33.2` (`CT_LIBC_UCLIBC`, not `-ng`) and
gcc 4.8.x; the vendor's buildroot vintage (2015.11.1, gcc 4.8.5) is what the
build string in §1.1 records, so aim for that pair. Everything below must match,
and each line has a one-command test so a candidate toolchain can be judged
before it ever produces a binary:

| must match | why | test |
|---|---|---|
| ARM EABI version **5** | OABI (v4) objects cannot be linked or loaded | `readelf -h` → `Flags: 0x…`; v5 EABI shows in `0x5000202` |
| **soft-float** ABI (`-mfloat-abi=soft`, no VFP) | hard-float passes float args in VFP registers; the call into `libplat_vi` reads garbage | `readelf -h` bit `EF_ARM_ABI_FLOAT_SOFT`; or `readelf -A` has **no** `Tag_ABI_VFP_args` |
| **uClibc 0.9.33.2** for loader *and* libc | they share private structs; we cannot replace `/lib/ld-uClibc.so.0` | `cmp <sysroot>/lib/ld-uClibc-0.9.33.2.so analysis/base-rootfs/lib/ld-uClibc-0.9.33.2.so` |
| SONAME `libc.so.0`, interp `/lib/ld-uClibc.so.0` | `NEEDED` strings are literal | `readelf -d <test>`, `readelf -l <test>` |
| **no symbol versioning** on both sides | means the loader will *not* catch any of the above | `readelf -V` on libc and on our binary |
| thread model (NPTL vs linuxthreads) | `pthread_mutex_t`/`cond_t` are ABI: wrong size = silent stack corruption in `ak_thread_*` | `nm -D <libc> \| grep __pthread_mutex_lock` both sides, and compile a `sizeof(pthread_mutex_t)` assertion |
| TLS enabled | `--enable-tls`; a non-TLS libc is a different ABI | `readelf -S <libc> \| grep tdata` |
| `-mcpu=arm926ej-s`, ARM mode, no Thumb-2/VFP emitted | v6 code is `SIGILL` before `main` — the same trap the Rust target has | `readelf -A <binary>` → `Tag_CPU_arch: v5TEJ` |
| `libgcc_s.so.1` ABI | only matters if the shim uses exceptions/64-bit division helpers | `readelf -d` |

Effort: one crosstool-ng build (~40–90 min on build-01), plus an afternoon of
crashes if any line above is wrong, because nothing reports the mismatch. This is
strictly worse than Lead A unless Lead A's provenance is unacceptable.

### (b) Let the device compile it — impossible, and I checked

* `analysis/rootfs/usr/bin` holds **23** binaries: `ak_adec_demo anyka_ipc ccli
  cloudAPI cmd cmd_serverd daemon discovery erase_dev getconf iconv iwconfig
  iwlist iwpriv ldd mdns read_dev read_gpio read_hw testptz write_dev write_gpio
  write_hw`. No compiler, no linker, no `make`.
* `analysis/base-rootfs/bin` holds only `busybox`; a strings scan of it for
  `cc gcc g++ make ld as tcc clang` applets returns **nothing**.
* No `python`, no `perl` anywhere in either extracted tree
  (`find … -name 'python*' -o -name perl` → empty).
* 36 MB of RAM, 8 MiB of flash, a 64 KB jffs2 with ~12 KB free
  (`analysis/flash-hardware.md`). A gcc-4.8 build is not going to live there.

So the shim is cross-compiled, always. (This is also why `tools/rust/motorctl`
and friends are cross-built; the build *host* question is separate and answered
by `build-01` for Rust — but note **build-01 has no ARM cross-toolchain for C**,
so the shim would be built on firefly or a new toolchain would have to go on
build-01. Unpacking Lead A there is 129 MB, fine for the box, but it is
"disposable with no backup", which is another reason not to make it the only
copy of a toolchain nobody can download twice.)

### (c) Keep the stock `rtsp` as the encoder forever, our server as relay

This is what `camrtsp --source client:` was built for and it is half done:
`tools/rust/camrtsp/src/client.rs` already pulls H.264 + a-law from an upstream
RTSP server into a `Hub` (`client::pull(upstream, path)`, `src/client.rs:100`),
and the stock server is **multi-session** so our relay can pull on loopback while
firefly still watches (`analysis/video-pipeline.md:307-322`; two concurrent
ffmpeg pulls, 171 and 175 frames in the same window).

Trade-offs: no new toolchain, no new binary on the card, keeps the encoder and
the ISP under vendor code — but the `uClibc rtsp` process and all 18 of its
`.so`s stay in the path permanently, the four day/night string patches stay
forever, the extra hop re-packetises (`ak_rtsp_demo` → RTP → our depacketiser →
our RTP) and the SDP/URL surface stays the stock one. It is the correct answer
for "ship something this week", and the wrong one for "own the pipeline".

---

## 3. What the shim must do

### 3.1 Where the encoder actually lives today

`rtsp.stock` imports only **21** `ak_*` symbols
(`readelf --dyn-syms Factory/custom/rtsp`):

```
ak_backtrace ak_print ak_sleep_ms ak_thread_create ak_thread_exit
ak_thread_get_tid ak_thread_set_name
ak_drv_ir_init ak_drv_ir_get_input_level
ak_vi_capture_on ak_vi_close ak_vi_get_handle ak_vi_get_sensor_resolution
ak_vi_match_sensor ak_vi_open ak_vi_set_channel_attr ak_vi_switch_mode
ak_rtsp_init ak_rtsp_start ak_rtsp_stop ak_rtsp_exit
```

No `ak_venc_*` at all. The encoder and the RTSP server are **inside
`libapp_rtsp.so`**: it imports `ak_venc_open`, `ak_venc_request_stream`,
`ak_venc_get_stream`, `ak_venc_release_stream`, `ak_venc_cancel_stream`,
`ak_venc_close`, plus `ak_ai_*` (9), `ak_aenc_*` (4) and `ak_aslc_*` (5) —
i.e. it owns video *and* audio. It exports `ak_rtsp_init/start/stop/exit`.

So "replace the encoder driver" concretely means: **replace
`ak_rtsp_init(&param)` and the two `ak_rtsp_start()` calls with our own
`ak_venc_open` + `ak_venc_request_stream` per channel.** `src/rtsp/ak_rtsp_demo.c`
is therefore *not* the reference for the encoder calls — `src/onvif/ak_onvif_demo.c`
is, because it opens the encoder itself:

* `onvif_init_venc()` — `src/onvif/ak_onvif_demo.c:427-465` (fills
  `struct encode_param` per channel, `return ak_venc_open(&param);`)
* `onvif_get_data()` — `src/onvif/ak_onvif_demo.c:490-560` (the
  request/loop/release pattern)

and `ak_rtsp_demo.c:494-521` is the reference for *what the RTSP layer was
configured with* (channel geometry, fps, kbps, qp, gop, `vi_handle`, suffix name).

### 3.2 The call order the shim must implement

Handles are all `void *`; only `ak_vi_open`/`ak_venc_open` create them and only
the matching `close` destroys them.

| # | call | source | notes |
|---|---|---|---|
| 1 | `ak_vi_match_sensor("/etc/jffs2/")` | `ak_rtsp_demo.c:149` | reads `/sys/ak_info_dump/sensor_id`, loads `isp_<sensor>.conf`, compares the `styleId` byte. This is the "read sensor fail" wall — `analysis/video-pipeline.md:43-90`. Unchanged for the shim. |
| 2 | `vi = ak_vi_open(VIDEO_DEV0)` | `:155` | allocates the 24 MiB ION block (`analysis/video-pipeline.md:62`) |
| 3 | `ak_vi_get_sensor_resolution(vi, &resolution)` | `:163` | logs `sensor resolution height:720, width:1280.` (`video-pipeline.md:64`) |
| 4 | fill `struct video_channel_attr` | `:170-190` | `crop = {0,0,W,H}`; `res[VIDEO_CHN_MAIN]={1280,720}`, `res[VIDEO_CHN_SUB]={640,480}`. **Copy the vendor's `max_width/max_height = 640/480` on *both* channels verbatim** — it looks wrong and it is what runs. |
| 5 | `ak_vi_set_channel_attr(vi, &attr)` | `:192` | |
| 6 | `ak_vi_capture_on(vi)` | `:198` | logs `[capture_thread:1416] start to capture` |
| 7 | per channel: fill `struct encode_param`, `enc = ak_venc_open(&p)` | `ak_venc.h:57-70`, `ak_onvif_demo.c:427-465` | values that reproduce today's stream: `width/height` as above, `minqp 20`, `maxqp 51`, `fps 25`, `goplen` (see below), `bps 2000` main / `200` sub (kbps), `profile PROFILE_MAIN`, `use_chn ENCODE_MAIN_CHN/SUB_CHN`, `enc_grp ENCODE_MAINCHN_NET/SUBCHN_NET`, `br_mode BR_MODE_CBR`, `enc_out_type H264_ENC_TYPE` (`ak_rtsp_demo.c:66-80`) |
| 8 | `st = ak_venc_request_stream(vi, enc)` | `ak_onvif_demo.c:499-501` | binds VI to VENC and starts capture. Refcounted: every new RTSP client in the stock flow re-requests it (`video-pipeline.md:158`) |
| 9 | loop `ak_venc_get_stream(st, &stream)` | `ak_onvif_demo.c:540-545` | `0` = got one; anything else = `ak_sleep_ms(10)` and retry. Non-zero is normal backpressure, not an error |
| 10 | `ak_venc_release_stream(st, &stream)` | `:552` | **mandatory** — the buffer belongs to the driver |
| 11 | teardown: `ak_venc_cancel_stream(st)` → `ak_venc_close(enc)` → `ak_vi_close(vi)` | `ak_venc.h:355-407`, `ak_vi.h:257` | never call `get_stream` concurrently with `cancel_stream` (`ak_venc.h:403` note) |

`struct video_stream` is `{unsigned char *data; unsigned int len; unsigned long
long ts; unsigned long seq_no; enum video_frame_type frame_type;}`
(`src/rtsp/include/libplat/include/ak_global.h:94-100`) — exactly the four fields
`camrtsp`'s `media::VideoFrame` needs, plus the frame type that tells us
keyframe vs not.

Dynamic knobs the shim should expose rather than reopen the encoder (all in
`ak_venc.h`): `ak_venc_set_iframe(enc)` (`:264`) — **this is what rtspd's
missing `request_keyframe()` becomes**, so a client that joins mid-GOP does not
wait ~3 s; `ak_venc_set_fps` (`:223`), `ak_venc_set_kbps` (`:257`),
`ak_venc_set_br` (`:297`), `ak_venc_set_gop_len` (`:281`),
`ak_venc_set_mjpeg_qlevel` (`:322`), `ak_venc_get_rate_stat` (`:397`).

**GOP is the one value I cannot pin down from the repo.** `ak_rtsp_demo.c:71`
defines `DEFAULT_GOP 2` (while the `--gop` option hint claims 50), and that `2`
is what reaches `ak_rtsp_init` as `gop_len`; the onvif demo instead
multiplies `goplen = fps * gop_len`. Measured behaviour: SPS says
`max_num_ref_frames=1`, and in-band SPS+PPS appear at each IDR, 36 of each in
120 s ≈ every 3.3 s ≈ 48 frames at 14.7 fps
(`analysis/video-pipeline.md:263-266`).
Pick `goplen = 50` (≈3.3 s at 15 fps) to reproduce it, and verify against the
SPS/IDR interval rather than trusting either comment.

Traps worth writing into the shim's comments:

* the four `get frame failed` lines at start-up are normal and transient
  (`analysis/video-pipeline.md:66-73`) — do not treat them as failure and do not
  restart the process on them;
* the ISP comes up in stages and takes >8 s; the stock app is started 30 s after
  boot for exactly this reason (`video-pipeline.md:54-90`);
* on a 36 MB board the encoder's 24 MiB ION block is the ceiling on everything
  else, including our Rust server (`video-pipeline.md:211`).

### 3.3 Which of those calls the stock binary really makes, at runtime

The app-level ones (steps 1–6) are evidenced three ways: they are in
`rtsp.stock`'s import list, they are in the demo source, and their log lines
appear in the captured boot log — `[ak_vi_open:890] register device, dev type:
0`, `[libakuio]ION: alloc 25165824 bytes`, `sensor resolution height:720,
width:1280.`, then four `[ak_vi_get_frame:1373] get frame failed` followed by
`[capture_thread:1416] start to capture`
(`analysis/video-pipeline.md:59-73`). Steps 7–11 happen inside
`libapp_rtsp.so` on our behalf; the same calls (and their group types) are in
the onvif demo, which is a second, independent caller of the same API against
the same libraries, so the API is not RTSP-specific.

### 3.4 Which config file lives where

* **Encoder: none.** `ak_venc_open()` takes everything in `struct encode_param`.
  The header documents a default config path `/etc/jffs2/venc.cfg` and a
  `ak_venc_set_cfg_path()` override (`src/rtsp/include/libmpi/include/ak_venc.h:154-173`),
  but: that file does not exist on the board (`analysis/rootfs/etc/jffs2/` holds
  `anyka_cfg.ini hw.conf passwd ptz.ini resolv.conf sensor.tgz shadow time
  time_zone.sh yi_cfg.ini yi.conf`), the deployed `libmpi_venc.so` contains no
  `venc.cfg` and no `/etc/` string at all (strings scan), and
  **`ak_venc_set_cfg_path` is not exported** by it (`readelf --dyn-syms` shows
  24 exported `ak_venc_*`, and neither `get_cfg_path` nor `set_cfg_path` is
  among them). Calling either will fail to link. Treat the cfg file as
  non-existent in this build; unverified whether some other path is read at
  runtime.
* **ISP/sensor: yes, on the card.** `/etc/jffs2/isp_sc1346.conf` is a symlink to
  `/mnt/Factory/custom/sensor/isp_sc1346.conf` (104,574 B, three blocks), and
  `/etc/jffs2/styleId` must be **absent** (`analysis/video-pipeline.md:43-64`).
  That is `libplat_vi`'s business and does not change when the RTSP process does.
* **Day/night setpoint: `/tmp/photo`, two bytes.** `printf '1\n'` = night,
  `'0\n'` = day; it is read by `ak_drv_ir_get_input_level`
  (`analysis/ir-daynight.md:251-280`) and today only exists because of a patched
  string (§4).

### 3.5 The wire format already exists — do not invent a second one

`tools/rust/camrtsp/src/venc.rs:1-20` documents `CAMRTSP1`: magic, `u32 flags`,
`u32 fps`, `u32 w`, `u32 h`, `u16 sps_len | sps`, `u16 pps_len | pps`, then
records `u8 kind (1=video, 2=audio) | u64 ts_ms | u32 len | payload`, all
little-endian, Annex-B access units. The shim should write exactly that, one
connection per video channel (the header carries a single w/h + SPS/PPS pair, so
main and sub are two sockets, `--listen` twice or two unix paths). That removes
the largest integration risk: the Rust side is already written and tested
against a synthetic producer.

### 3.6 Deployment facts the shim inherits

* The stock process is started as
  `cd /mnt/Factory/custom && LD_LIBRARY_PATH=$PWD/lib ./rtsp > /tmp/rtsp.log &`
  (`analysis/video-pipeline.md:229-236`). Of `rtsp.stock`'s 19 `NEEDED` entries,
  **fifteen come from the card** and `libakispsdk.so`, `libakmedia.so` and
  `libakaudiocodec.so` come from the device's `/usr/lib` — the loader mixes both,
  as it does today. Whatever the shim needs must satisfy the same split.
* **Version skew, and it matters.** `libakmedia.so` on the board is
  `Media Lib V1.19.00_svn5741`; the copy in `src/rtsp/lib` is
  `V1.17.02_svn5674` (different sha256). Exported symbol sets are identical
  (83 symbols, diff empty), `libakispsdk.so` is `V3.1.00` on both sides (97
  symbols, identical set, different bytes). Before relying on this, pull the
  board's `/usr/lib` copies read-only (`tools/get`) and link against *those*, so
  the link-time prototypes match the run-time objects.
* The shim must never ship the toolchain's own `libc.so.0`: it would land in
  `LD_LIBRARY_PATH` ahead of the device's. Ship one file.
* The stock front end advertises itself through nginx/1.17.8, accepts only
  TCP-interleaved `SETUP`, and answers `404` to UDP (`video-pipeline.md:280-296`).
  None of that survives; `rtspd` already implements both transports and its own
  SDP, which is the point.

---

## 4. What breaks, and which patches move into our code

Today day/night works because of four string slots patched into two files on the
SD card (`analysis/ir-daynight.md:214-236`):

| # | file | offset | was → now | who needs it |
|---|---|---|---|---|
| 1 | `custom/lib/libplat_drv.so` | `0x33bb` | `/sys/user-gpio/gpio-rf_feed` → `/tmp/photo` | `ak_drv_ir_get_input_level` (the photo input) |
| 2 | `custom/lib/libplat_drv.so` | `0x3473` | `…/gpio-ircut_a` → `…/ircut_a` | `ak_drv_ir_init` (`stat()`s it) |
| 3 | `custom/lib/libplat_drv.so` | `0x348f` | `…/gpio-ircut_b` → `…/ircut_b` | same |
| 4 | `custom/rtsp` | `0x1d18` | `…/gpio-ircut_a` → `…/ircut_a` | `camera_set_ir()` in the app (`ak_rtsp_demo.c:248-258`, called at `:301-308`) |

When our shim replaces `rtsp`:

* **#4 disappears** — it is a patch inside the binary we stop running. The shim
  writes the sysfs pins itself (`open("/sys/user-gpio/ircut_a","w")`,
  `…/IR_LED`), which is what `tools/rust/irctl` already does.
* **#2 and #3 become unnecessary** *if the shim does not call
  `ak_drv_ir_init()`*. That call exists only to let the vendor driver read the
  sensor; we do not want the driver's opinion. `libplat_drv.so` can go back to
  the stock file, which is one less patched binary to keep track of.
* **#1 likewise becomes unnecessary** for the same reason: our server owns the
  day/night policy, so `/tmp/photo` is read by *our* code (or an API call), not
  by `ak_drv_ir_get_input_level`.
* **What replaces them is one API call plus two pin writes:**
  `ak_vi_switch_mode(vi, VI_MODE_DAY | VI_MODE_NIGHT)` (`ak_vi.h:200-206`,
  used at `ak_rtsp_demo.c:302` and `:307`) does the ISP scene switch — the thing
  that turned violet night frames (mean RGB 165 161 242, ISP still in day mode)
  into neutral ones (116 116 116) — and the pins are driven directly:
  **day = `IR_LED` 0, `ircut_a` 1; night = `IR_LED` 1, `ircut_a` 0**, `ircut_b`
  untouched (`analysis/ir-daynight.md:293` for night, `:306` for day).
* Copy the **observed pin polarity**, not the vendor's logic: `camera_set_ir()`
  writes `!ir_val` to `IR_LED` in *both* branches after an `ir_val` inversion at
  the call site (`ak_rtsp_demo.c:339`: `ak_misc_set_video_day_night(vi, !ir_val,
  DAY_LEVEL_HH)`, `HH` meaning `led = day`). The truth is the table, not the
  four-line switch.
* The vendor's switch also does `ak_sleep_ms(300)` "to notify av mode to modify
  gop" — nothing is notified (`ak_rtsp_demo.c:311-312`). Do not reproduce the
  comment, do reproduce the settle delay before grabbing frames.
* **The 5 s poll itself is ours to reimplement.** Automatic day/night today is
  a thread in the stock binary (`photosensitive_switch_th_ex`,
  `ak_rtsp_demo.c:319-350`, `ak_sleep_ms(5000)` at `:344`, started in
  `SET_AUTO_MODE`) that reads the photo input and calls the switch on change.
  If the shim has no equivalent, the camera quietly stops switching at dusk and
  nobody finds out until the night footage is black — and `tools/irnight`, which
  works by writing `/tmp/photo` and letting that thread notice within 5 s
  (`analysis/ir-daynight.md:377-380`), stops working too. Keep the same contract:
  poll `/tmp/photo` (or our own file) every 5 s, switch on change, and expose a
  way for the Rust server to ask for a mode directly. One inherited trap: a
  *missing* `/tmp/photo` must mean **day** in our code. In the vendor driver it
  falls through to `/sys/kernel/ain/ain0`, which is a dead ADC stuck at 1171 and
  resolves to night (`ir-daynight.md:371-380`) — that is why `config.sh` writes
  `0\n` at boot.

Also lost with `libapp_rtsp.so`, and each needs an owner in the Rust server:

* **Audio.** 8 kHz a-law exists because `libapp_rtsp` opens `ak_ai` +
  `ak_aenc` (+ `ak_aslc` for AEC/volume) and advertises PCMA in the SDP
  (`video-pipeline.md:263-266`; imports in §3.1). Audio is frozen by decision
  (`ROADMAP.md:67`), so the first shim iteration is video-only and `/vs0` loses
  its sound track. That is a visible regression a recorder might notice.
* **Keyframe-on-join.** The stock server gets a fresh IDR within ~100 ms of
  `PLAY`; our fan-out currently waits for the next GOP unless
  `ak_venc_set_iframe` is called (`src/rtsp/NEEDS.md:6`, "Keyframe on join").
  Owning the encoder is precisely how that ask gets answered.
* **Multi-session fan-out.** Two concurrent pulls work today
  (`video-pipeline.md:307-322`); with the shim, `rtspd`'s own fan-out carries
  that, so the risk moves into our code where we can test it.
* **Snapshot/JPEG.** Stock has no JPEG on 554 (`tools/README.md:237-239`), so nothing is
  lost today; but with the encoder in hand, `ENCODE_PICTURE` +
  `MJPEG_ENC_TYPE` (`ak_venc.h:7-32`, `:322`) is finally reachable for a
  board-side snapshot — that is the "serve the panel from the camera" plan.
* **Risk that does not change:** the 24 MiB ION reservation, the 64 MB board,
  the 4 `get frame failed`, the SD-card `LD_LIBRARY_PATH` arrangement, and the
  rule that only one process may own the encoder. The last one becomes our
  problem to enforce instead of the stock binary's.

---

## 5. Decision table

| route | effort | risk | what proves it worked |
|---|---|---|---|
| **A. Vendor crosstool (Lead A) + C shim → CAMRTSP1 → `rtspd`** | ~1–2 days: unpack toolchain on firefly, write ~300-line shim (VI+VENC+2 channels+day/night pins+`set_iframe`), reproduce `CAMRTSP1`, start from `config.sh` with `LD_LIBRARY_PATH`; `rtspd` side already exists | Medium. Licence/provenance unattributed (local use only). uClibc 0.9.33.2 ABI is *verified*, but the first shim that gets the order or the channel attrs wrong will emit zero frames with no error (vendor libs are silent). Must link against the board's own `libakmedia`/`libakispsdk`. Audio regresses unless added. | On the board: shim runs, `/tmp/shim.log` shows `ak_vi_open` + `capture_thread`, and `rtspd` serves `/vs0` 720p ~14 fps from the socket with firefly's ffmpeg pulling it; a byte-compare of SPS + first IDR against today's stream; `curl` the panel and see pixels with the stock `rtsp` **stopped** |
| **B. Build our own crosstool-ng uClibc 0.9.33.2** | 1 day setup + unknown debugging (§2a) | High-ish: silent ABI mismatches, no symbol versioning to catch them, and slow feedback because every test needs the board | Same as A — plus a `cmp` of our `ld-uClibc`/`libuClibc` against `analysis/base-rootfs/lib/` before anything is deployed |
| **C. uClibc-ng toolchain (Lead B)** | 1–2 h to try | High: wrong loader/libc pair (§1.1); probably crashes in the first vendor call, as it did for ipcd on v330 | A single `hello` using `ak_vi_match_sensor` that reaches `ak_vi_open` without `SIGSEGV` |
| **D. Keep stock `rtsp`, relay with `camrtsp --source client:`** | Hours: wire `client::pull` into `main.rs`, start it after `rtsp` in `config.sh` | Low technically, permanent vendor dependency: 18 `.so`s + 4 string patches + a stock binary we cannot rebuild stay on the card forever | `rtspd` on the camera serving frames whose SPS matches the stock stream, with `/vs0` on 554 still working for firefly |
| **E. Compile on the device** | — | Impossible (§2b) | — |
| **F. Static musl Rust binary that `dlopen`s the vendor libs** | — | Impossible: `NEEDED libc.so.0` cannot be satisfied inside a musl process; no `libc.so.0` soname shim exists | — |

Recommendation: **A**, with **D** as the fallback that already works today.
Before writing the shim, do the 20-minute read-only step in §3.6: pull
`/usr/lib/libakmedia.so` and `/usr/lib/libakispsdk.so` off the board so the
prototypes and the runtime objects come from the same build.

---

## 6. Verified / not verified

**Verified today (host only, no camera contact):**

* the ABI the device requires, from the extracted filesystems (§1.1);
* Lead A downloads, unpacks, runs, and links a program against the board's
  `libmpi_venc.so`/`libplat_vi.so` producing an ARMv5TEJ soft-float EABI5
  executable with `NEEDED libc.so.0` and interp `/lib/ld-uClibc.so.0` (§1.2);
* Lead B/C exist at the sizes stated and are public (§1.3, §1.4);
* the stock binary's import list, `libapp_rtsp`'s import list, and that
  `ak_venc_set_cfg_path` is *not* exported (§3.1, §3.4);
* no compiler of any kind exists on the device (§2b);
* `libakmedia.so` version skew between the board and `src/rtsp/lib` (§3.6).

**Not verified — anybody picking this up should know:**

* nothing was run on the camera: no shim has executed, no frame has been produced
  by our code;
* the actual `goplen` the stock encoder uses (§3.2) — inferred from the observed
  IDR interval, not from a successful read of a config;
* whether `libmpi_venc.so` reads any config file at all beyond
  `struct encode_param` (§3.4);
* uClibc-ng viability (§1.3) and the crosstool-ng route (§2a) were not attempted;
* Gerge's gitea archive was not reachable in this pass (§1.5);
* whether the shim needs `ak_drv_ir_init()` for anything else; I believe it does
  not, but the only evidence is that `ak_vi_open`/`ak_vi_switch_mode` do not
  reference the IR symbols.

### Reproducing the link test

```sh
cd /tmp && curl -sL -o anyka-crosstool.zip \
  https://raw.githubusercontent.com/ricardojlrufino/arm-anykav200-crosstool/master/arm-anykav200-crosstool.zip
mkdir -p xc && unzip -q anyka-crosstool.zip -d xc
TC=/tmp/xc/arm-anykav200-crosstool/usr/bin
export LD_LIBRARY_PATH=/tmp/xc/arm-anykav200-crosstool/usr/lib
cd ~/lab/anyka
$TC/arm-anykav200-linux-uclibcgnueabi-gcc -O2 -o /tmp/shim-arm your_shim.c \
  -I src/rtsp/include/libplat/include -I src/rtsp/include/libmpi/include \
  -L Factory/custom/lib -L src/rtsp/lib \
  -Wl,--start-group -lmpi_venc -lplat_vi -lplat_common -lplat_thread \
      -lplat_vpss -lplat_ipcsrv -lakispsdk -lakuio -lakstreamenc -lakmedia \
  -Wl,--end-group
readelf -h -A -d -l /tmp/shim-arm | grep -E "Flags|Tag_CPU_arch|NEEDED|interpreter"
```

A correct build prints `Flags: 0x5000202, Version5 EABI, soft-float ABI`,
`Tag_CPU_arch: v5TEJ`, a run of `NEEDED` lines ending in `libc.so.0` (eleven in
my test build — one per vendor library actually referenced, plus libc), and
`/lib/ld-uClibc.so.0`. If `Flags` reads `0x5000402` the toolchain is hard-float
and everything downstream is wasted effort — stop there.
