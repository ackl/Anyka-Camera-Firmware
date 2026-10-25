#!/bin/sh

export LD_LIBRARY_PATH=/mnt/Factory/custom/lib

/usr/sbin/net_manage.sh &
/mnt/Factory/custom/busybox telnetd -p 2323 -l /bin/sh &
ak_adec_demo 16000 2 aac /usr/share/audio_file/common/didi.aac
# Disabled: daemon auto-homes/moves and persists position internally.
# /mnt/Factory/custom/ptz_daemon &
/mnt/Factory/custom/busybox httpd -p 8080 -h /mnt/Factory/custom/web_interface/www &
sleep 30
/mnt/Factory/custom/rtsp &
/mnt/Factory/custom/vpn/vpn.sh &
