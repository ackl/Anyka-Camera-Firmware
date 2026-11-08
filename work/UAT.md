# UAT — accepting the camera as a product (2026-11-08)

The build is done enough to stop testing *pieces* and start testing *journeys*: what
a person gets when they plug the thing in and walk away from it. This is the plan,
the measurements, and the honest list of what a pass cannot yet cover.

Two halves to every journey:

* **the human half** — unplug it, wave at it, turn the hotspot on. Only you can do
  this, and it is the only way to accept the thing.
* **the measured half** — `tools/uat`, one command on firefly, prints a pass/fail
  table with the evidence. Run it at the start and end of every journey:

```
tools/uat            # board + 3 s live capture + receiver + edge   (~15 s)
tools/uat --no-live  # skip the capture                              (~5 s)
tools/uat --json     # append one line per run to a results file
tools/uat --expect-upload=600   # wait for a *new* file on the receiver
```

It is read-only: nothing is started, stopped or written on the card.

---

## What "accepted" means

| # | Criterion | Journey |
|---|---|---|
| A1 | Power cut → it comes back on its own, dark, with video, in under 2 min, touching nothing | J1 |
| A2 | A person can see the live picture from the laptop | J2 |
| A3 | Something happens → a clip of it exists on the homelab within one upload pass | J3 |
| A4 | Nothing happens → zero bytes leave the house | J4 |
| A5 | Uplink or receiver down → nothing is lost, it retries, it catches up | J5 |
| A6 | A busy hour does not lose footage to the card quota | J6 |
| A7 | Offsite on LTE, with no inbound reachability, A1–A5 still hold | J7 |
| A8 | Anything broken can be undone by pulling the SD card | J8 |

---

## J0 — Preflight (2 min)

```bash
cd ~/lab/anyka && tools/uat
```

**Pass:** `board reachable`, `clock sane`, `boot LED guard`, `stock rtsp`, `camrtsp`,
`upload target`, `receiver reachable`, `edge auth` all `ok`. A `warn` on
`last upload pass — none yet` is normal within the first 5 minutes of a camrtsp
start (the first pass is 300 s in).

Do not start a journey on a preflight fail; the journey would be measuring the
wrong thing.

---

## J1 — Cold boot, unattended (10 min)

**Who:** you, at the power socket. **Goal:** A1.

Not `reboot -f`. A reboot keeps the kernel's SD card slot state warm; a real power
cut is the condition the camera will actually live in.

1. `tools/uat` — note the current numbers (this is your baseline).
2. Pull power. Wait 10 s. Plug it back in.
3. **Do not touch anything for 3 minutes.** Watch for a red glow at ~5 s (that is
   the failure this journey exists to catch) and listen for the chime.
4. `tools/uat`

**Expect:** no IR glow, one chime, then by ~40 s uptime the table reads `clock sane`
with today's date, `boot LED guard photo=0 IR_LED=0 WHITE_LED=0`, `stock rtsp` and
`camrtsp` both running, `live video :8554 1280x720`.

**Fail looks like:** the red glow (night fallback — `/tmp/photo` was not written
early enough); `clock sane FAIL` at 1970 (then anything recorded is named `19700101-*`
and never uploads, because `keep_days` treats a 1970 file as infinitely old);
`camrtsp` missing (usually `ingest.url` or the binary).

**Note:** the board is unreachable for the first ~30 s while the kernel and SquashFS
come up. That is not a failure.

---

## J2 — Watch it live from the laptop (2 min)

**Who:** you, in a browser. **Goal:** A2.

Either of these:

* `rtsp://192.168.1.103:8554/vs0` in VLC/mpv — **our** server.
* `http://firefly:8090` — the MJPEG panel (`tools/camweb serve`, if it is running).

**Expect:** 1280x720, ~15 fps, sound. `tools/uat` should say
`live video :8554 1280x720 @ 15/1` in the same minute.

**Fail looks like:** a picture but `live video FAIL` — that means it was serving from
somewhere else. Check `stock rtsp (:554)` too: our server pulls from the stock one,
so if that pid is gone, ours has nothing to re-serve and will stall at the next
keyframe.

---

