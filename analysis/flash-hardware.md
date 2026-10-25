# Stock flash layout and hardware clues

## Scope and method

Read-only local analysis of `backups/stock-192.168.1.103/` and `analysis/rootfs/`. No camera connection, ARM execution, flashing, package installation, or original-file changes. Used host `file`, Python byte matching, uImage CRC validation, in-memory XZ/LZMA decompression, SquashFS superblock/root-directory parsing, and tar member inspection. This report deliberately omits credentials, device identifiers and configuration contents unrelated to hardware.

## Main findings

- **The unidentified A partition is the stock root filesystem `/`**, SquashFS/XZ, not a second firmware slot. Its root contains `bin`, `sbin`, `lib`, `etc`, `init`, etc. The saved boot environment explicitly selects `/dev/mtdblock4`.
- **All seven partition dumps match byte-for-byte inside the 8 MiB `mtd0.bin`**, including the writable filesystem partitions at the time of capture. The whole-flash dump additionally preserves the bootloader/partition-metadata region not exposed by those seven partitions.
- The sensor package is **SC1346** (`sensor_sc1346.ko` plus `isp_sc1346.conf`). The configured Wi-Fi driver family is **Altobeam ATBM6031x**. These are strong stock-configuration clues; they are not a live sensor-ID or Wi-Fi-silicon probe.
- Stock firmware version is `6.0.26.10_202405261656`; kernel is Linux 3.4.35 / ARMv5 module ABI. The saved boot environment configures **64 MiB RAM**.

## Verified flash map

Offsets are relative to `mtd0.bin`; end addresses are exclusive. Each partition offset below was obtained by finding the **entire** corresponding dump in `mtd0.bin`, not by guessing from adjacent sizes.

| Dump / label | Start | End exclusive | Size | Identification |
|---|---:|---:|---:|---|
| `mtd0`, `spi0.0` | `0x000000` | `0x800000` | 8,388,608 bytes | Entire SPI flash |
| Region not separately dumped | `0x000000` | `0x031000` | 200,704 bytes | Bootloader and associated metadata; internal boundaries not fully decoded |
| `mtd1`, `KERNEL` | `0x031000` | `0x1b1000` | 1,572,864 bytes | Legacy uImage wrapping ARM zImage |
| `mtd2`, `MAC` | `0x1b1000` | `0x1b2000` | 4,096 bytes | Vendor-labelled MAC/config region; contents not reported |
| `mtd3`, `ENV` | `0x1b2000` | `0x1b3000` | 4,096 bytes | Boot environment |
| `mtd4`, `A` | `0x1b3000` | `0x2b3000` | 1,048,576 bytes | SquashFS root `/` |
| `mtd5`, `B` | `0x2b3000` | `0x5a8000` | 3,100,672 bytes | SquashFS mounted at `/usr` |
| `mtd6`, `C` | `0x5a8000` | `0x7d2000` | 2,269,184 bytes | JFFS2 mounted at `/data` |
| `mtd7`, `D` | `0x7d2000` | `0x7e2000` | 65,536 bytes | JFFS2 mounted at `/etc/jffs2` |
| Trailing region | `0x7e2000` | `0x800000` | 122,880 bytes | Entire region is `0xff` in this dump |

`proc-mtd.txt` reports 4 KiB erase size for every entry. `mtd0` overlaps all the others: do **not** concatenate `mtd0` with the partition files or regard them as additional flash capacity. The labels A/B/C/D here describe filesystems, not demonstrated A/B update slots.

## Bootloader and kernel evidence

### Bootloader

The initial region contains the banner at whole-flash offset `0x24e43`:

```text
U-Boot 2013.10.0-AK_V2.0.04 (Jan 18 2024 - 17:59:59)
```

It also contains Anyka flash-partition routines and SPI-flash/update command strings. This establishes an Anyka-customized U-Boot presence, but does not establish the exact boot-ROM header format, partition-table boundaries, or a safe standalone bootloader flashing procedure.

`file mtd0.bin` misleadingly labels it **OpenPGP Secret Key**. That is a magic-signature false positive in this context, not evidence of encryption or a key file. The embedded valid uImage, two SquashFS filesystems and exact partition matches establish the firmware structure independently.

### Kernel

At whole-flash `0x31000` (`mtd1` offset zero):

- Legacy uImage magic `0x27051956`.
- Name `Linux-3.4.35`, Linux/ARM kernel, payload length **1,475,064 bytes**.
- Load address `0x81808000`; entry point `0x81808040`.
- Header CRC and payload CRC both recompute correctly.
- uImage timestamp: **2024-04-12 09:59:40 UTC**.
- ARM zImage signature at whole-flash `0x31064`, consistent with its position 0x24 bytes inside the payload after the 64-byte uImage header.

Although the uImage header says **not compressed**, the wrapped zImage contains an XZ-compressed kernel stream. The valid XZ stream at `mtd1 + 0x3898` expands in memory to **4,227,836 bytes**. It contains Linux 3.4.35 build text dated April 12, 2024 and board-platform string:

```text
CLOUD39EV2_AK3918EV200X_EVB_CBD
```

This supports the AK3918EV200 platform family but is not proof of the exact package suffix read on the physical chip (user-established marking: AK3918EN088 V200). Likewise, a compiled board name is not necessarily the retail PCB designation.

### Boot parameters and RAM

The saved `mtd3` environment specifies:

```text
root=/dev/mtdblock4 rootfstype=squashfs init=/sbin/init
mem=64M memsize=64M
console=ttySAK0,115200n8
```

