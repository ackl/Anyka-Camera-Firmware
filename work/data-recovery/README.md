# /data recovery kit — card-independent copy of the Factory app

Host side of the job done 2026-10-25 10:25 host time (board clock 00:04, no RTC).
Writes went **only** under `/data/recovery/` on the board; `/mnt` was read-only for
this task; nothing was flashed, no reboot, no card pull, rtsp untouched.

## Why

`/mnt/Factory` is the entire application (an SD card holding a `Factory` directory
makes `service.sh` run `config.sh` instead of `anyka_ipc.sh`). It exists only on the
card, and a yank on 2026-10-24 truncated four files to 0 bytes. The host tar
(`backups/factory-sd-2026-10-24.tar`, 78 entries) is no use if the card dies and
that note is all that's left. `/data` = mtd6, JFFS2, in flash: survives the card.

## Space

| | used | free |
|---|---|---|
| `/data` before | 1712 kB | 504 kB (77%) |
| `/data` after | 1828 kB | 388 kB (82%) |

Kit content: 93,394 B of payload + RESTORE.txt 9,486 B.

## What is in it (all mode 0755, copied with `busybox cp -p`)

| path under `/data/recovery` | bytes | sha256 | host counterpart |
|---|---|---|---|
| `config.sh` | 1603 | `c0d13ddb…456f3c7` | `work/data-recovery/config.sh.sd` |
| `custom/rtsp` | 20949 | `fb375089…77e1c6b` | `work/irpatch-rtsp` |
| `custom/lib/libplat_drv.so` | 26594 | `cdbd35a3…0dcafe` | `work/irpatch-libplat_drv.so` |
| `custom/beep` | 956 | `3fc3c2c1…be0259` | `work/audio/beep.sh` |
| `custom/quiet-cue.sh` | 4332 | `cea9d54c…c7ed510f` | `work/data-recovery/quiet-cue.sh.sd` |
| `custom/audio/cue.pcm` | 11520 | `3ba70989…0ef06b` | `work/audio/cue.pcm` |
| `custom/audio/warn.pcm` | 13760 | `67cc91f1…ccc39d` | `work/audio/warn.pcm` |
| `custom/audio/alarm.pcm` | 23680 | `4fea39a3…9d0060` | `work/audio/alarm.pcm` |
| `RESTORE.txt` | 9486 | `6e624457…73f4abef` | `work/data-recovery/RESTORE.txt` |

Every file was hashed **on the card** and **on /data**, and matched. All eight also
match their host counterparts above, so the kit is reproducible from the host
without the card (push with `PUT_PORT=… tools/put`).

`rtsp` and `libplat_drv.so` are the **patched** day/night builds (`fb375089…`,
`cdbd35a3…`); stock originals `7cbce393…` / `383d7d71…` sit next to them on the card
as `.stock`. Re-applying the patch after a restore is *not* needed if these files are
copied back — the patch is inside them.

`config.sh` and `quiet-cue.sh` were copied **last** because another agent was editing
them. They changed under me mid-job (config.sh 905 -> 1603 B: boot LED guard that
writes `/tmp/photo`=0, `IR_LED`=0, `WHITE_LED`=0; quiet-cue.sh 1817 -> 4014 B: keeper
length sized from the file's own duration, 120 s ceiling instead of a flat 10 s), so
the kit was re-copied and the hashes above are the *post-edit* versions, re-hashed
against the card and matching. Host copies of exactly these two are
`config.sh.sd` / `quiet-cue.sh.sd` in this directory, pulled from the card after the
edit (quiet-cue.sh.sd is the 4650 B intermediate -- the authoritative host copy of the
final 4332 B version is `work/quiet-cue.sh`).

The 4332 B quiet-cue.sh also **deleted** the WHITE_LED lamp writes that the 4014 B
version had: a cue no longer lights anything for its duration (the card's
`custom/led` is `led.frozen`, non-executable, on purpose). A cue must never be
steady-on; if a visual cue is ever wanted, blink, don't hold. That rule is written
into RESTORE.txt so a restore cannot bring it back.

## Skipped deliberately (not a mirror — 392 kB left is the reason)

`custom/lib/*.so` (the other 17), `custom/ispconf`, `custom/irctl`, `custom/motorctl`,
`custom/led`, `custom/ptz_daemon`, `custom/onvif`, `custom/sensor/isp_sc1346.conf`
(104574 B — too big for the free space), `web_interface/`, `busybox`, the ~20 test
`.pcm` files, `/mnt/stock-backup` (25 MB, card only). All of those are in the host tar.

Nothing was 0 bytes (the SD-eject signature) at copy time: `find /data/recovery -size 0`
returns only the directories.

## Restore

`RESTORE.txt` on the board is the authoritative, telnet-only procedure: backup the
broken tree aside, `cp -p` the eight files back, `sha256sum` check, `sh -n config.sh`,
restart `rtsp` with `LD_LIBRARY_PATH=/mnt/Factory/custom/lib`. It also spells out the
two facts that are easy to lose: **`/etc/jffs2/styleId` must not exist** (the stock
"48" read as byte 52 is what broke video), and **a dead card means no telnet**, so a
minimal `/mnt/Factory/config.sh` that only brings up the network and `telnetd -p 2323`
is needed first (or the serial console).

## Not done here / for the parent to fold into ROADMAP.md (item 5)

ROADMAP item 5's "recovery copy of the critical `custom/` binaries in `/data`" is now
done and verified; `/data` has 392 kB free, so this is the last such copy that fits.
Suggest: re-check this kit with `cd /data/recovery && sha256sum -c`-style after any
future card change, and add the two patched-binary hashes to the roadmap's day/night
section so a restore cannot silently undo the patch.
