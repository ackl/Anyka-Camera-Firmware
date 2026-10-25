#!/bin/sh
# Quiet A/B: is the amp SPK_PA active-HIGH or active-LOW?
# Sound = beep-q.pcm, 440 Hz, 1.5 s, -20 dBFS (deliberately quiet).
# LED cue before each sound: 1 blink = pin held LOW, 2 blinks = driver default (HIGH).
LED=/mnt/Factory/custom/led
SPK=/sys/user-gpio/SPK_PA
export LD_LIBRARY_PATH=/mnt/Factory/custom/lib
PCM=/mnt/Factory/custom/audio/beep-q.pcm

play() {  # $1 = low|high
  if [ "$1" = "low" ]; then
    ( i=0; while [ $i -lt 20 ]; do echo 0 > $SPK; sleep 0.1; i=$((i+1)); done ) &
  fi
  ak_adec_demo 16000 1 pcm $PCM >/dev/null 2>&1
  rc=$?
  [ "$1" = "low" ] && wait
  echo "  play $1 rc=$rc spkpa=$(cat $SPK)"
}

echo "AB2_START $(date +%T)"
sleep 2

echo "PHASE1_low_$(date +%T)"
$LED blink 1 150 150 >/dev/null 2>&1
play low
sleep 5

echo "PHASE2_high_$(date +%T)"
$LED blink 2 150 150 >/dev/null 2>&1
play high
sleep 5

echo "PHASE3_low_$(date +%T)"
$LED blink 1 150 150 >/dev/null 2>&1
play low

$LED ping >/dev/null 2>&1
echo "AB2_DONE $(date +%T)"
