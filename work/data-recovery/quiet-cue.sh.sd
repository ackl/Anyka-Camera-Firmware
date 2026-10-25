#!/bin/sh
# quiet-cue.sh FILE [RATE] [CHANNELS] -- play a PCM file with the amp actually on.
#
# Why this exists: the AK3918 audio driver raises SPK_PA to 1 for the duration of
# a playback (IOC_SET_SPEAKER value=1, "enable speaker").  The amp on this board
# is ACTIVE-LOW -- 0 = amp on, 1 = muted -- so the driver mutes the speaker
# exactly while it is trying to play.  Every plain `ak_adec_demo` run is silent.
#
# So: hold SPK_PA at 0 for the whole playback (a keeper loop re-asserts it every
# 50 ms because the driver flips it), then restore 1 (muted) afterwards.
# Verified by ear on 2026-10-24: 2 LED blinks (pin held low) is the only audible
# phase of the beep-train test; plain plays made no sound the user could hear.
#
# Levels are baked into the file, not passed here: the decoder hardcodes
# DAC 6 / DEV 5 / ASLC 6 and accepts no volume argument.  See gen-cues.py.
#
# Keeper length (2026-10-25).  It used to be a flat 200-iteration cap (10 s
# nominal), which silently dropped the amp mid-play for any longer sound.  It is
# now sized from the file itself -- s16le is RATE*CH*2 bytes per second:
#
#     SECS = ceil(bytes / (RATE*CH*2)) + 2        (+2 s decoder start/drain)
#
# capped at MAXSECS so a bogus file or rate cannot pin the amp forever.  One
# iteration really costs ~75 ms here (50 ms sleep + fork), so counting 20
# iterations per second keeps the keeper *longer* than the nominal estimate --
# it must never be shorter than the playback.  A wall-clock check is the hard
# ceiling, independent of how fast the loop actually spins.

SPK=/sys/user-gpio/SPK_PA
TICKS=/tmp/spk.ticks
MAXSECS=120
F="$1"
RATE=${2:-16000}
CH=${3:-1}

[ -n "$F" ] || { echo "usage: quiet-cue.sh FILE [RATE] [CHANNELS]"; exit 2; }
[ -f "$F" ] || { echo "quiet-cue: no such file: $F"; exit 2; }

# --- keeper duration: bytes -> seconds of audio -----------------------------
case "$RATE" in ''|*[!0-9]*) RATE=16000 ;; esac
case "$CH" in ''|*[!0-9]*) CH=1 ;; esac
[ "$RATE" -gt 0 ] 2>/dev/null || RATE=16000
[ "$CH" -gt 0 ] 2>/dev/null || CH=1
BPS=$((RATE * CH * 2))
[ "$BPS" -gt 0 ] 2>/dev/null || BPS=32000

BYTES=$(wc -c < "$F" 2>/dev/null)
BYTES=$(echo $BYTES)                       # busybox wc may pad
case "$BYTES" in ''|*[!0-9]*) BYTES=0 ;; esac
if [ "$BYTES" -gt 0 ]; then
    SECS=$(( (BYTES + BPS - 1) / BPS + 2 ))
else
    SECS=3                                  # unknown size: treat as a short cue
fi
[ "$SECS" -gt "$MAXSECS" ] && SECS=$MAXSECS
[ "$SECS" -lt 1 ] && SECS=1
ITERS=$((SECS * 20))                        # 20 x 50 ms nominal per second
CEILING=$(( $(cut -d. -f1 /proc/uptime) + SECS ))   # hard wall-clock limit

# NO LED CUE (removed 2026-10-25, board time 00:10).  This script used to do
# `led on` before the play and `led off` after.  That is fine for a 0.36 s chime
# and wrong for anything longer: around a 62 s silent file it lit the front of
# the device, in the user's room, for a full minute -- twice.  A cue must never
# be steady-on for the duration of the play, and the white lamp is not ours to
# drive from an audio helper.  So: this beeps, and writes no lamp pin at all.

restore() {
    touch /tmp/spkstop 2>/dev/null
    kill $KEEPER 2>/dev/null
    echo 1 > $SPK 2>/dev/null
}
trap 'restore; exit 130' INT TERM HUP

rm -f /tmp/spkstop
echo 0 > $TICKS
# Keeper: pin low for the whole play.  Bounded by ITERS (the file's own length)
# and by the wall clock (CEILING).  Progress goes to $TICKS so a long play can
# be watched from outside.  It restores the pin itself too, in case the parent
# is killed outright.
( i=0
  while [ $i -lt $ITERS ] && [ ! -f /tmp/spkstop ]; do
      echo 0 > $SPK 2>/dev/null
      i=$((i+1))
      if [ $((i % 20)) -eq 0 ]; then
          echo $i > $TICKS
          [ "$(cut -d. -f1 /proc/uptime)" -ge $CEILING ] && break
      fi
      sleep 0.05
  done
  echo $i > $TICKS
  echo 1 > $SPK 2>/dev/null ) &
KEEPER=$!
sleep 0.15   # let the keeper win the race before the driver asserts anything

LD_LIBRARY_PATH=/mnt/Factory/custom/lib ak_adec_demo $RATE $CH pcm "$F" >/dev/null 2>&1
rc=$?

touch /tmp/spkstop
sleep 0.15
kill $KEEPER 2>/dev/null
wait 2>/dev/null
echo 1 > $SPK 2>/dev/null      # back to muted while idle
echo "quiet-cue $(basename "$F") rc=$rc spk=$(cat $SPK) secs=$SECS iters=$ITERS ticks=$(cat $TICKS 2>/dev/null) $(date +%T)"
exit $rc
