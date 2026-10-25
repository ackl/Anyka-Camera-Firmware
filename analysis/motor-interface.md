# Stock motor driver: exact-kernel static analysis

Date: 2026-10-15. Scope: local saved flash/ELF analysis only. No camera connection, motor-device open, ioctl, ARM execution, reboot, configuration change or motor command was performed by this analysis. The separately collected `live-motor-metadata.txt` was read as evidence; that collection belongs to the parent task.

## Findings that change the safety plan

- **The motor driver is built into this exact stock kernel**, not an unidentified loadable module. Its implementation was recovered from the saved `mtd1.bin`.
- **A second opener is not excluded.** It shares the existing driver's device state. **Closing that second descriptor requests a motor stop**, even if it only queried status. Do not attach a supposedly passive standalone motor probe while the existing daemon owns the devices.
- **The ioctl named GET_HIT_STATUS also requests STOP**, due to switch fall-through. It is not a passive query.
- **Both motor axes have their two limit-switch GPIOs configured as -1.** Hardware end-stop detection is disabled by this board configuration. Software position clamping does **not** stop electrical stepping at the nominal position limits.
- STOP is now understood: it marks the motor stopping; the next active hardware-timer callback drives all four phase GPIOs low, posts a stop event, and stops the timer. **The ioctl does not wait for that completion.** No physical stopping/coil-release timing has been tested.
- No replacement controller source is supplied yet. The API is substantially recovered, but exclusive takeover, actual physical safe bounds/direction and asynchronous stop validation are still untested. Providing a superficially safe controller now would hide those unresolved safety conditions.

## 1. Recovery and evidence

The valid XZ stream at offset `0x3898` in `backups/stock-192.168.1.103/mtd1.bin` expands to 4,227,836 bytes. There is an earlier false-positive XZ signature at `0x37bb`; do not use the first signature blindly.

Saved artifacts under `analysis/motor-research/`:

- `kernel.raw`: decoded stock kernel.
- `kernel.elf`: analysis ELF reconstructed with `vmlinux-to-elf`; **not a new flashable firmware image**.
- `kernel-extraction.log`: records 25,808 recovered kallsyms, inferred load base `0xc0008000`.
- `kernel-motor-symbols.txt`, `kernel-motor-disassembly.txt`, `kernel-motor-probe.txt`, `kernel-timer-disassembly.txt`, `kernel-board-init.txt`.
- `board-motor-data.json`: raw words for the two board platform-data structures.
- `testptz-disassembly.txt`: stock userspace PTZ program.
- `reference-ak_motor.c` / `.h`: related public source, used only as an aid, not assumed identical.

Symbols/call targets, ARM instruction alignment, literal strings and decoded structures consistently support the inferred ELF mapping. Relevant exact-stock addresses:

| Function | Address |
|---|---:|
| `ak_motor_open` | `0xc018b238` |
| `ak_motor_close` | `0xc018b498` |
| `ak_motor_ioctl` | `0xc018b510` |
| `ak_motor_turn` | `0xc018b06c` |
| `ak_motor_stop.isra.6` | `0xc018b470` |
| `ak_motor_read` / `poll` | `0xc018b124` / `0xc018b02c` |
| `ak_motor_check_hit_in_runtime_dir` | `0xc018b830` |
| `ak_motor_hw_timer_handler` | `0xc018b894` |
| `ak_motor_probe` | `0xc02f4398` |
| `ak_motor_init` | `0xc03dc210` |
| `ak3918_init_machine` | `0xc03d2de0` |

The live metadata independently records misc character devices `/dev/ak-motor0` (10:61), `/dev/ak-motor1` (10:60), with custom `ptz_daemon` PID 458 as their sole observed owner, fds 3 and 4. No motor module appears in `/proc/modules`. Those observations are a snapshot, not a permanent ownership guarantee.

Public reference source:

- https://github.com/OpenIPC/ak3918ev200/blob/5fafbded3f4fa2552890d78674d88aacd5cac86a/vendor/linux-3.4.35-ak3918-diff/drivers/misc/ak_motor.c
- https://github.com/OpenIPC/ak3918ev200/blob/5fafbded3f4fa2552890d78674d88aacd5cac86a/vendor/linux-3.4.35-ak3918-diff/include/plat-anyka/ak_motor.h

**This source is an older/different driver:** it has software timers, a 1–16 speed range and different angle handling. Our kernel has hardware timers, a 1–200 speed range, extra parameter/status commands, and shared-open behavior. It does share the hit-query fall-through bug. Do not compile its header and assume the resulting application matches this camera.

## 2. Exact stock ioctl dispatch

