#!/bin/sh
# /mnt/Factory/custom/beep -- play a notification cue.
# On the camera:  /mnt/Factory/custom/beep [cue|warn|alarm]
# From firefly:   tools/beep [cue|warn|alarm]
#
# Levels baked into audio/<level>.pcm (see gen-cues.py); chosen by ear:
#   cue    0.36 s rising two-tone, -40 dBFS  everyday ("I am up", ack)
#   warn   0.43 s three pips,      -30 dBFS  attention, still not painful
#   alarm  0.74 s alternating,     -20 dBFS  EMERGENCY ONLY -- user asked this
#                                            level be reserved for critical
#
# Goes through quiet-cue.sh, which holds the active-low amp enable pin at 0;
# a bare ak_adec_demo is silent because the driver mutes while it plays.
L=${1:-cue}
AUD=/mnt/Factory/custom/audio
case "$L" in
    cue|warn|alarm) ;;
    *) echo "usage: beep [cue|warn|alarm]"; exit 2 ;;
esac
[ -f "$AUD/$L.pcm" ] || { echo "beep: missing $AUD/$L.pcm"; exit 2; }
exec /mnt/Factory/custom/quiet-cue.sh "$AUD/$L.pcm"
