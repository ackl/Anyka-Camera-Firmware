# PTZ / motor-control analysis

Analysis date: 2026-10-15. Sources: locally extracted `analysis/rootfs`, `src/ptz`, and the repository's shipped `Factory/custom` ARM executables/libraries. This analysis only inspected files, ELF symbols and ARM disassembly; no ARM program was executed, no camera connection was made, and no motor/configuration command was sent.

## Main conclusions

1. **There is an existing Anyka motor-driver interface: `/dev/ak-motor0` and `/dev/ak-motor1`.** Both the stock `anyka_ipc`/`testptz` programs and the custom PTZ library reference these nodes. Reusing the board's existing motor drivers is the appropriate starting point; an ESP32 or manual GPIO sequencing is not presently necessary.
2. **Important correction: the shipped custom daemon automatically runs motor self-check/homing and requests movement at startup. Avoiding the `init` command alone does not prevent motion.** This is confirmed in both source and shipped binary, not merely inferred from the README.
3. **The SD application is not completely write-free internally.** The PTZ daemon creates/opens `/etc/jffs2/ptz.ini` at startup and saves positions there after movement commands. Thus the backup is a snapshot taken after custom-code boot, not necessarily a pristine pre-custom-boot configuration. No conclusion is made that the backup procedure itself modified these settings.
4. **The README's `setspeed` command is not implemented in the current C++ command dispatcher.** The shipped `libplat_drv.so` also lacks the public `ak_drv_ptz_set_speed` and `ak_drv_ptz_turn_stop` symbols described by the README's example library listing. Do not rely on a low-speed/stop command being available in this daemon.
5. SD/web-interface success proves application execution, **not** correct motor travel calibration, successful homing or absence of competing stock motor control. Those remain to be checked without movement.

## Stock motor software and configuration

### Programs and SDK code

`analysis/rootfs/usr/bin/testptz` and `usr/bin/anyka_ipc` are ARM EABI5 executables using `/lib/ld-uClibc.so.0`. Both contain the strings:

- `/dev/ak-motor0`, `/dev/ak-motor1`
- `libplat_drv_ptz V1.0.01`
- `motor_turn_thread`, `motor_turn`, `motor_change_speed`, `motor_wait_hit_stop`
- `ak_drv_ptz_open`, `ak_drv_ptz_check_self`, `ak_drv_ptz_check_self_ex`
- `ak_drv_ptz_turn`, `ak_drv_ptz_turn_to_pos`, `ak_drv_ptz_turn_stop`
- `ak_drv_ptz_get_status`, `ak_drv_ptz_set_speed`, `ak_drv_ptz_close`
- getters/setters for horizontal/vertical current position and maximum hit.

For `testptz`, ELF dynamic symbols establish that these are **defined implementations inside the executable**, rather than merely unresolved library imports. Examples:

| Symbol | Address in stock `testptz` |
|---|---:|
| `ak_drv_ptz_open` | `0xcd1c` |
| `ak_drv_ptz_check_self` | `0xce0c` |
| `ak_drv_ptz_check_self_ex` | `0xcecc` |
| `ak_drv_ptz_turn` | `0xd0d0` |
| `ak_drv_ptz_turn_to_pos` | `0xd298` |
| `ak_drv_ptz_turn_stop` | `0xd430` |
| `ak_drv_ptz_set_speed` | `0xd5e0` |

`testptz` needs only `librt.so.0`, `libpthread.so.0`, `libdl.so.0`, and `libc.so.0`; its platform PTZ implementation is linked into the program. No standalone stock `libplat_drv.so` was found in extracted `/usr/lib`. The README instruction to substitute the camera's matching platform library is therefore not as simple as copying that filename from this archive. Do not launch `testptz` merely to obtain help: its startup safety has not been established.

Stock `anyka_ipc` additionally has YI-specific PTZ functions (`yi_others_ptz_init`, `yi_others_set_ptz_cmd`, `yi_p2p_on_ptz_direction_ctrl`, `yi_p2p_on_ptz_direction_ctrl_stop`, presets/cruise/home/motion-tracking handlers). Strings describe horizontal/vertical range, degree, rate and speed hardware settings. These are evidence of vendor-specific calibration/control layered on the Anyka driver, **not** a documented CLI or externally callable API.

### Configuration evidence

Only PTZ fields were included here; unrelated configuration and credentials are omitted.

