#!/bin/sh
# Three plain plays, no pin manipulation: does the normal path make sound?
export LD_LIBRARY_PATH=/mnt/Factory/custom/lib
PCM=/mnt/Factory/custom/audio/beep-q.pcm
echo "P_START $(date +%T) spkpa=$(cat /sys/user-gpio/SPK_PA)"
sleep 2
i=1
while [ $i -le 3 ]; do
  echo "PLAY$i $(date +%T) spkpa_before=$(cat /sys/user-gpio/SPK_PA)"
  ak_adec_demo 16000 1 pcm $PCM >/dev/null 2>&1
  echo "PLAY$i_END rc=$? $(date +%T) spkpa_after=$(cat /sys/user-gpio/SPK_PA)"
  sleep 4
  i=$((i+1))
done
echo "P_DONE $(date +%T)"
