#!/usr/bin/env bash
# 15b verification, the privileged half — VT switching needs console access,
# input needs the uinput module. Run once:  sudo bash ~/deploy/test-15b.sh
# Leaves labelled screenshots in /home/evan for the operator to pull.
set -u
home=/home/evan
mp=$(systemctl show rill-session -p MainPID --value)
[ -n "$mp" ] && [ "$mp" != "0" ] || { echo "rill-session not running"; exit 1; }
shot() { rm -f "$home"/rill-shot-*.png; kill -USR2 "$mp"; sleep 2; local f; f=$(ls -t "$home"/rill-shot-*.png 2>/dev/null | head -1); [ -n "$f" ] && cp "$f" "$home/$1"; echo "  -> $1"; }

echo "=== 0. baseline"; shot 15b-0-baseline.png

echo "=== 1. VT-switch survival (the risk this milestone was built around)"
cur=$(fgconsole 2>/dev/null || echo 1)
mark=$(date '+%Y-%m-%d %H:%M:%S')
echo "  foreground VT is $cur; switching to VT2 for 3s, then back"
chvt 2; sleep 3; chvt "$cur"; sleep 3
echo "  -- compositor's own words across the switch:"
journalctl -u rill-session --no-pager --since "$mark" 2>/dev/null \
  | grep -oiE "seat (disabled|enabled|is ours[^\"]*)|pausing|resuming|lighting HDMI[^ ]* at [0-9x@]*|modeset up" \
  | sed 's/^/     /' | tail -8
shot 15b-1-after-vt.png

echo "=== 2. input driving the desktop"
if modprobe uinput 2>/dev/null; then echo "  uinput module loaded"; else echo "  modprobe uinput FAILED"; fi
echo uinput | tee /etc/modules-load.d/rill-uinput.conf >/dev/null   # persist for 15c reboot
chgrp input /dev/uinput 2>/dev/null; chmod 0660 /dev/uinput 2>/dev/null
echo "  pointer -> screen centre (cursor should appear mid-frame):"
python3 "$home/deploy/uinput-inject.py" move 1900 1000
shot 15b-2-pointer.png
echo "  keyboard -> Ctrl+Shift+R (cycles the rice; look for a changed look):"
python3 "$home/deploy/uinput-inject.py" key LEFTCTRL,LEFTSHIFT,R
shot 15b-3-key.png

chown evan:evan "$home"/15b-*.png 2>/dev/null
echo "=== done. pull: scp rill-pi:~/15b-*.png ."
