# /mnt/stock-backup archived — 2026-10-25

The stock flash dump that sat on the SD card at `/mnt/stock-backup` (25.7 MB,
card-only) now has a second home on firefly.

## What was archived

| | |
|---|---|
| Board file | `/mnt/stock-backup-2026-10-25.tar` (**kept on the card**) |
| Board size | 26,045,952 B (25.4 MiB; the tree itself is 25,728 kB) |
| Board sha256 | `2ecd7efb6c2cd033dd9bf0907d34f94a8926752e86f5c52d4043e69550f9104e` |
| Host file | `~/lab/anyka/backups/stock-backup-2026-10-25.tar` |
| Host size | 26,045,952 B |
| Host sha256 | `2ecd7efb6c2cd033dd9bf0907d34f94a8926752e86f5c52d4043e69550f9104e` (exact match) |
| Entries in tar | **16** (`stock-backup/` + 15 files) — board `tar -tf … \| wc -l` and host agree |
| Format | **plain tar** (busybox has no gzip applet — a `tar -z` once silently wrote 0 bytes) |

Contents (all members are `root/root`, mtime 1980-01-01):

    stock-backup/mtd0.bin            8388608   U-Boot + boot area
    stock-backup/mtd1.bin            1572864   kernel
    stock-backup/mtd2.bin               4096
    stock-backup/mtd3.bin               4096
    stock-backup/mtd4.bin            1048576   SquashFS / (stock)
    stock-backup/mtd5.bin            3100672   /usr (stock)
    stock-backup/mtd6.bin            2269184   /data (stock JFFS2)
    stock-backup/mtd7.bin              65536   /etc/jffs2 (stock, 64 KB)
    stock-backup/stock-files.tar     9579520   stock rootfs files, tarred
    stock-backup/stock-files.tar.gz        0   ← 0 bytes: the gzip-applet trap, worthless
    stock-backup/SHA256SUMS              682   hashes of mtd0–7 + stock-files.tar
    stock-backup/mounts.txt              783
    stock-backup/proc-mtd.txt            269
    stock-backup/uname.txt                71
    stock-backup/tar-warnings.txt         44

## Why it exists / why archive it

`stock-backup` on the card is a *copy of the stock flash dump taken on
2026-10-15* — the rollback material for this camera if anything ever has to go
back to stock. It lived only on the SD card, i.e. one power-up away from the pull
that already truncated four files once (`analysis/audio.md`, SD-eject incident).
It is now also in `backups/`, next to `factory-sd-2026-10-24.tar`.

**Note on overlap:** every member is byte-identical to the already-banked
`backups/stock-192.168.1.103/` (checked member by member with `cmp`; `mtd0.bin`
`c4eab15d…`, `mtd7.bin` `8f5f235c…`, `stock-files.tar` `954c10df…`,
`SHA256SUMS` `fb8ea538…` on both sides). So no *content* was at risk there — what
was card-only was the tree as a single self-contained artifact, plus the useless
0-byte `stock-files.tar.gz`. The 9-hash `SHA256SUMS` verifies clean against the
extracted members, so the rollback images are intact.

## Verification performed

1. Board: `du -sk /mnt/stock-backup` → 25,728 kB; `ls` → 15 entries; sizes listed
   and compared to the host dump; `sha256sum SHA256SUMS uname.txt tar-warnings.txt
   mounts.txt` → md5s identical to the host files.
2. Board: `/mnt/Factory/custom/busybox tar -cf /mnt/stock-backup-2026-10-25.tar
   stock-backup` → `TAR_RC=0`, and **size checked, not assumed** (26,045,952 B;
   a failed run once left 0 bytes and looked like success).
3. Board: `sha256sum` of the tar; `tar -tf … | wc -l` → 16.
4. Pull with `work/pull-big.py` (streaming, `:9985`), sha256 compared to the
   board's before the file was renamed into place — first attempt matched.
5. Host: `sha256sum` re-run independently → identical; `tar -tvf` lists 16
   members; extracted to `/tmp/stock-verify` (then deleted): `sha256sum -c
   SHA256SUMS` → **mtd0–mtd7 + stock-files.tar all OK**, and `mtd0/mtd5/mtd7/
   stock-files.tar/SHA256SUMS` hashes equal to the board originals.
6. Board afterwards: tar still present, `rtsp` running (1), no stray `nc`
   listeners left, `/mnt` free space unaffected (30 GB card).

## Restore / use

Nothing on the card needs rebuilding to use this; the extraction is a plain tar:

    mkdir -p /tmp/stock-restore
    tar -xf ~/lab/anyka/backups/stock-backup-2026-10-25.tar -C /tmp/stock-restore
    cd /tmp/stock-restore/stock-backup && sha256sum -c SHA256SUMS

On the board (if the card's copy is needed back):

    cd /mnt && tar -xf /mnt/stock-backup-2026-10-25.tar

These are **stock flash images**. Restoring them to `mtd*` means writing raw
flash, which this project has never done and does not do without explicit fresh
approval. The `.bin` files are read-only reference material until then.

## Kept or removed?

**Kept** on the card: `/mnt/stock-backup-2026-10-25.tar` (26 MB) and the original
`/mnt/stock-backup/` tree are both still there. `/mnt` is a 30 GB card that is
~0.1% used; there is no reason to delete either, and the card copy is what makes
the dump restorable on the board without a host in the loop.

## Gotcha for the next big pull

`tools/get` reads the whole blob into memory with a **40 s socket timeout**, and
`tools/cam` kills its session at **`CAM_TIMEOUT` = 30 s** by default — so a 26 MB
transfer dies at ~18–20 MB, every attempt, with a clean-looking EOF and a wrong
hash. `work/pull-big.py REMOTE LOCAL EXPECTED_SHA256 [PORT]` streams to a
pid-unique `.part` with a 180 s idle timeout and `CAM_TIMEOUT=600` for the
listener, and only renames on a hash match. It pulled this file in ~52 s
(~490 KB/s). Use it for anything larger than a few MB; `tools/get` stays fine for
small files.

## What is still card-only after this

- `/mnt/Factory` **as it exists now** — `factory-sd-2026-10-24.tar` predates the
  day/night patches (`rtsp`, `lib/libplat_drv.so`) and the quiet boot chime, so
  the current tree is newer than any archive. (Reproducible from the host:
  `src/rtsp/*` + `work/irpatch.py`, plus the cue files and
  `quiet-cue.sh` in `work/`.)
- `/data` (mtd6, 77% full): stock prompts, `factory_is_not_ok`, wifi scripts —
  the `/data` rescue kit is a separate task.
- `stock-files.tar.gz` (0 bytes) is only on the card, and is worthless; do not
  recreate it with `tar -z`.
