#!/bin/sh
# A/B test: same sound twice, differing only in the SPK_PA amp-enable level.
#   Phase A = SPK_PA held LOW for the whole playback (inverted-amp hypothesis)
#   Phase B = SPK_PA left to the driver (goes HIGH during playback, as usual)
# Sound = /mnt/Factory/custom/audio/pattern.pcm: three 2 s 1 kHz beeps, 1 s apart.
# LED cue: 1 blink = next sound is phase A, 2 blinks = phase B.
LED=/mnt/Factory/custom/led
export LD_LIBRARY_PATH=/mnt/Factory/custom/lib
DEC="ak_adec_demo 16000 1 pcm /mnt/Factory/custom/audio/pattern.pcm"
SPK=/sys/user-gpio/SPK_PA

echo "AB_START $(date +%T) spkpa=$(cat $SPK)"
sleep 3

echo "A_LOW_START $(date +%T)"
$LED blink 1 120 120 >/dev/null 2>&1
( i=0; while [ $i -lt 70 ]; do echo 0 > $SPK; sleep 0.2; i=$((i+1)); done ) &
$DEC >/dev/null 2>&1
echo "A_LOW_END rc=$? spkpa=$(cat $SPK) $(date +%T)"

sleep 3
echo "B_HIGH_START $(date +%T)"
$LED blink 2 120 120 >/dev/null 2>&1
$DEC >/dev/null 2>&1
echo "B_HIGH_END rc=$? spkpa=$(cat $SPK) $(date +%T)"

$LED ping >/dev/null 2>&1
echo "AB_DONE $(date +%T)"
