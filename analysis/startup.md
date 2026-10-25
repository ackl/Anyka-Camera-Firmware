# Stock startup and SD execution analysis

## Scope

Read-only local analysis of `analysis/rootfs`, `analysis/archive-links.txt`, backup metadata and the checked-out `Anyka-Camera-Firmware` repository. No camera connection, ARM execution, configuration changes or motor commands were used. Paths below beginning `etc/`, `usr/` or `data/` refer to files beneath `analysis/rootfs`, unless explicitly identified as repository paths. Line numbers refer to this extracted snapshot. Sensitive configuration contents are deliberately omitted.

## Main finding: the exact SD trigger is identified

**The stock firmware intentionally executes `/mnt/Factory/config.sh` when an SD card contains a directory named `Factory`.** This is a factory-test startup branch, not booting a replacement Linux image and not the model-specific V380 update-patch mechanism.

`usr/sbin/service.sh:185–195` mounts the SD partition (or the whole card if no partition node is present), then sets `FACTORY_TEST=1` if `/mnt/Factory` exists. `service.sh:90–91` directly runs `/mnt/Factory/config.sh`. It does not check a model name, signature or password at that call site. The directory check is not a check that `config.sh` is valid; a missing/broken script can still divert startup away from the normal application branch.

This resolves the previously missing stock caller. Previously observed working HTTP 8080 and root Telnet access are consistent with this chain; those observations predate this local analysis and were not repeated here.

## Boot chain

```text
kernel / initial root filesystem (not reconstructed by this task)
  -> init reads /etc/inittab
     -> /etc/init.d/rcS
        -> /etc/init.d/rc.local
           -> mount stock /usr, /data, /etc/jffs2
           -> start FTP and logging
           -> /usr/sbin/camera.sh setup
              -> temporary SD mount, load sensor and camera modules
              -> unmount SD
           -> /usr/sbin/service.sh start &
              -> mount SD again at /mnt
              -> detect /mnt/Factory
              -> start cmd_serverd, set white-LED output, kill telnetd
              -> execute /mnt/Factory/config.sh
                 -> stock networking + SD-supplied applications
```

Evidence:

| Stage | Evidence |
|---|---|
| Init startup and serial console | `etc/inittab:17` invokes `rcS`; `:19` configures a respawning getty on `ttySAK0` at 115200. The physical UART pads/voltage are not established by this. |
| Basic filesystems and device nodes | `etc/init.d/rcS:5` runs `mount -av`; `:16–17` installs/scans mdev; `:31` calls `rc.local`. `etc/fstab:14–19` describes proc, tmpfs, devpts and sysfs mounts. |
| Initial Telnet | `etc/init.d/rcS:7–8` starts `telnetd`, but later `service.sh:85` kills it. This is not an assurance of a persistent stock login service. |
| Flash-backed mounts | `etc/init.d/rc.local:7–10`: mtdblock5 → `/usr` (SquashFS), mtdblock6 → `/data` (JFFS2), mtdblock7 → `/etc/jffs2` (JFFS2). |
| FTP and logs | `etc/init.d/rc.local:15` starts `tcpsvd`/`ftpd` with `/` as its root and writes enabled; `:18–19` starts syslogd/klogd. Actual FTP authentication behavior is not inferred here. |
| Driver setup then service startup | `etc/init.d/rc.local:31` runs `camera.sh setup` synchronously; `:35` backgrounds `service.sh start`. |
| SD detection | `usr/sbin/service.sh:185–195`; prefers `/dev/mmcblk0p1`, falls back to `/dev/mmcblk0`. |
| Dispatch | `usr/sbin/service.sh:208–210` calls `start_service`; `:80–91` performs preliminary setup then launches the factory script. |

The archived mount snapshot independently corroborates the flash mounts and shows the SD mounted as **exFAT**, not FAT32: `backups/stock-192.168.1.103/mounts.txt:10–14`. The stock mount commands do not force a filesystem type.

