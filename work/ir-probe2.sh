echo ==lssysgpio==; ls -l /sys/user-gpio/
echo ==stat==; for g in IR_LED ircut_a ircut_b SPK_PA WHITE_LED; do printf "%s " "$g"; ls -ld /sys/user-gpio/$g; done
echo ==read==; for g in IR_LED ircut_a ircut_b; do printf "%s=" "$g"; cat /sys/user-gpio/$g 2>&1; echo " rc=$?"; done