## J3 — Something happens, and I get it (the core journey, 10 min)

**Who:** you in front of the camera, then the homelab. **Goal:** A3.

1. `tools/uat` — note `receiver reachable … NN files`.
2. Stand in the picture and wave/move for a good 10 s. Walk away.
3. Within ~10 s the log shows it:
   ```
   tools/cam -t 60 'grep -E "camrtsp: (motion|rec)" /tmp/camrtsp.log | tail -3'
   ```
   `motion active` while you move, then `motion quiet`, then `rec close … (post_roll)`.
4. Wait one upload pass (300 s), then:
   ```
   tools/uat --expect-upload=600
   ```

**Expect, in order:** motion fires within ~2 s of real movement (the detector compares
each P-frame against a 30-frame median, so a wave against a static wall scores 2–4×);
a segment opens at the next keyframe with **3 s of pre-roll** already in the ring, so
your entry lands ~3 s into the clip rather than at frame 0; it closes **3 s after the
detector clears** (the detector itself holds `active` for 5 s after your last move, so
expect ~8 s of tail); a `.json` sidecar appears next to the `.h264`; the next
upload pass sends it, the receiver logs its sha256, and **only then** is the local copy
deleted. `tools/uat` after the fact: `card segments 0 h264`, `receiver freshness PASS`,
`last upload pass 1 considered, 1 uploaded … 0 failed`.

