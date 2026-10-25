#!/bin/sh
# Audibility test with SHORT beeps (0.2 s x5 per phase).
# Round = three phases; the LED tells you which one is starting:
#   1 blink  -> SPK_PA held HIGH during the beeps
#   2 blinks -> SPK_PA held LOW  during the beeps
#   3 blinks -> driver default (no writes)
LED_GP=/sys/user-gpio/WHITE_LED
SPK=/sys/user-gpio/SPK_PA
export LD_LIBRARY_PATH=/mnt/Factory/custom/lib
AUD=/mnt/Factory/custom/audio
ROUNDS=${1:-10}

blink() {
  n=$1; i=0
  while [ $i -lt $n ]; do
    echo 1 > $LED_GP; sleep 0.12; echo 0 > $LED_GP; sleep 0.30
    i=$((i+1))
  done
  sleep 0.40
}

hold() {   # hold VALUE  -> keeps pin at VALUE for ~4 s in background
  v=$1; i=0
  while [ $i -lt 40 ]; do echo $v > $SPK 2>/dev/null; sleep 0.1; i=$((i+1)); done
}

play() { ak_adec_demo 16000 1 pcm $AUD/train.pcm >/dev/null 2>&1; }

echo "BT_START $(date +%T)"
sleep 20

r=0
while [ $r -lt $ROUNDS ]; do
  r=$((r+1)); echo "R${r} $(date +%T)"
  blink 1; echo "  p1_high $(date +%T)"; hold 1 & h=$!; play; kill $h 2>/dev/null; wait $h 2>/dev/null; echo "  p1_end $(date +%T)"; sleep 2
  blink 2; echo "  p2_low $(date +%T)";  hold 0 & h=$!; play; kill $h 2>/dev/null; wait $h 2>/dev/null; echo "  p2_end $(date +%T)"; sleep 2
  blink 3; echo "  p3_default $(date +%T)"; play; echo "  p3_end $(date +%T)"; sleep 3
done

echo 0 > $LED_GP
echo "BT_DONE $(date +%T)"