These are reverse-engineering notes, **not commands to run against the camera**. All integer fields below are 32-bit little-endian. Pointer arguments require correctly sized buffers, irrespective of the size encoded in the ioctl number.

| Request | Exact stock behavior |
|---|---|
| `0x40046d0b` | SET speed: read signed 32-bit integer; accept **1–200**, otherwise `-EINVAL`. Store value, compute a timer frequency in the shared device state. No homing or movement by this command itself. |
| `0x80046d0c` | GET speed: write the stored 32-bit speed value to the supplied pointer. No motor-state modification in this dispatch path. |
| `0x40046d0d` | Relative movement, direction field 0. Read signed 32-bit count, reject negative values; set remaining count and running state, start hardware timer. |
| `0x40046d0e` | Relative movement, direction field 1. Same semantics, opposite phase-sequence order. |
| `0x40046d0f` | Read configured hit GPIO states into a 32-bit bitmask (bits 0/1); **then fall through to STOP**. The ioctl return is based on the second GPIO read, not necessarily zero. On this board the hit inputs are disabled, making this an unsuitable limit-status API regardless. |
| `0x40046d10` | STOP: ignore the data contents, mark stopping and return immediately. The ioctl access checks still examine the encoded direction/size. A real valid 32-bit dummy buffer is preferable to depending on the stock application's null-pointer convention. |
| `0x40046d43` | Extended status: **copy 32 bytes**, despite the ioctl encoding a 4-byte write argument. Returns `copy_to_user` result (zero on success). Does not itself step or stop. See layout below. |
| `0x40046d00` | Set extended parameters: **copy 20 bytes** and replace shared software position/frequency/range state after checks. Not a query; do not use in exploratory inspection. Layout and checks below. |

The magic is `'m'` (`0x6d`). Unknown commands with that magic can return zero instead of an error. **A successful ioctl return is not proof that a request was recognized or completed.** Movement dispatch also discards the return from `ak_motor_turn`, so a timer-start failure need not reach userspace.

### Speed formula and units

At `0xc018b618–0xc018b668`, for accepted speed `s`:

```
f0       = floor(s * 4096 / 720)
delay_ms = max(2, floor(1000 / f0))
freq_hz  = floor(1000 / delay_ms)
```

`ak39_timer_start` divides a 12 MHz clock by that frequency and programs the timer. Each successful timer step applies one of four phase patterns and decrements the requested count by one. Thus movement counts are **phase-update steps in this kernel**, not directly degrees at the camera head. Example settings: `s=1` gives 5 Hz, `s=10` gives 58 Hz, and large settings saturate at 500 Hz. The source name “angular speed” and 4096/720 conversion suggest intended angular calibration, but gearing and physical degrees for these salvaged motors have not been verified. Driver-accepted maximum 200 is **not** a recommended safe speed.

The frequency is installed when starting a turn. Updating the stored frequency during an already-running timer does not visibly reprogram that active timer in the speed ioctl. Do not assume speed changes immediately slow an ongoing move.

### Extended status layout (`0x40046d43`)

Eight 32-bit words, reconstructed from `0xc018b720–0xc018b79c`:

| Word | Meaning established by data flow |
|---:|---|
| 0 | `runtime.running == 1` (boolean) |
| 1 | Software current-position counter |
| 2 | Configured timer frequency (Hz) |
| 3 | Zero |
| 4 | First configured range value, device offset `0x28` |
| 5 | Second configured range value, device offset `0x2c`; upper clamp used for software position |
| 6, 7 | Zero |

**Word 0 being zero does not prove electrical stop completion:** STOP sets runtime to 2 immediately, and completion sets it to 3 later. Both report false. The position is software bookkeeping, not an encoder reading. Field names from an unverified newer SDK are deliberately not assigned to the two range values.

The 20-byte parameter-setting command copies five words to device offsets `0x20..0x30`: software position, frequency, first range, second range, and a fifth value not used in the inspected movement path. It requires first and second ranges positive, second ≤ first, position ≥ 0 and < second, and frequency positive. This route can bypass the normal speed-setting range/formula. It is not appropriate for read-only discovery.

## 3. Open/close, events and STOP

### Ownership is not enforced by the kernel

At `0xc018b298`, if the motor has an existing device-state pointer, `open` logs `warning ak-motor had opened.` and reuses it. It does not return `EBUSY`. First open initializes software state (position 0, two range words 2048, frequency from the board default); it does not call the turn/homing path.

At `0xc018b4f4–0xc018b500`, release/close calls STOP on that shared motor. Therefore opening for GET speed/status and closing is **not passive overall**. A lock file in a new controller would not protect against the already-running daemon, which does not cooperate with that lock.

### Stop sequence

