#!/bin/sh
# Recording indicator: WHITE_LED blinks once per second for N seconds.
# Usage: mic-indicator.sh [SECONDS]
LED=/mnt/Factory/custom/led
N=${1:-40}
i=0
while [ $i -lt $N ]; do
  $LED on  >/dev/null 2>&1
  sleep 0.5
  $LED off >/dev/null 2>&1
  sleep 0.5
  i=$((i+1))
done
echo "indicator done $(date +%T)"
