echo 1 > /sys/user-gpio/ircut_a; sleep 0.4; echo 0 > /sys/user-gpio/ircut_a
echo "state: IR_LED=$(cat /sys/user-gpio/IR_LED) a=$(cat /sys/user-gpio/ircut_a) b=$(cat /sys/user-gpio/ircut_b)"
echo "BEAT"
