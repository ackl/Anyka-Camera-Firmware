#!/bin/sh
# hog.sh SECONDS -- burn CPU for SECONDS on the board, then stop.
#
# Imitates the cold-boot storm (wpa_supplicant, telnetd, httpd, libraries being
# paged in off exFAT) without needing a reboot, so the same speaker trigger can
# be measured both idle and under contention.
D=${1:-15}
: > /tmp/hog.pids
i=0
while [ $i -lt 6 ]; do
    ( while :; do :; done ) &
    echo $! >> /tmp/hog.pids
    i=$((i + 1))
done
echo "HOG_START $(date +%T) pids=$(wc -l < /tmp/hog.pids)"
sleep "$D"
kill $(cat /tmp/hog.pids) 2>/dev/null
echo "HOG_END $(date +%T)"