## Measured cold-boot timeline (2026-10-25, first instrumented reboot)

This is the first boot observed in flight rather than sampled afterwards (host 10:37,
`sync; reboot -f` from a telnet shell, no person involved). Board clock restarts from
~00:00 on every boot — there is no RTC — so uptime is the only honest time base.

| Uptime | State |
|---|---|
| ~0 s | reboot issued at board 00:15:29 (host 10:37:14); the old kernel keeps answering ping for a few seconds during teardown, so "board is up" polls are false positives until uptime resets |
| ~31 s | `telnetd -p 2323` already answering (`config.sh` line 18) |
| ~41 s | first instrumented sample: `photo=0`, `IR_LED=0`, `WHITE_LED=0`, `ircut_a=1`, `SPK_PA=1`, `rtsp` running |
| 41–145 s | sampled every 2 s, 45 samples, **not one deviation** — no IR glow, no white LED, day the whole way |

Consequences, both of which correct earlier notes:

- The window a card-supplied `config.sh` cannot reach is ~the first **30 s**, not the
  60–80 s previously written here. `telnetd` is up at 31 s and the pins are already
  right at 41 s. Nothing was sampled in 0–30 s (kernel + squashfs rootfs before the SD
  is mounted), so a sub-30 s flash of the stock `IR_LED=1` default is still
  *unruled out* — only the serial console could settle it.
- Video came up from `config.sh` unaided on that boot: 85 frames / 5.907 s =
  **14.4 fps** at 1280×720.
- The day/night thread is alive after a cold boot: `rtsp` pid 683 carries a
  **`PS_switch`** thread (`for d in /proc/$P/task/*; do cat $d/comm; done`) next to
  `ai_capture`, `venc_capture`, `venc_encode` and `vi_change_fps`.
  **Do not use `dmesg | grep "not inited"` for this** — an earlier draft of this note
  did, and it is worthless: `/proc/sys/kernel/printk` reads `3 4 1 7` on this board,
  so every `ak_print` line (including `ak_drv_ir_get_input_level:129 not inited`,
  the signature of the dead stock path) is suppressed and absent whether or not the
  path initialized. The thread list and the pin values are the observables.
- Shutdown was clean: `[EXFAT] unmounted successfully` then `mounted successfully` on
  the next boot, and nothing on the card changed — all Factory hashes matched
  afterwards. `reboot` from the shell is therefore a safe operation on this board
  (the earlier damage came from physically pulling the card, not from rebooting).

## Two mount quirks that produce wrong numbers

**1. `df /mnt` lies — it reports a shadowed tmpfs, not the SD card.** `/etc/fstab`
mounts `tmpfs` at `/mnt` early, and `service.sh` then mounts `/dev/mmcblk0p1` (exFAT)
*over* it. `/proc/mounts` keeps both lines (line 8 `tmpfs /mnt`, line 14
`/dev/mmcblk0p1 /mnt exfat`), and busybox `df /mnt` — and even
`df /dev/mmcblk0p1` — resolves the *first* match, reporting a 31,157,248 KB tmpfs at
0 % use. The real filesystem is the exFAT card. Trust device numbers instead:

```
stat -c '%n dev=%D' /mnt /mnt/Factory /mnt/Factory/custom/rtsp
# all three -> dev=b301  = 179:1 = /dev/mmcblk0p1
```

Any `df /mnt` figure recorded in these notes before 2026-10-25 is suspect for this
reason. `/data` (mtdblock6) and `/etc/jffs2` (mtdblock7) numbers are unaffected.

**2. `find /mnt -size 0` gets ~21 false positives.** The card root carries macOS
`.Spotlight-V100/` and `.fseventsd/` metadata with legitimate empty files. Scope the
zero-byte sweep to what we actually own: `find /mnt/Factory -type f -size 0`
(0 hits after this reboot).

## What factory mode replaces—and what still runs

