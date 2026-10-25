echo ==onvif-dir==; ls -l /mnt/Factory/custom/onvif/ 2>&1 | head -20
echo ==onvif-files==; find /mnt/Factory/custom/onvif -maxdepth 2 -type f 2>/dev/null | head -20
echo ==ir-sysfs==; ls /sys/class/ 2>/dev/null | tr '\n' ' '; echo
echo ==sys-ir==; find /sys -maxdepth 4 \( -iname '*ir' -o -iname '*ircut*' -o -iname '*led*' \) 2>/dev/null | head -20
echo ==sys-ak==; ls /sys/ak_info_dump/ 2>/dev/null
echo ==modules==; ls /sys/module 2>/dev/null | tr '\n' ' '; echo
echo ==meminfo==; head -5 /proc/meminfo
