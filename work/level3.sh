#!/bin/sh
# Level ladder: same 440Hz beep at -20 / -30 / -40 dBFS. Cue = N blinks for level N.
LED=/mnt/Factory/custom/led
export LD_LIBRARY_PATH=/mnt/Factory/custom/lib
A=/mnt/Factory/custom/audio
echo "LV_START $(date +%T)"
sleep 2
i=20; n=1
for lvl in 20 30 40; do
  echo "LEVEL$n $(date +%T)"
  $LED blink $n 250 250 >/dev/null 2>&1
  ak_adec_demo 16000 1 pcm $A/lvl$lvl.pcm >/dev/null 2>&1
  echo "LEVEL${n}_END rc=$?"
  sleep 4
  n=$((n+1))
done
echo "LV_DONE $(date +%T)"