The factory branch is an alternative to the normal application branch, not merely an extra script appended after the full stock camera app starts:

- `service.sh:90–107` selects Factory / identity-programming / normal branches.
- The normal branch starts `daemon` (`:121–125`), may update factory data (`:136–138`), and launches `anyka_ipc.sh start` (`:140–149`). Those lines are skipped in Factory mode.
- `cmd_serverd` is still invoked first (`:80`), and `close_white_led` still runs (`:82`, implementation `:67–76`). The contents/side effects of the compiled command server have not been reconstructed.
- Kernel/sensor drivers, FTP, syslog, mounts and the SD mount have already been set up. Factory mode does not bypass all stock software.
- `service.sh:153–159` only prints a networking message in the normal local-boot case; the actual `net_manage.sh` command there is commented out. The repository's Factory script explicitly starts networking instead.

Thus "no flashing required" is accurate; "no writes or hardware actions ever occur" would not be. Normal startup scripts write runtime state, and factory-related driver scripts include optional copying paths. Nothing in this analysis executed them.

## Sensor/driver preparation before the custom script

`usr/sbin/camera.sh:35–79`:

1. Attempts sensor modules in `/etc/jffs2` and `/usr/modules` (`:37–38`).
2. Temporarily mounts the SD (`:43–47`).
3. If `/mnt/Factory/data` exists, executes its `data_copy.sh cp_sensor` (`:50–51`) and later loads sensor modules from its sensor subdirectory (`:66–67`). **Do not add factory-data bundles casually: these are different from the current minimal custom-app folder and can invoke copy logic.**
4. If `/etc/jffs2/sensor.tgz` exists, extracts it into `/tmp/sensor_ko_and_isp_conf` and inserts matching modules (`:58–60`); otherwise uses `/data/sensor_ko_and_isp_conf` (`:62`). The latter is an omitted symlink to `/tmp/sensor_ko_and_isp_conf` recorded in `analysis/archive-links.txt`.
5. Unmounts the SD (`:71–75`) and inserts `akcamera.ko` and `ak_info_dump.ko` (`:78–79`).

The subsequent service script mounts the card again. Therefore the custom applications inherit a stock kernel with drivers already prepared; they do not supply a replacement kernel.

## Repository Factory script and dependencies

`Factory/config.sh` contains:

| Lines | Action / implication |
|---|---|
| 3 | Exports `LD_LIBRARY_PATH=/mnt/Factory/custom/lib`; child processes inherit this library search override, including stock programs started here. Binary/ABI compatibility still matters. |
| 5 | Starts stock `/usr/sbin/net_manage.sh` in the background. |
| 6 | Starts default `telnetd`; this does not itself replace or reset account credentials. |
| 7 | Runs the stock AAC playback utility in the foreground. Later launches depend on that command returning. |
| 8 | Starts SD `ptz_daemon`; startup is not proof that a specific motor/driver interface works. |
| 9 | Starts bundled BusyBox HTTP server on 8080 with the SD web root. |
| 10–11 | Waits 30 seconds, then starts SD RTSP binary. |
| 12 | Attempts `/mnt/Factory/custom/vpn/vpn.sh`, but that path is absent from the checked-out Factory tree. No successful VPN execution can be claimed from this script. |

**Snapshot distinction:** the local repository script is the original version with ordinary Telnet. The user's on-card script was subsequently edited to launch a passwordless shell on port 2323. `/mnt` was not included in the stock-files archive, so the exact current on-card script is not part of this local extracted snapshot. Do not present repository line 6 as the current on-card command.

Network dependency chain:

