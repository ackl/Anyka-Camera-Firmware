echo ==onvif-bin==; ls -l /mnt/Factory/custom/onvif; ls -l /etc/jffs2/*onvif* 2>/dev/null; ls /mnt/Factory/custom/*.xml /mnt/Factory/custom/web_interface/www/*onvif* 2>/dev/null
echo ==gpiotree==; find /sys/user-gpio -maxdepth 2 2>/dev/null | head -40
echo ==gpio-vals==; for g in IR_LED ircut_a ircut_b WHITE_LED; do echo -n "$g: "; for f in /sys/user-gpio/$g/*; do echo -n "$(basename $f)=$(cat $f 2>/dev/null | tr -d '\n') "; done; echo; done
echo ==leds==; ls /sys/class/leds/ 2>/dev/null
echo ==onvifstr==; grep -ac . /mnt/Factory/custom/onvif 2>/dev/null; grep -aoE "[ -~]{6,}" /mnt/Factory/custom/onvif 2>/dev/null | grep -iE "onvif|wsdl|http://|usage|port|discovery" | head -20
