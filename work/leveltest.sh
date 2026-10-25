#!/bin/sh
# leveltest.sh -- ONE round, three loudness levels, amp correctly enabled.
#   1 blink  -> three clicks at -20 dBFS
#   2 blinks -> -30 dBFS
#   3 blinks -> -40 dBFS
LED=/mnt/Factory/custom/led
AUD=/mnt/Factory/custom/audio
Q=/mnt/Factory/custom/quiet-cue.sh

cue() { i=0; while [ $i -lt $1 ]; do $LED on >/dev/null 2>&1; sleep 0.18; $LED off >/dev/null 2>&1; sleep 0.18; i=$((i+1)); done; }

echo "LT_START $(date +%T)"
n=1
for l in 20 30 40; do
  echo "LEVEL$n blink=$n file=click$l"
  cue $n
  sleep 0.3
  $Q $AUD/click$l.pcm
  sleep 1.2
  n=$((n+1))
done
echo "LT_DONE $(date +%T)"