- `usr/sbin/net_manage.sh:48–51` starts Wi-Fi if there is no `eth0`; otherwise it switches between Wi-Fi and Ethernet according to link state (`:18–38`, `:58–79`).
- `usr/sbin/wifi_manage.sh:36–39` backgrounds `wifi_run.sh`.
- `usr/sbin/wifi_run.sh:70–96` loads the station driver and waits for `wlan0`; `:99–115` invokes `wifi_station.sh` to start/connect.
- `analysis/archive-links.txt` records `/usr/sbin/wifi_driver.sh` and `wifi_station.sh` as symlinks into `/data`. It also records WPA executables linked into `/tmp`.
- `data/wifi_driver.sh:103–134` extracts driver/tool archives from `/data` into `/tmp/ko` and `/tmp` under the relevant conditions. An apparently missing executable in the extracted `/usr` tree may be a runtime-provided link target, not a missing firmware component.

The initial archive failure due to a missing `gzip` applet is not sufficient to conclude all boot-time `.tgz` extraction fails. The original stock BusyBox binary, its exact applet/build behavior and boot logs would need separate investigation.

## The README's custom.sh hook is not the stock hook

The README describes `/etc/jffs2/custom.sh` at `README.upstream.md:139–145`. The repository's **replacement** startup script actually calls it at `rootfs/etc/init.d/rc.local:35–36`, after starting Wi-Fi.

The extracted stock startup scripts instead use `/mnt/Factory/config.sh`. No literal `/etc/jffs2/custom.sh` reference was found in the extracted `usr` files or `etc/init.d` files. This is strong evidence not to rely on the README hook for the present stock installation, but not proof that no opaque/constructed binary path could ever refer to it.

There is also a concrete partition-layout mismatch:

- Repository replacement `rootfs/etc/init.d/rc.local:9`: **mtdblock6 → `/etc/jffs2`**.
- Actual stock `etc/init.d/rc.local:9–10`: **mtdblock6 → `/data`**, **mtdblock7 → `/etc/jffs2`**.

This is an independent reason **not to flash the repository image or copy its replacement rootfs wholesale**, even though its SD applications execute on this board. This agrees with the repository's own warning at README lines 41–45.

## What remains unproven

- Bootloader/kernel/rootfs handoff and exact physical flash offsets are outside this script-level analysis.
- The archive contains `/usr`, `/etc`, `/data` and logs—not a complete root tree. `/bin`, `/sbin`, `/lib`, runtime `/proc`, `/sys`, most of `/tmp`, and the SD `/mnt` are absent. In particular the actual init, shell, loader and stock BusyBox implementations cannot all be inspected from this tar alone.
- Script text establishes intended control flow, not a complete trace of one boot. It does not prove every background process succeeded or every driver insertion succeeded.
- HTTP success does not establish RTSP, sensor quality, PTZ safety, compatible replacement firmware, or absence of network/cloud activity.
- Factory mode avoids the explicit normal daemon/anyka_ipc launches shown above; it is not proof that no other component could start related processes.

## Next read-only checks

1. **Locally:** extract the root filesystem from the raw dumps into a separate analysis tree, preserving backups, to recover `/bin`, `/sbin`, `/lib` and any files hidden by mounted filesystems. Compare with the live-view archive; don't execute recovered binaries.
2. **Locally:** inspect ELF dependencies of the custom applications and compare with the bundled and stock libraries. Use static ELF tooling, not the camera's `ldd` or ARM execution.
3. **On the camera, only when a later task authorizes live inspection:** read `/mnt/Factory/config.sh` and list its payload to record the actual deployed version, with any private settings redacted.
4. **Live read-only follow-up:** collect `/proc/cmdline`, `/proc/mounts`, `/proc/modules`, process names/parent relationships and listening ports; inspect relevant boot-log snippets after redaction. This can confirm the script path actually taken and whether normal applications/watchdogs remain active.
5. **Live read-only follow-up:** list runtime sensor/Wi-Fi filenames and resolved symlink targets, and inspect module metadata. Avoid probing hardware buses, invoking driver setup scripts, changing GPIO, or sending PTZ initialization commands.

The best immediate course is to keep using the established SD Factory hook and existing stock drivers while investigating PTZ separately. No internal firmware change is required to continue.
