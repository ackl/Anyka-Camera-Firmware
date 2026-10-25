echo ==www==; ls /mnt/Factory/custom/web_interface/www | head -40
echo ==cgi==; ls -l /mnt/Factory/custom/web_interface/www/cgi-bin 2>/dev/null | head; find /mnt/Factory/custom/web_interface -name "*.cgi" -o -name "cgi*" -type d 2>/dev/null | head
echo ==html==; ls /mnt/Factory/custom/web_interface/www/*.html /mnt/Factory/custom/web_interface/www/**/*.html 2>/dev/null | head -20
echo ==cfg==; ls -l /mnt/Factory/custom/web_interface/ 2>/dev/null
