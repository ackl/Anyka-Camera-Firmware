# Stock base filesystem and built-in Telnet login

## Outcome

- Extracted the base SquashFS partition `mtd4.bin` to `analysis/base-rootfs/` for local analysis.
- **No root password recovered.** A bounded offline check of 20 common/default camera-password candidates against both saved root hashes found no matches. No online guesses, large cracking job, or camera connection were made.
- The built-in Telnet daemon can select a shell instead of its login program: static inspection confirms its `-l` and `-p` options. Therefore a passwordless SD-startup shell does **not inherently require the custom BusyBox executable**. This is a separate mechanism from discovering a password.
- No credentials or password hashes are included here. No recovered-secret file was created, since there was no recovery.

## Extraction and safe handling

Source: `backups/stock-192.168.1.103/mtd4.bin`, SquashFS 4 / XZ. Parsed using host Python with `PySquashfsImage 0.9.0` installed into a temporary `uv` environment.

Output consists of **36 regular files and 19 directories**. All **173 symlinks** were recorded in `analysis/base-rootfs-manifest.json` rather than materialized; no device nodes were created. Paths were checked against traversal, regular-file copies have mode 0600 and directories 0700, and executable bits were not retained. The manifest records original modes/ownership and file SHA256 values. These are analysis copies, not an immediately runnable reconstructed root filesystem.

The missing `/bin`, `/sbin`, `/lib` and base startup files are now available. Symlinks explain why some expected paths are not regular files in the extraction. Original backup checksums were reverified after analysis: all nine still match. No ARM executable was run and neither backup nor repository files were edited.

## Where stock account information actually lives

The SquashFS root has these links:

| Visible path | Link target |
|---|---|
| `/etc/passwd` | `jffs2/passwd` |
| `/etc/shadow` | `jffs2/shadow` |
| `/bin/login` | `busybox` |
| `/bin/telnetd` | `busybox` |
| `/sbin/telnetd` | `../bin/busybox` |

Stock `/etc/init.d/rc.local:10` mounts `mtdblock7` at `/etc/jffs2`. Thus account data comes from the writable D partition, **not from embedded plaintext defaults in the base SquashFS**. The archive preserves it as regular files at:

- `analysis/rootfs/etc/jffs2/passwd`
- `analysis/rootfs/etc/jffs2/shadow`

`root` has UID/GID 0, home `/`, shell `/bin/sh`. Its passwd entry contains a traditional **DES-crypt hash directly**, not `x`. The separate shadow entry contains an **MD5-crypt hash**. Do not assume these two entries are synchronized or that the shadow entry controls login.

The saved files are post-custom-boot evidence. Their archived timestamps are both 315504000 (1980-01-01 UTC), but camera clock/timestamp reliability is insufficient to establish creation date or factory originality.

## Which hash the built-in login uses

The extracted `/bin/busybox` identifies itself as **BusyBox v1.24.1, built 2023-04-10**. Its exported/imported-symbol information and ARM disassembly were inspected locally with `pyelftools 0.33` and `capstone 5.0.9`.

Relevant virtual addresses in this exact binary:

- Login path at `0x10420` calls imported `getpwnam`; its result becomes the account structure.
- At `0x1051c–0x10520`, the account structure is passed to password prompting/checking.
- The prompt wrapper at `0x8a568` passes it onward to the checker at `0x8a510`.
- At `0x8a518`, the checker loads the account's `pw_passwd` field (structure offset 4), passes it as the salt/hash argument to the crypt wrapper at `0x92f84`, then compares the returned crypt string against the same field.
- The wrapper calls imported `crypt` at `0x92f88`.

This directly supports **authentication against the passwd hash in this build**, with no intervening shadow lookup on the inspected path. There is also no `getspnam` import or `/etc/shadow` literal in this BusyBox binary. The libc does provide shadow functions, but their mere presence does not mean BusyBox login uses them.

The 20-candidate offline check used Passlib 1.7.4's DES-crypt and MD5-crypt implementations on the respective saved entries. **Zero matches in either file.** A familiar password from a different Anyka/V380 camera must not be presented as verified for this unit.

## Are the account files generated at boot?

No account-generation or password-replacement command was found in the extracted text startup scripts. The inspected chain mounts the existing JFFS2 account files rather than creating them:

1. `/etc/init.d/rcS:8` starts stock `telnetd`.
2. `rcS` subsequently invokes `rc.local`.
3. `rc.local:9–10` mounts `/data` and `/etc/jffs2`.
4. `/usr/sbin/service.sh:85` kills Telnet before selecting its normal/factory/debug branch.
5. The SD Factory script is responsible for starting whichever later Telnet service is desired.

A scan of extracted regular-file contents found no literal `/etc/jffs2/passwd` or `/etc/jffs2/shadow` references in application binaries/scripts, and no obvious startup `passwd`/`chpasswd` writer. This is evidence **against simple script-based per-boot generation**, not proof that factory provisioning, a reset path, or an opaque binary could never modify the files. `/tmp` and all runtime application behavior were not captured; no live inspection was performed. Account defaults across other units remain unknown.

## Native Telnet alternative, without changing a password

Static inspection of the stock BusyBox Telnet daemon shows:

- default login program `/bin/login` (literal reference at `0x2e644`);
- parsed option string `f:l:Kip:b:FSw:` (literal reference at `0x2e654`), including **`-l PROGRAM` and `-p PORT`**.

Consequently the equivalent **candidate SD-startup command** is:

```sh
/bin/busybox telnetd -p 2323 -l /bin/sh &
```

This uses stock BusyBox instead of `/mnt/Factory/custom/busybox`, and bypasses login rather than recovering or changing a password. It was **not executed or tested on the device**. Do not start a second listener on port 2323 while the existing one owns that port. Normal password-authenticated stock Telnet still requires the unknown root password. Startup service ordering matters because `service.sh` kills earlier Telnet daemons.

This remains an unauthenticated root shell and must be confined to a trusted LAN, never port-forwarded, and removed when no longer required. Avoid restarting the full current Factory script merely to test it: that script also starts the auto-homing PTZ daemon. Nothing in this analysis reboots the camera, starts/stops a daemon, changes credentials, or moves motors.