`mtd_root` also points to `/dev/mtdblock4`. Embedded defaults elsewhere mention different root block indices; those fallback/build-time strings should not override this saved environment and filesystem evidence. **64 MiB is the configured kernel memory size**, not an independent physical RAM capacity measurement. Console parameters suggest 115200 8N1, but do not establish UART pad locations or voltage.

## SquashFS filesystems and why the file archive is incomplete

Both superblocks are little-endian SquashFS 4.0 with XZ compression (compression ID 4), 131,072-byte blocks.

| Partition | Filesystem bytes used | Inodes | Creation time (UTC) | Parsed root entries |
|---|---:|---:|---|---|
| A / `mtd4` | 971,054 | 228 | 2024-05-26 08:56:41 | `backup`, `bin`, `data`, `dev`, `etc`, `init`, `lib`, `mnt`, `proc`, `sbin`, `sys`, `tmp`, `usr`, `var` |
| B / `mtd5` | 2,883,232 | 282 | 2024-05-26 08:56:42 | `bin`, `fw_version`, `lib`, `local`, `modules`, `sbin`, `share` |

Root entries were decoded from the SquashFS inode/directory metadata in memory, rather than inferred solely from signatures. The mount capture independently records `/dev/root` as SquashFS at `/`, `mtdblock5` as SquashFS at `/usr`, and the two JFFS2 mounts as shown above.

The existing `stock-files.tar` covers `/usr`, `/etc`, `/data` and `/var/log/messages`. It does **not** cover the complete base filesystem: notably `/bin`, `/sbin`, `/lib`, `/init`, and any files hidden beneath mountpoints are not comprehensively represented. Thus **raw extraction of `mtd4` remains useful**, even though most application files are already available. A backup of the mounted view and a raw flash dump complement each other.

This task parsed the SquashFS roots, not every inode/file. No complete JFFS2 reconstruction was attempted; JFFS2 can also retain obsolete nodes absent from the mounted filesystem view.

## Sensor and Wi-Fi evidence

### Image sensor: SC1346

`analysis/rootfs/etc/jffs2/sensor.tgz` contains exactly:

- `sensor_sc1346.ko` — 7,612 bytes.
- `isp_sc1346.conf` — 104,574 bytes.

The module's own metadata says `SoC Camera driver for sc1346 sensor`, with `vermagic=3.4.35 mod_unload ARMv5`. Internal symbols include `sc1346_probe_id`, `sc1346_get_resolution`, and SC1346 exposure/gain/timing functions.

`usr/sbin/camera.sh:57–62` extracts this package into `/tmp/sensor_ko_and_isp_conf/` and inserts matching `sensor_*.ko` modules. Later it inserts `akcamera.ko` and `ak_info_dump.ko`.

**Conclusion:** SC1346 is the stock-selected sensor driver/ISP pairing. A read-only runtime log or already-exposed sensor-ID value would confirm the hardware match. This analysis does not claim sensor resolution, frame-rate limits or bus wiring merely from the filename.

### Wi-Fi: configured ATBM6031x family

`analysis/rootfs/data/wifi_driver.tgz` contains `atbm6031x.ko.lzma` (320,939 bytes). Host-side, in-memory LZMA decoding reveals:

```text
description=Softmac altobeam apollo wifi common code
alias=atbm_core
vermagic=3.4.35 mod_unload ARMv5
```

`data/wifi_driver.sh:41–46` reads a one-character Wi-Fi selector from `hw.conf`. Reading only that field yields **`4`**; `get_info_from_hw_value()` maps `4` to `atbm6031x` (around lines 83–84). Its configuration function inserts that module (lines 294–303), optionally with `wifi_bt_comb=1` when another mode flag requests it.

The same script supports many alternative Wi-Fi families, but their presence in generic branches **does not mean this board contains those chips**. The actual bundled driver plus saved selector are the relevant evidence. The optional combination-mode branch also does not by itself prove Bluetooth hardware is present or enabled.

`wifi_tool.tgz` includes hostapd, wpa_supplicant and related tools/config templates. No network credentials are included in this report.

## Recommended next read-only checks

1. Extract **only `mtd4`** into a separate private analysis directory with a host SquashFS reader, preserving symlinks safely. This should recover `/bin`, `/sbin`, `/lib`, `/init` and hidden base files missing from the current archive. Neither `unsquashfs`, `binwalk` nor `7z` was found in PATH during this task; none was installed.
2. If live inspection is subsequently authorized, read `/proc/cmdline`, `/proc/meminfo`, `/proc/modules`, and hardware-identification logs to confirm boot parameters, loaded sensor/driver and exposed RAM. Avoid I²C probing or reloading modules just to identify hardware.
3. Keep the original `sensor.tgz`, ISP configuration, Wi-Fi driver bundle and whole-flash dump together. Exact sensor/ISP and kernel-module ABI compatibility matters more than the broad Anyka family name when adapting custom applications.
4. Binwalk is optional now: the major regions have already been identified and their offsets verified. It may help a later survey of embedded objects, but **it is not required to proceed with startup/PTZ analysis**, nor does signature detection guarantee a flashable image.

## Remaining limitations

- Bootloader header/partition-table internals and a restoration procedure are not validated.
- Sensor and Wi-Fi identifications are package/configuration evidence, not live chip-ID reads.
- No physical flash-manufacturer identification is inferred from the generic `spi0.0` label.
- No proof of physical RAM size beyond the 64 MiB boot configuration.
- Only filesystem metadata roots, not complete raw filesystem contents, were decoded in this task.
- Existing mounted-file archive was captured while the system was running, not from a frozen filesystem snapshot. However, every saved partition dump exactly matches its corresponding region of the saved whole-flash dump.
