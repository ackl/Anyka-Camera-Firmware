# anyka — YI IoT CB211 / AK3918EN088 V200

Reverse engineering and on-device tooling for one cheap IP camera: stock firmware
archaeology, the SD-card "Factory" app that replaces the stock application, and
the bits of it we drive from a shell or a browser.

The board itself is the lab. Almost everything here was found by reading a
disassembly on the host, pushing a static ARM binary over telnet, and measuring
what the hardware actually did.

## The device

| | |
|---|---|
| SoC | AK3918EN088 V200, ARM926EJ-S (v5TEJ) — **ARMv5**, not v6 |
| Kernel | Linux 3.4.35 `-AKV_2.5.06`, `mem=64M`, uClibc, BusyBox 1.24.1 |
| Sensors | SC1346 1.3 MP (`sensor_sc1346.ko` + `isp_sc1346.conf`) |
| Flash | 8 MiB SPI: `mtd4` SquashFS `/`, `mtd5` `/usr`, `mtd6` JFFS2 `/data`, `mtd7` JFFS2 `/etc/jffs2` (64 KB, ~12 KB free) |
| Network | wlan0 `192.168.1.103`, reached from firefly over `tailscale0` |
| App | `/mnt/Factory` on the SD card; `service.sh` sees it and boots `config.sh` instead of the stock application |

Ports: **2323** telnetd root shell (unauthenticated — trusted LAN only, killed on a
normal boot), 8080 httpd, 554 RTSP, 21 ftpd. 8782 `cmd_serverd` is loopback-only.

## What is in here

| path | contents |
|---|---|
| `analysis/` | the notes. Read these before re-deriving anything: `flash-hardware.md`, `stock-login.md`, `motor-interface.md`, `ptz.md`, `startup.md`, `video-pipeline.md`, `ir-daynight.md`, `audio.md` |
| `tools/` | host-side helpers (`cam`, `put`, `get`, `snap`, `rec`, `beep`, `irnight`, `camweb`) and their docs in `tools/README.md` |
| `tools/rust/` | the Rust on-device tools (`motorctl`, `irctl`, `ispconf`, `led`) and the host panel `camweb` (own RTSP client, own telnet client) |
| `work/` | scratch that turned out to matter: `irpatch.py`, `quiet-cue.sh`, `config.new.sh` (the deployed boot script), the CGI `webui.new`, recovery kit, capture scripts and evidence images |
| `ROADMAP.md` | current state, rollback set, what is next, what is parked |

## Toolchain, the trap that eats afternoons

Device target is **`armv5te-unknown-linux-musleabi`** and nothing else.
`armv6` targets (the Pi Zero ones) link fine, ship, and die with
`Illegal instruction` before `main` because ARM926EJ-S is v5TEJ. Check with
`readelf -A` — `Tag_CPU_arch: v6` means wrong target. The build scripts here
verify that.

    cd tools/rust/motorctl && ./build.sh     # rustc 1.99.1, rust-lld, self-contained
    tools/rust/camweb/build.sh               # host binary, no cargo, no crates
    # C: zig cc -target arm-linux-musleabi -mcpu=arm926ej_s -static -Os

`rustc`/`cargo` on PATH are mise shims that fail with "No version is set for shim";
the build scripts pin the real toolchain under `~/.rustup/toolchains/`.

## Rules for touching the camera

- **No flashing, ever, without a fresh explicit approval.** Nothing here has ever
  written raw flash, and SD-card-only changes are reversible by pulling the card.
- **No reboots to "see if it helps"** — the board needs somebody to power-cycle it.
  A software `reboot -f` works and has been done, but it is a decision, not a reflex.
- **The telnet shell on 2323 is root with no password.** Never forward it.
- **No device credentials in this repo**: no wi-fi PSK, no `p2pPwd`, no password
  hashes. The dumped stock configs that contain them are gitignored (see below).
  Keep it that way.
- Motors have **no end-stops** and nothing knows where the head is pointing. Ask
  before moving anything, and treat driver-reported completion as *reported*, not
  observed.
- **Audio is frozen** (2026-10-25): no tones, no level tests, no chime work unless
  asked. The web panel at `:8080` exists so the owner can test the speaker himself.

## What is deliberately not in git

`.gitignore` explains each entry; the short version:

- `backups/` — firmware images and SD-card tars, ~64 MB. This is the rollback path
  and it lives on firefly, on elisa, and on the SD card itself.
- `analysis/rootfs/`, `analysis/base-rootfs/` — extracted stock filesystems, which
  contain the camera's real `p2pPwd` and root password. Reproducible from `backups/`.
- `Anyka-Camera-Firmware/` — public upstream
  (MuhammedKalkan/Anyka-Camera-Firmware) at `823cb55`, cloned as the SD app source.
- `tools/zig-macos-aarch64-0.13.0` — a downloaded toolchain tarball.
- `tools/rust/*/target/`, compiled ARM binaries, and captured `.mp4`/`.pcm` media —
  build output and bulky evidence; the interesting frames are kept as `.jpg`.

## Where it stands

Video works: `rtsp://192.168.1.103:554/vs0` is 1280×720 H.264 at ~14.3 fps with
8 kHz a-law audio, `/vs1` is 640×480, and it comes up on boot because the SD app
runs `rtsp` itself. Day/night is automatic and switches the ISP scene in-process,
so night frames are neutral rather than violet. The browser panel
(`tools/camweb serve`, `http://firefly:8090/`) shows live MJPEG with PTZ, LEDs and
day/night controls.

The interesting scars, all written up in `analysis/`:

- video was dead for days because `libplat_vi.so` reads `/etc/jffs2/styleId` as a
  single raw byte — `"48\n"` became 52, no ISP block matched, `check_file` said
  "read sensor fail". Deleting the file was the fix.
- the IR-cut/illuminator never worked in stock because the driver hardcodes
  `gpio-ircut_a/b` and `gpio-rf_feed` sysfs paths that do not exist on this board.
  Four string patches make day/night work, both directions, mid-stream.
- the speaker amp is **active-low** and the kernel asserts `SPK_PA=1`
  (`IOC_SET_SPEAKER`) during playback, which mutes it. `quiet-cue.sh` holds the pin
  down for the duration; that is why plain plays were silent.