`ak_motor_stop` only changes runtime state to 2 with interrupts protected. At the next active timer callback, the handler sees it is no longer running:

1. Changes runtime state to 3.
2. Writes **0 to each of the four phase GPIOs** (`0xc018b8f0–0xc018b904`).
3. Posts event bit `4` with remaining count.
4. Calls `ak39_timer_stop` (`0xc018ba28`).

This establishes intended coil-output release through the existing driver circuitry, not measured electrical behavior. Software stop cannot guarantee action if the CPU/timer fails. No active timer means no callback is scheduled merely by STOP; a fresh idle device already has no requested stepping, but the status boolean is still not a completion acknowledgement.

### Event reading has nonstandard behavior

`poll` reports pending notification. `read` copies a 12-byte structure consisting of hit number, event bitmask, and remaining step count, then clears notification state. **It returns the result of `copy_to_user` (normally zero), not 12 bytes.** It also does not use the supplied read length as a normal byte-count limit in the inspected path. A caller must not assume conventional stream semantics. The handler emits stop event bit `4`, or `5` for hit plus stop; the older public header's `AK_MOTOR_EVENT_STOP=3` is wrong for this stock binary. Interrupted/blocking read behavior and concurrent readers make naive completion loops unsafe.

## 4. Board wiring and absent end stops

`ak3918_init_machine` assigns these platform-data addresses:

| Axis/device | Platform data | Phase GPIO numbers (A,B,C,D) | Hit GPIOs | Default speed parameter |
|---|---|---|---|---:|
| `ak-motor0` | `0xc03f8360` | 20, 19, 51, 50 | -1, -1 | 200 |
| `ak-motor1` | `0xc03f83b8` | 61, 47, 11, 10 | -1, -1 | 100 |

These are SoC GPIO identifiers, **not connector pin numbers**, and are not permission to bit-bang or rewire motors. Physical pan/tilt axis assignment and positive direction remain untested. Keep using the original board drivers/connectors.

`ak_motor_probe` sees negative hit-pin entries and sets a flag disabling runtime hit checks. The stepping callback saturates its position counter at 0 or `range2 - 1`, but **continues applying phase updates until the requested count is exhausted or stopped**. Stored nominal position/range cannot be used as a physical end-stop safeguard. This is especially important after removing the enclosure and losing the original homing geometry.

## 5. Next implementation gate — no controller yet

The basic relative-step and stop mechanisms are no longer mysterious. Remaining work is about safely owning and validating them, not finding an ESP32 or guessing GPIO patterns:

1. Preserve/reconstruct the SD script as requested and prepare a PTZ-disabled startup variant. Local preparation does not disable the daemon already running on the camera. A deliberate takeover plan must account for that live owner and for any reboot's startup behavior.
2. Do not stop/kill/restart processes as a “read-only check.” Closing the owner's motor fds changes motor state. Arrange safe physical conditions before an explicitly authorized takeover (power off before disconnecting motors).
3. Validate query-buffer and event/stop-completion behavior with a sole owner and motors disconnected first. The existing stock daemon cannot safely coexist with a new probe/controller. A status boolean alone is insufficient to verify stop completion.
4. Only after explicit approval for motion: secure the mechanism, establish which axis/direction is which using a tightly capped relative step count, low timer rate, bounded time and verified stop path. No automatic homing and no restore-to-position movement. Never trust the driver's clamped position as a physical limit.
5. The eventual controller should start idle, issue no configuration-changing queries or internal-flash writes, reject malformed/out-of-range commands, allow only one owner, keep state in RAM, cap both steps and elapsed time, drain/interpret stock events correctly, and handle interruption/stop explicitly. Hardware power removal remains the independent last-resort stop, not a software timing guarantee.

No test-ready C program is included because this gate is not yet satisfied. In particular, disguising motor open/query/close as a harmless diagnostic would contradict the exact kernel behavior established above.

## Reproduction (host-only)

```sh
python3 - <<'PY'
from pathlib import Path
import lzma
src = Path('backups/stock-192.168.1.103/mtd1.bin').read_bytes()
raw = lzma.LZMADecompressor().decompress(src[0x3898:])
assert len(raw) == 4227836
Path('analysis/motor-research/kernel.raw').write_bytes(raw)
PY
uv tool run --from vmlinux-to-elf vmlinux-to-elf \
  analysis/motor-research/kernel.raw analysis/motor-research/kernel.elf
arm-none-eabi-objdump -d analysis/motor-research/kernel.elf \
  --start-address=0xc018b02c --stop-address=0xc018ba50
```

Only host analysis tools run in this procedure; the ARM kernel and camera executables are never executed. Original backups and `analysis/base-rootfs` were not modified.
