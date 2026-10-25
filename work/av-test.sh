#!/bin/sh
# A/V presence test: LED markers around two audible tones.
# LED = /sys/user-gpio/WHITE_LED via /mnt/Factory/custom/led (restores state itself).
LED=/mnt/Factory/custom/led
AUD=/mnt/Factory/custom/audio
export LD_LIBRARY_PATH=/mnt/Factory/custom/lib

echo "START $(date +%T)"

# 1) marker: 3 quick blinks = "starting now"
$LED ping

# 2) 400 Hz, 30 s  (should be plainly audible / piercing)
echo "TONE400_START $(date +%T)"
ak_adec_demo 16000 1 pcm $AUD/tone400.pcm
echo "TONE400_END $(date +%T) rc=$?"

# marker again
$LED ping
sleep 2

# 3) 150 Hz, 30 s (low hum - tests a muffled/small speaker)
echo "TONE150_START $(date +%T)"
ak_adec_demo 16000 1 pcm $AUD/tone150.pcm
echo "TONE150_END $(date +%T) rc=$?"

# 4) final marker
$LED ping
echo "DONE $(date +%T)"
