# ispconf — the Anyka ISP config container, in Rust — 2026-10-24

Reads and validates `isp_<sensor>.conf` files (the `DV5_*` format) and explains
`libplat_vi.so`'s own checks in plain language. Written because the whole
"no pixels" problem on this board came down to one byte in this container versus
one byte in `/etc/jffs2/styleId`, and none of that was documented anywhere.

Both binaries are built by `./build.sh`:

| file | runs on | size |
|---|---|---|
| `target/ispconf` | this host (x86-64) | ~450 KB |
| `target/ispconf-arm` | the camera, `armv5te-unknown-linux-musleabi` | ~400 KB |

The ARM build is deployed as `/mnt/Factory/custom/ispconf` (sha256 in
`../../README.md`). Same target rule as motorctl: **`armv5te-…`**, never
`arm-unknown-linux-musleabi` (ARMv6, SIGILL on the ARM926EJ-S).

## Usage

    ispconf inspect   FILE                             # list blocks
    ispconf check     FILE [--sensor 0x1346] [--style 0]
    ispconf extract   FILE --index 0 --out payload.bin  # bytes the ISP parses
    ispconf set-style FILE --index 0 --style 0 [--out F | --in-place]

`check` reproduces the header rules of `check_data.constprop.3` and exits 1 with
the library's own wording when they fail. On the camera, feed it the live sensor
id so the check is against real hardware:

    /mnt/Factory/custom/ispconf check /mnt/Factory/custom/sensor/isp_sc1346.conf \
        --sensor "$(cat /sys/ak_info_dump/sensor_id)"

## The format, as recovered

A container is a sequence of fixed-size blocks; each block is a 512-byte header
followed by a module payload.

| off | size | field |
|---:|---:|---|
| 0x00 | 4 | version, must be **3** |
| 0x04 | 16 | variant name, `DV5_092602` (NUL padded) |
| 0x14 | 4 | sensor id, `0x1346` |
| 0x18 | 2 | build year (u16) |
| 0x1a | 5 | build month, day, hour, minute, second |
| 0x1f | 1 | **block index**, 0–4 |
| 0x20 | 1 | **style id**, 0–255 |
| 0x21 | 3 | zero |
| 0x24 | 4 | block size, header included (`0x882a` here) |
| 0x28 | — | zero up to 0x200 |
| 0x200 | — | payload, `block_size - 512` bytes |

`sc1346`'s file is 104,574 bytes = 3 blocks of 34,858 (34,346 payload each):
block 0 built 2024-04-09, block 1 2024-04-01, block 2 2022-12-12, all style 0.

Two consequences worth knowing:

* `isp_cfg_file_load(0, …)` loads the payload of the block whose **index byte is
  0**. That is the live main-path config; blocks 1–2 are alternate modes.
* The library's `[isp.conf]modify time:` log line prints the *last* block's date
  (2022-12-12 here), not the date of the block it actually loaded (2024-04-09).
  It is a leaked loop variable, not a second config.

## The one-byte failure this tool exists for

`isp_cfg_file_get_style_id()` does `fopen("/etc/jffs2/styleId", "rb")` then
`fread(…, 1, 1, …)` — **one raw byte**, no parsing. `isp_cfg_file_set_style_id()`
writes one raw byte. `check_data` compares it with the block's style byte at
0x20, which is **0** in this container.

So a text file `styleId` containing `48\n` reads as `0x34` ('4') = **52**, which
is exactly the `style_id:52, 0` mismatch that blocked video for days. A missing
file reads as 0 and matches. Stock firmware shipped no `styleId` at all; the fix
was to delete the one that had been created by hand and leave it absent.

    $ ispconf check isp_sc1346.conf --style 52
    block index 0 (0x000000): style id mismatch: block 0, wanted 52 ("is not need file")
    ...

## How the format was found

`libplat_vi.so` on the camera is unstripped and carries DWARF, so its structure
survives: `llvm-objdump -d --triple=armv5te` plus the `.rodata` string pool
reconstructs the checks directly. Key addresses in the `6.0.26.10` build:

| symbol | address | role |
|---|---|---|
| `check_data.constprop.3` | `0x8b1c` | header validation, style/sensor match, block table |
| `isp_cfg_file_load` | `0x9044` | sensor id from `/sys/ak_info_dump/sensor_id`, calls `check_data` |
| `isp_cfg_file_get_style_id` | `0x8fa0` | the 1-byte `styleId` read |
| `isp_main_match_sensor_cfgfile` | `0x9530` | allocates 204,800 bytes, picks the file |
| `ak_vi_match_sensor` | `0x4f10` | scans the config dir for `isp_<id>.conf`, calls `do_match` |

See `../../../analysis/video-pipeline.md` for the whole narrative.
