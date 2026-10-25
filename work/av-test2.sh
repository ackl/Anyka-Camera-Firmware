#!/bin/sh
# Play stock's own voice-prompt files through the vendor's own player.
LED=/mnt/Factory/custom/led
export LD_LIBRARY_PATH=/mnt/Factory/custom/lib

echo "START $(date +%T)"
ls -l /data/audio_file/language/1.aac 2>&1

$LED ping
sleep 2

echo "AAC8K_START $(date +%T)"
ak_adec_demo 8000 1 aac /data/audio_file/language/1.aac
echo "AAC8K_END $(date +%T) rc=$?"

$LED ping
sleep 1

echo "AAC16K_START $(date +%T)"
ak_adec_demo 16000 1 aac /data/audio_file/language/1.aac
echo "AAC16K_END $(date +%T) rc=$?"

$LED ping
echo "DONE $(date +%T)"