**Fail looks like:**
* nothing at all in the log → the detector is not seeing you (night/IR footage is
  untuned: the day→night bitrate shift is ~1.65× and the threshold is 1.75×, so a
  night-time wave may not cross it — retune with `motion.rs`'s documented knobs);
* `upload … 0 uploaded, 1 failed` → see J5, then the reason is in the log's
  `upload kept NAME: message` line;
* uploaded but you cannot watch it → that is a receiver problem, not a camera one;
  the sidecar tells you `duration_ms`, `au_count`, `keyframe_count`, `width`, `height`.

**Acceptance detail worth checking by hand once:** download one segment from
storage-01 and play it. Pre-roll is **3 s** (`pre_roll_ms`, capped at 96 AUs / 4 MiB),
so you should appear a couple of seconds in, not at frame 0 and not 15 s in. If you
appear at frame 0 the ring had no IDR in it and the recorder fell back to the
`keyframe_wait_ms` escape hatch (≤5 s) — that is the failure this check catches. A
segment that is exactly 60 s means you were still moving at rotation time
(`max_segment_len_ms`), not that it broke.

---

## J4 — Nothing happens, nothing moves (30 min, mostly waiting)

**Who:** you, not in the room. **Goal:** A4 — this is the money journey on LTE.

1. Note `receiver reachable … NN files`. Leave the room. Do not walk past the camera.
2. After 20–30 quiet minutes: `tools/uat`.

**Expect:** `motion quiet` in the log, **`NN` unchanged and zero new bytes on the
receiver**, no new segments on the card, and `skipped-young`-only upload passes at
most. This is what makes a 10.5 GB/day 720p stream affordable over cellular: the
uplink only ever carries events.

**Fail looks like:** a slowly climbing file count with nobody in the room — that is
the detector firing on something real (a tree, a curtain, a flickering lamp, or the
day→night ISP switch) and it is *the* thing to retune before the camera leaves the
house.

---

## J5 — The uplink goes away (15 min)

**Who:** you. **Goal:** A5. Done entirely from the homelab side, so it is safe.

1. On storage-01: `sudo systemctl stop camingest`.
2. Trigger motion in front of the camera (J3 step 2).
3. Wait one pass. **Expect:** the segment closes, the log shows
   `upload: 1 considered, 0 uploaded, 0 deleted, … 1 failed`, and the `.h264` is
   **still on the card**. Nothing is deleted on a failed upload — that is the
   invariant worth proving.
4. Repeat step 2 twice more so there are several pending segments.
5. `sudo systemctl start camingest`. Wait a pass.
   **Expect:** `N considered, N uploaded, N deleted, 0 failed`, and `tools/uat`
   reporting `card segments 0 h264` with the receiver's file count up by N.

**Also worth one run:** real network loss rather than a dead receiver — switch the
board's wifi off for a pass and back on. The failure mode should look identical
(`failed`, files kept). It has to be the board's own link, not the router's, or the
telnet session you are watching it through dies with it.

**Fail looks like:** `409`, or a stall of exactly 30 s before a failure — those are
the two receiver contract bugs listed in `analysis/offsite-ingest.md` (HEAD must
answer `Content-Length: 0`; a 409 must not echo the client's offset).

---

## J6 — A busy hour (the quota journey, ~1 h)

**Who:** you. **Goal:** A6. **This one is expected to find something.**

The arithmetic, from measured numbers:

| | |
|---|---|
| 720p segment, 60 s | ≈ 7.7 MB (0.97 Mbps) |
| upload ceiling | `--upload-max 8M` per 300 s pass ≈ **100 MB/hour** |
| recording ceiling, continuous motion | **≈ 465 MB/hour** |
| card quota | `--rec-quota 2G` ≈ **4.5 h** of continuous motion |

So on a *busy* hour the recorder can produce 4–5× more than the uplink can carry,
and `rec.rs`'s `evict()` deletes the oldest finished segments **without knowing
whether they were uploaded** (it protects only the open segment and the newest one).
`keep_days` is the opposite mistake: it skips expired footage and never deletes it,
so it silently stops sending after 14 days.

**Test it deliberately** rather than waiting for it to bite:

1. Stop camrtsp. Start one by hand with a small quota and a dead receiver:
   ```
   tools/cam -t 90 - <<'EOF'
   killall camrtsp
   /mnt/Factory/custom/camrtsp serve --source client:127.0.0.1:554/vs0 \
     --listen 0.0.0.0:8554 --rec /mnt/Factory/qtest --rec-quota 8M \
     --upload http://192.168.1.240:8080/ingest --upload-every 60 \
     --upload-token deadbeef >/tmp/qtest.log 2>&1 &
   EOF
   ```
2. Keep motion going (a hand moving in frame is enough) for ~10 minutes.
3. **Expect:** `rec quota evicted the oldest segment` in the log while the receiver
   is failing — i.e. footage lost un-uploaded.
4. Restore: `killall camrtsp`, `rm -rf /mnt/Factory/qtest`, start from `config.sh`.

**Decision this forces** (yours): either the uploader learns to protect pending
segments (`evict` skips anything the state file has not confirmed), or the quota goes
big and the pass budget goes up. A 29.7 GB card can hold ~1 h of continuous 720p per
GB… it is 4.5 h of continuous motion at 2 G, ~2 days at 2 G for *normal* traffic.

---

## J7 — Offsite, on LTE (the acceptance gate, half a day)

**Who:** you, with the E5885. **Goal:** A7 — the whole point.

This is the only journey where you cannot measure from firefly, because once the
camera is on the hotspot there is **no inbound path**: CGNAT, no port mapping, no
VPN server on the device. Everything below is observed from the homelab.

1. **Before you unplug anything** — put the hotspot's SSID/PSK into the camera the
   same way the house wifi got there (stock wifi page, or `/data/wifi_station.sh`
   with `/tmp/wifi_info`; never into the repo). Keep the house profile so you can
   come back.
2. On the card, nothing changes: `ingest.url` already points at
   `https://liu.ackl.me/ingest`, and the embedded Cloudflare roots mean no CA file
   is needed.
3. Power cycle onto the hotspot. Disconnect the house wifi.
4. From firefly, watch the receiver: `tools/uat` will report `board reachable FAIL`
   (expected — that is the point), and everything else should stay `ok`.
5. Walk in front of the camera and wait for the pass. Then on storage-01:
   `ffprobe` the new segment, and check the sidecar.

**Expect:** uploads keep arriving over the tunnel with the same sha-verified
behaviour, at whatever the LTE uplink sustains. **Record the numbers**: bytes/s for a
`--upload-max 8M` pass, and how long a pass takes. That converts the unknown
"8 MiB per 300 s is untested on cellular" into a measured figure, and decides whether
`--upload-max` needs raising or the interval shortening.

**Fail looks like:** `tlsprobe` failing (run `camrtsp tlsprobe liu.ackl.me` **before**
moving it), uploads failing but local recording fine (that is a working camera with a
broken uplink — still acceptable, never lost), or repeated `skipped-young` with a
climbing card count (the link is slower than the recording rate → J6's problem, now
real).

**The gap this journey exposes, unavoidably:** silence on the receiver means either
"nothing happened" or "the camera is dead", and from the homelab those look identical.
Right now nothing tells you which. A blank `GET` on the tunnel returns 401, the
camera sends no heartbeat, and there is no status object on the receiver. That is the
one feature UAT will demand before the camera can live somewhere you cannot reach —
see the gaps list below.

---

## J8 — Recovery (15 min, on the bench)

**Who:** you, with a card reader. **Goal:** A8.

* Pull the card mid-recording. Expected: the open `.h264` is playable up to the last
  flushed frame, and the `.json` is absent (the sidecar is written on close) — an
  `.h264` with no `.json` on the receiver is normal for a camera that lost power.
* Card back in: it boots and continues. Nothing on the card is precious; the app and
  the config are the only things that must survive, and both are in
  `backups/factory-sd-2026-10-25.tar` (`6b29ab48…`, see `tools/README.md`).
* Boot-config rollback is on the card already: `config.before-offsite.sh` (the
  pre-upload config) sits next to `config.sh`.
* Dead-end state (no telnet, no video): the card is removable, so recovery never
  needs the serial port — restore the tarball, boot, telnet on 2323.

---

## J9 — Someone else finds it (10 min)

**Who:** you. **Goal:** clear trust boundaries before it leaves the house.

* Port 2323 is an **unauthenticated root shell**. On the LAN that is a tool; offsite
  it must only ever exist behind the LTE CGNAT, never forwarded. Confirm nothing
  forwards it.
* The ingest endpoint is a shared secret, not an account: `401` without the token,
  and the token is the only thing standing between the internet and the receiver.
  It is not in git, not in `ps`, and Caddy now strips it from the access log — verify
  with a `grep` of `/data/access-liu.log` after a real upload.
* The card holds the token in clear (`custom/ingest.token`). Anyone who steals the
  camera can upload junk to the receiver. Rotate by writing both files and restarting.
* storage-01 has **no retention policy**; 570 GB free, no timers. Decide a delete
  policy before the card's quota becomes the only limit in the system.

---

## Gaps this plan already knows about

1. **No liveness signal** (J7). Silence is ambiguous. Cheapest fix: have the uploader
   `PUT` a small `status.json` every pass even when nothing was recorded — count,
   bytes, uptime, clock, motion state. Then "quiet" and "dead" are different rows on
   the homelab and `tools/uat` can check them from outside.
2. **Quota can evict un-uploaded footage** (J6) — `evict()` has no upload state.
3. **`keep_days` silently stops sending** after 14 days, and never deletes.
4. **Motion thresholds are untuned on night/IR** — the day→night shift is 1.65× and
   the threshold is 1.75×, which is thin.
5. **Only 3 s of pre-roll** — enough for "what happened", too short for "who was it"
   if the trigger fires on the tail of the movement. Cheap to raise to 6–8 s; costs
   the ring (~375 kB per 3 s, caps already allow 4 MiB).
6. **storage-01 has no retention**, no offsite copy of the offsite copy.
7. **The stock `rtsp` is still in the loop.** Everything above dies if it does, and
   it is closed-source. Route off it: `analysis/encoder-ownership.md`.

---

## Recording results

Keep a run log so "it worked yesterday" is checkable:

```
tools/uat --json >> ~/lab/anyka/work/uat-runs.jsonl
```

| Date | Journey | Result | Evidence / notes |
|---|---|---|---|
| 2026-11-08 23:46 | J0 preflight | pass | 15 ok, 1 warn (`no upload line yet` — 4 min into a 300 s cadence), 1280x720@15, receiver 52 files/179 MB, edge 401/405 |
| | J1 cold boot | | |
| | J2 live view | | |
| | J3 motion → homelab | | |
| | J4 quiet | | |
| | J5 uplink down | | |
| | J6 busy hour | | |
| | J7 LTE offsite | | |
| | J8 recovery | | |
| | J9 exposure | | |
