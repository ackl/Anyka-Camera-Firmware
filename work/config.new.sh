#!/bin/sh

export LD_LIBRARY_PATH=/mnt/Factory/custom/lib

# Boot LED guard: come up dark.  The patched libplat_drv.so reads the photo
# level from /tmp/photo (0=day, 1=night); with that file MISSING it falls back
# to /sys/kernel/ain/ain0 (reads ~1171 here, it is not a light sensor), decides
# "night", and the PS_switch thread inside rtsp switches the IR illuminator on
# ~5 s after start -- the red glow seen at boot.  Writing 0 here makes the thread
# do a real day switch (IR_LED=0, ircut_a=1, ISP day) and stay there.
# The two echo lines are belt-and-braces; ircut_a/b are latching-solenoid coils
# with no pin memory, so they are deliberately not touched.
printf '0\n' > /tmp/photo
echo 0 > /sys/user-gpio/IR_LED
echo 0 > /sys/user-gpio/WHITE_LED

/usr/sbin/net_manage.sh &
/mnt/Factory/custom/busybox telnetd -p 2323 -l /bin/sh &
/mnt/Factory/custom/busybox httpd -p 8080 -h /mnt/Factory/custom/web_interface/www &

# Startup chime (SD-card audio): rising three-tone "I am up" cue, -40 dBFS,
# 1.06 s (audio/boot40.pcm).  Two things learned the hard way on 2026-10-25:
#
#  1. It must NOT fire at t=0.  Played immediately, the cue was inaudible at
#     boot although it played: the decoder ran 2-3x slower than realtime
#     (quiet-cue ticks=40 for a 0.36 s file, vs ticks=14 on demand) while
#     wpa_supplicant/telnetd/httpd were starting and ak_adec_demo plus its libs
#     were being read off the exFAT card.  A starved DMA feeder emits silence
#     and still logs "playback_close OK".  So: give the rush 5 s first.
#  2. It must be long enough to survive the amp's mute window.  The old 0.36 s
#     cue at -40 dBFS is audible when listened for, but has no margin.
#
# Goes through quiet-cue.sh because the driver mutes the ACTIVE-LOW amp while it
# plays (SPK_PA=1 on IOC_SET_SPEAKER); quiet-cue holds the pin at 0.  The log is
# kept so the next boot can be measured: /tmp/quiet-cue.boot.log holds the
# rc/secs/iters/ticks line.  Non-fatal by design -- no "&&", nothing depends on it.
( sleep 5
  if [ -x /mnt/Factory/custom/quiet-cue.sh ]; then
      /mnt/Factory/custom/quiet-cue.sh /mnt/Factory/custom/audio/boot40.pcm >/tmp/quiet-cue.boot.log 2>&1
  else
      ak_adec_demo 16000 2 pcm /mnt/Factory/custom/audio/chime.pcm >/dev/null 2>&1
  fi ) &

# Disabled automatic homing: /mnt/Factory/custom/ptz_daemon &
sleep 30

# Clock.  There is no RTC on this board: it boots at 1970-01-01, which named
# every recording 19700101-*.  The house router answers NTP; the server is a
# literal IP because DNS here is unreliable (pool.ntp.org does not resolve,
# though 8.8.8.8 pings fine).  Backgrounded so a dead NTP server can never stop
# the video, and stepped before the recorder names anything.
# Note: ntpd exits 0 even when it fails to set the clock, and busybox has no
# `timeout`; check `date`, not the exit code.
( ntpd -q -p 192.168.1.254 >/tmp/ntp.log 2>&1; date >/tmp/ntp.date ) &
sleep 2

/mnt/Factory/custom/rtsp &

# Our own server on top of the stock encoder: re-serves /vs0 on :8554, records
# motion-gated segments to the card, and pushes them to the homelab.  The
# upstream is the stock rtsp above, so this must start after it.
#
# The ingest URL lives in a file so switching between the LAN receiver and the
# public Cloudflare endpoint (https://liu.ackl.me/ingest) needs no edit here.
# The token comes from the environment, not argv, so it stays out of `ps`.
# Both are optional: without them it still records, it just never uploads.
INGEST_URL=$(cat /mnt/Factory/custom/ingest.url 2>/dev/null)
if [ -x /mnt/Factory/custom/camrtsp ] && [ -n "$INGEST_URL" ]; then
    CAMRTSP_UPLOAD_TOKEN=$(cat /mnt/Factory/custom/ingest.token 2>/dev/null) \
    /mnt/Factory/custom/camrtsp serve \
        --source client:127.0.0.1:554/vs0 \
        --listen 0.0.0.0:8554 \
        --rec /mnt/Factory/rec --rec-quota 2G \
        --motion-log \
        --upload "$INGEST_URL" --upload-every 300 \
        >/tmp/camrtsp.log 2>&1 &
fi

/mnt/Factory/custom/vpn/vpn.sh &