- `etc/jffs2/yi_cfg.ini`: `ptz_Hpos=105`, `ptz_Vpos=70`, `ptz_reset_time=20`, `ptz_h_maxhit=219`, `ptz_h_cur_pos=0`, `ptz_v_maxhit=119`, `ptz_v_cur_pos=0`.
- `usr/local/yi_cfg.ini`: defaults `ptz_Hpos=180`, `ptz_Vpos=65`.
- `etc/jffs2/ptz.ini`: `130 100`, matching the custom daemon's two-integer storage format. Its writer/time of origin cannot be proven from the snapshot alone.
- `var/log/messages:563,565` records two `motor_turn_thread:421` thread IDs during boot. This supports actual motor-software startup; it does not by itself prove physical movement or identify which process owned those threads.

Do not assume these stored integers are direct electrical steps or reliable absolute positions after disassembly. The custom daemon's hard-coded 360/192 degree configuration and approximate 350/120 UI limits are not demonstrated to match the stock vendor calibration.

### Kernel-side module identification: unresolved

No separate `ak_motor.ko` or similarly named motor module was found in the extracted files. `/usr/modules` contains camera/USB/info-dump modules, not a visibly named motor module. Startup scripts inspected show camera/sensor module loading, not a separate motor `insmod`.

The motor driver may be built into the kernel or loaded from a location not captured by this archive. This cannot be settled from the `/usr`, `/etc`, `/data`, log archive alone. `AK_MOTOR` in the custom README is a description, not proof of an installed module named `AK_MOTOR.ko`. Kernel/raw-flash analysis or read-only `/proc`/`sys` inspection is needed.

## Driver interface: what static analysis establishes

Stock `testptz` opens the two motor nodes with `O_RDWR` (helper at `0xcb20`; `ak_drv_ptz_open` at `0xcd1c`). Application-side state and angle-to-step conversion are maintained above the kernel interface. The driver is controlled using `ioctl`, with event waiting also evident in the platform implementation. It is not a text file where one writes `left`/`right`; that text protocol belongs to the custom daemon's FIFO.

The following ABI constants are useful reverse-engineering evidence, **not instructions to send them**:

| Request | Evidence | Interpretation / limits |
|---|---|---|
| `0x40046d0d`, `0x40046d0e` | Custom library `motor_turn.isra.4` at `0x1e84`, constants at `0x1f00/0x1f04`; same constants in stock `testptz` at `0xb390/0xb394` | Opposite-direction step commands, argument pointer to a 32-bit count. Physical left/right/up/down mapping is not established. |
| `0x40046d0b` | Custom `motor_change_speed.isra.3` at `0x1e24`, constant `0x1e78`; stock constant at `0xb6b8` | Speed-setting request with 32-bit value. Units and safe limits need confirmation. |
| `0x40046d10` | Stock `ak_drv_ptz_turn_stop` at `0xd430`, ioctl at `0xd4b4`, constant `0xd4fc` | Stock stop request, passed with argument zero in this implementation. Actual kernel stop/coil-release behavior remains unverified. |

Agreement of these request numbers is stronger evidence of a shared driver ABI than merely sharing an Anyka chip name, but does not establish complete ABI equality or compatible calibration. Complete query structures and kernel semantics have not been recovered. Do not directly experiment with guessed ioctls.

Stock `ak_drv_ptz_turn_stop` accepts direction values 1/2 for one axis and 3/4 for the other. Which direction corresponds to the user's physical assembly still needs confirmation. Killing a process or using FIFO `q` is **not** established as an emergency motor stop.

## Custom daemon comparison and safety issues

### Startup movement: source and binary agree

`src/ptz/ptz_daemon_cpp.cpp:203–230`:

- opens `/etc/jffs2/ptz.ini` with append/create mode;
- creates `/tmp/ptz.daemon` if needed;
- calls `ak_drv_ptz_open()`;
- calls `ak_drv_ptz_check_self(0)`;
- sets degree/rate parameters;
- reads saved position;
- calls `ak_drv_ptz_turn_to_pos(xpos, ypos)` **before accepting commands**.

In the shipped `Factory/custom/ptz_daemon`, disassembly of `main` confirms:

- `0xabfc`: `ak_drv_ptz_open`
- `0xac04`: `ak_drv_ptz_check_self`, argument 0
- `0xac10`: `ak_drv_ptz_set_degree`, arguments 360 and 192
- `0xac1c`: `ak_drv_ptz_set_angle_rate`, arguments `0x40000000` twice
- `0xac20`: saved-position read
- `0xac3c`: `ak_drv_ptz_turn_to_pos`

The library's `ak_drv_ptz_check_self` implementation at `0x2328` confirms argument 0 does **not** mean a passive check: the branch with argument 0 still reaches `motor_turn` at `0x2460` and subsequent repositioning. Whether these calls succeeded on the actual camera is not proven by static analysis.

