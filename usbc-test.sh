#!/bin/sh
# USB-C thử nghiệm (25/11). Chạy bằng root.
#   sh usbc-test.sh install   tải ps5170.ko + boot_b mới, sao lưu boot_b cũ vào /root, ghi boot_b mới
#   sh usbc-test.sh state     nạp module và in trạng thái USB-C (chạy sau khi khởi động lại)
#   sh usbc-test.sh restore   ghi lại boot_b cũ từ /root/boot_b-backup.img
set -u
U=https://raw.githubusercontent.com/ducdx86/buntos-TB375FC/debug-tools
K=$(uname -r)
BOOT=/dev/disk/by-partlabel/boot_b
NEW_SHA=a623d245d81a1cd69989e046907e2445035aab4e15a1cef787b7b4e04b43c042
OLD_SHA=427808c2fe5c04ba67fba6f74d5d3d2244a7f01f2e109a9091211833cba89de4
KO_SHA=bba7bd14fd87e0bd955b7b5bf4bd8bb607024eafaacef8e4b236589337c2c008
case "${1:-state}" in
install)
	[ -b "$BOOT" ] || { echo "KHONG THAY $BOOT"; exit 1; }
	mkdir -p /usr/lib/modules/$K/updates
	curl -sfL -o /tmp/ps5170.ko $U/ps5170.ko && echo "$KO_SHA  /tmp/ps5170.ko" | sha256sum -c - || exit 1
	curl -sfL -o /tmp/boot_b-usbc.img $U/boot_b-usbc.img && echo "$NEW_SHA  /tmp/boot_b-usbc.img" | sha256sum -c - || exit 1
	SZ=$(stat -c %s /tmp/boot_b-usbc.img)
	[ -f /root/boot_b-backup.img ] || dd if=$BOOT of=/root/boot_b-backup.img bs=1M status=none
	CUR=$(head -c $SZ $BOOT | sha256sum | cut -d' ' -f1)
	echo "boot_b hien tai: $CUR"
	[ "$CUR" = "$OLD_SHA" ] && echo "(dung ban boot_b-full cu)"
	install -m644 /tmp/ps5170.ko /usr/lib/modules/$K/updates/ps5170.ko && depmod -a
	dd if=/tmp/boot_b-usbc.img of=$BOOT bs=1M conv=fsync status=none && sync
	NOW=$(head -c $SZ $BOOT | sha256sum | cut -d' ' -f1)
	[ "$NOW" = "$NEW_SHA" ] && echo "XONG: da ghi boot_b moi. Khoi dong lai." || echo "LOI: doc lai khong khop"
	;;
restore)
	dd if=/root/boot_b-backup.img of=$BOOT bs=1M conv=fsync status=none && sync && echo "da ghi lai boot_b cu"
	;;
state)
	for m in tcpci_mt6375 ps5170 typec_displayport; do modprobe $m 2>&1; done
	sleep 2
	echo "== module"; lsmod | grep -E '^(tcpci|tcpm|typec|ps5170|xhci|mtu3)' | cut -c1-40
	echo "== typec"
	for p in /sys/class/typec/port[0-9]; do
		[ -d "$p" ] || continue
		for f in power_role data_role orientation power_operation_mode; do printf '%s %s: ' "${p##*/}" $f; cat $p/$f 2>/dev/null || echo -; done
	done
	ls /sys/class/typec/ /sys/class/typec_mux/ 2>/dev/null | tr '\n' ' '; echo
	echo "== usb role"; for r in /sys/class/usb_role/*; do echo "${r##*/}: $(cat $r/role)"; done
	echo "== regulator"
	for r in /sys/class/regulator/regulator.*; do n=$(cat $r/name); case "$n" in *otg*|*vbus*|*VBUS*|*usb*) echo "$n: $(cat $r/state 2>/dev/null) $(cat $r/num_users 2>/dev/null)";; esac; done
	echo "== usb"; for d in /sys/bus/usb/devices/*; do [ -f $d/product ] && echo "${d##*/}: $(cat $d/product) $(cat $d/speed 2>/dev/null)M"; done
	lsblk -o NAME,SIZE,TRAN,MODEL 2>/dev/null | grep -i usb
	echo "== tcpm log"; mount -t debugfs none /sys/kernel/debug 2>/dev/null; tail -n 25 /sys/kernel/debug/usb/tcpm*/log 2>/dev/null || cat /sys/kernel/debug/usb/tcpm* 2>/dev/null | tail -n 25
	echo "== dmesg"; dmesg | grep -iE 'tcpm|tcpci|typec|ps5170|xhci|mtu3|usb [0-9]|vbus|otg|sd[a-z]:' | tail -n 30
	;;
esac
