#!/usr/bin/env python3
"""Patch the Anyka IR/day-night path strings in the SD-card binaries.

Why this exists
---------------
`/mnt/Factory/custom/rtsp` already contains a complete auto day/night path:
main() -> init_other_platform() -> ak_drv_ir_init() + photosensitive thread
(SET_AUTO_MODE), which every 5 s calls ak_drv_ir_get_input_level() and, on a
change, ak_misc_set_video_day_night() -> ircut + IR LED + ak_vi_switch_mode(),
i.e. the ISP scene switch. Nothing needs to be recompiled.

It is dead on this firmware for one reason: the binaries look for sysfs nodes
that this kernel does not create.

    libplat_drv.so  ak_drv_ir_init()  stat()s /sys/user-gpio/gpio-ircut_a and
                    /sys/user-gpio/gpio-ircut_b -- the firmware creates
                    /sys/user-gpio/ircut_a and ircut_b (no "gpio-" prefix).
                    Both stat()s fail -> bitmask 3 -> -1, error 6, and
                    ak_drv_ir_get_input_level() then prints
                    "[%s:%d] not inited." at line 129 and returns -1. That is
                    the message this camera has been logging all along.
    libplat_drv.so  ak_drv_ir_get_input_level() reads the photo-sensor value
                    from /sys/user-gpio/gpio-rf_feed (fallback
                    /sys/kernel/ain/ain0). Neither exists on this board; the
                    stock kernel has the photo-sensor GPIO set to -1.
    rtsp            camera_set_ir() shells out to "echo %d > %s" with
                    /sys/user-gpio/gpio-ircut_a.

So: rewrite three .rodata strings, same slot, NUL-padded. Each string is its own
NUL-terminated constant, so padding after the terminator cannot affect the
neighbours (verified: the next string in the blob starts exactly after the slot).

Control surface after patching
------------------------------
The photo value is whatever we put in the patched path. ak_drv_ir_init() sets
check mode 4 and thresholds 1000/2000, and get_input_level() for mode 4 is:

    value > 1000   -> 1
    value == 1     -> 1
    value == 0     -> 0
    anything else  -> -1 or 0

so with a 2-byte file ("1\n" or "0\n", see irnight) the level is exactly our
value:  1 = night, 0 = day.  The auto thread maps level 1 -> VI_MODE_NIGHT +
IR LED on + ircut_a 0, and level 0 -> VI_MODE_DAY + IR LED off + ircut_a 1, which
matches the wiring established by hand with irctl.

The path is /tmp/photo (volatile on purpose): the board then boots in day mode
unless config.sh seeds it otherwise, and a missing file just means "no photo
sensor", i.e. exactly today's behaviour.

Usage
-----
    ./irpatch.py --check  FILE        # report patched / unpatched / unknown
    ./irpatch.py --apply  IN OUT     # write patched copy (refuses wrong input)
"""

import hashlib
import sys

SLOT = 28  # bytes reserved for each path string, including its NUL

# (file offset, original ASCII path, replacement)
PATCHES = {
    "libplat_drv.so": {
        "sha256": [
            "383d7d710f3c24aa18cd852a94bdfabe751cf6d7db4806f53ddf198fcb9e9e72",
        ],
        "edits": [
            (0x33BB, "/sys/user-gpio/gpio-rf_feed", "/tmp/photo"),
            (0x3473, "/sys/user-gpio/gpio-ircut_a", "/sys/user-gpio/ircut_a"),
            (0x348F, "/sys/user-gpio/gpio-ircut_b", "/sys/user-gpio/ircut_b"),
        ],
    },
    "rtsp": {
        "sha256": [
            "7cbce393cf2452a23f41e71c03c00ab7c98cb0ac8d6062e18a42a621a980e6d9",
        ],
        "edits": [
            (0x1D18, "/sys/user-gpio/gpio-ircut_a", "/sys/user-gpio/ircut_a"),
        ],
    },
}


def which(data):
    for name, spec in PATCHES.items():
        if hashlib.sha256(data).hexdigest() in spec["sha256"]:
            return name
    # also recognise an already-patched file by its edit sites
    for name, spec in PATCHES.items():
        if all(slot_matches(data, off, new) for off, _, new in spec["edits"]):
            return name + " (already patched)"
    return None


def slot_matches(data, off, text):
    raw = data[off : off + SLOT]
    want = text.encode() + b"\0"
    return raw.startswith(want) and raw[len(want) :].strip(b"\0") == b""


def check(path):
    data = open(path, "rb").read()
    name = which(data)
    print(f"{path}: {len(data)} bytes sha256={hashlib.sha256(data).hexdigest()}")
    print(f"  identified as: {name or 'unknown build'}")
    if name is None:
        return 1
    if name.endswith("(already patched)"):
        return 0
    spec = PATCHES[name]
    ok = True
    for off, old, new in spec["edits"]:
        raw = data[off : off + SLOT]
        good = raw.startswith(old.encode() + b"\0") and raw[len(old) + 1 :].strip(b"\0") == b""
        print(f"  0x{off:04x}: {'ok  ' if good else 'BAD '} {raw!r} -> {new!r}")
        ok &= good
    return 0 if ok else 1


def apply(src, dst):
    data = bytearray(open(src, "rb").read())
    name = which(bytes(data))
    if name is None or name.endswith("(already patched)"):
        sys.exit(f"refusing: {src} is not the expected unpatched build ({name})")
    before = hashlib.sha256(bytes(data)).hexdigest()
    touched = 0
    for off, old, new in PATCHES[name]["edits"]:
        raw = bytes(data[off : off + SLOT])
        if not (raw.startswith(old.encode() + b"\0") and raw[len(old) + 1 :].strip(b"\0") == b""):
            sys.exit(f"refusing: unexpected bytes at 0x{off:04x}: {raw!r}")
        want = new.encode() + b"\0"
        if len(want) > SLOT:
            sys.exit(f"replacement too long for the 0x{off:04x} slot")
        data[off : off + SLOT] = want.ljust(SLOT, b"\0")
        touched += sum(1 for a, b in zip(raw, want.ljust(SLOT, b"\0")) if a != b)
        print(f"  0x{off:04x}: {old} -> {new}")
    out = bytes(data)
    open(dst, "wb").write(out)
    after = hashlib.sha256(out).hexdigest()
    print(f"{name}: {len(out)} bytes, {touched} bytes changed")
    print(f"  {before}\n  -> {after}")
    print(f"  written: {dst}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--check":
        sys.exit(check(sys.argv[2]))
    if len(sys.argv) == 4 and sys.argv[1] == "--apply":
        sys.exit(apply(sys.argv[2], sys.argv[3]))
    print(__doc__)
    sys.exit(2)
