R=/data/recovery
mkdir -p $R/custom/lib $R/custom/audio || exit 1
cp -p /mnt/Factory/custom/rtsp                  $R/custom/rtsp        || exit 2
cp -p /mnt/Factory/custom/lib/libplat_drv.so    $R/custom/lib/libplat_drv.so || exit 3
cp -p /mnt/Factory/custom/beep                  $R/custom/beep        || exit 4
cp -p /mnt/Factory/custom/audio/cue.pcm         $R/custom/audio/cue.pcm   || exit 5
cp -p /mnt/Factory/custom/audio/warn.pcm        $R/custom/audio/warn.pcm  || exit 6
cp -p /mnt/Factory/custom/audio/alarm.pcm       $R/custom/audio/alarm.pcm || exit 7
echo "== stage1 copied =="
for f in custom/rtsp custom/lib/libplat_drv.so custom/beep custom/audio/cue.pcm custom/audio/warn.pcm custom/audio/alarm.pcm; do
  s=$(sha256sum /mnt/Factory/$f | cut -d" " -f1)
  d=$(sha256sum $R/$f | cut -d" " -f1)
  if [ "$s" = "$d" ]; then echo "OK   $(wc -c < $R/$f) $d $f"; else echo "FAIL $f src=$s dst=$d"; fi
done
echo "== df =="
df -k /data