`Factory/config.sh` launches this daemon automatically. Consequently **rebooting with this SD startup configuration, restarting the daemon, or running it as a connectivity test is potentially a movement action**.

### Actual library versus README example

The shipped `Factory/custom/lib/libplat_drv.so` identifies its PTZ component as `libplat_drv_ptz V2.1.02`. Its exported PTZ API is:

`get_version`, `open`, `check_self`, `set_angle_rate`, `set_degree`, `turn`, `turn_to_pos`, `get_status`, `close` (all with `ak_drv_ptz_` prefix).

It does **not** export the public speed, explicit stop, extended self-check or setup-step-parameter functions shown in the README's example from a different camera/library. The current daemon declares `ak_drv_ptz_set_speed` in `ak_extern.h` but does not call it or implement a `setspeed` command. Its normal directional buttons request 10-unit absolute-position changes; they are not proven low-speed jogs.

### Additional issues before robot use

- Position-changing commands overwrite internal `/etc/jffs2/ptz.ini`; an SD-only experiment should instead keep runtime state in RAM or on SD, with persistence explicitly opt-in.
- `t2p` accepts unchecked values; parsing uses unchecked token indexes and `atoll` conversions. Malformed commands may crash or request unintended targets.
- The `t` command saves `xpos/ypos` without updating them to reflect its relative turn; saved position can become stale.
- Shared command queue/flags are accessed across threads without synchronization.
- `q` just changes a flag; no explicit `ak_drv_ptz_turn_stop`/close or safe shutdown is provided.
- SDK function declarations are hand-written (`void` return types discard status), not verified stock SDK headers. Errors from opening/initialization/movement are ignored.
- The older `ptz_daemon.c` has duplicate `t2p` conditions and frees a pointer into its stack buffer. **This is not the current shipped C++ control path**, so the duplicate-command bug must not be attributed to that binary.

## Recommended next checks — no motion first

1. Keep motors/board secured and do not reboot/restart the existing PTZ daemon as a diagnostic. Any future boot with motors attached must first account for both custom and stock automatic homing. No startup edits were made in this analysis.
2. With separately authorized read-only camera inspection, collect only relevant process names, `/proc/modules`, `/proc/devices`, node metadata for `/dev/ak-motor*`, the corresponding sysfs driver links and process FD ownership. Confirm whether stock `anyka_ipc` and custom `ptz_daemon` are both accessing the motors. Avoid broad configuration dumps containing credentials.
3. Identify kernel motor-driver implementation/config and recover exact query/stop/speed semantics. Prefer inspecting the stock kernel or matching source to inventing GPIO sequences. Do not assume opening a device or a function named “get” is side-effect free until its kernel path is checked.
4. Before any movement test, prepare a bounded control program that starts idle: no automatic homing, no restore-to-position movement, no internal-flash writes, checked errors/arguments, one controller at a time, and a known stop path. Reuse the matching driver interface; do not replace the whole firmware or kernel for this purpose. This is a proposed change, not implemented work.
5. Only after the above and explicit readiness for motor movement, test one short low-speed relative move with a confirmed stop mechanism and travel limits. For a dismantled mechanism, do not rely on homing by hitting former enclosure stops. If the SDK requires homing to establish its coordinate state, do not simply remove that call and assume absolute-position control remains valid; use a verified relative-step path or deliberate calibration plan.

The user's resistance measurements (red-to-other wires approximately 15 Ω, other pairs approximately 30 Ω) are consistent with five-wire unipolar steppers. They do not reveal GPIO assignment, winding sequence, maximum continuous current, gearing or travel limits. Reuse the original connectors/board driver circuitry; never connect motor windings directly to SoC GPIO or infer safe dwell behavior solely from the 5 V marking.

## Reproducibility

Static inspection used `file`, `strings`, `rg`, `arm-none-eabi-readelf` and `arm-none-eabi-objdump`, all on local files. Useful repeatable checks:

```sh
arm-none-eabi-readelf -sW analysis/rootfs/usr/bin/testptz
arm-none-eabi-readelf -d analysis/rootfs/usr/bin/testptz
arm-none-eabi-readelf -sW Factory/custom/lib/libplat_drv.so
arm-none-eabi-objdump -d Factory/custom/ptz_daemon \
  | sed -n '/<main>:/,/^$/p'
arm-none-eabi-objdump -d Factory/custom/lib/libplat_drv.so \
  | sed -n '/<ak_drv_ptz_check_self>:/,/^$/p'
```

This report does not claim a tested motor-control procedure or a complete kernel/GPIO reverse-engineering result.
