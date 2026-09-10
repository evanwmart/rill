#!/usr/bin/env bash
# 15b VT-switch + input, against the RUNNING (active-seat) compositor — no
# restart (a mid-life restart does not reliably re-enter the active seat; a
# boot does, and this runs after one). Run: sudo bash ~/deploy/vt-test.sh
# Pull: scp rill-pi:~/vt-*.png .
set -u
home=/home/evan
mp=$(systemctl show rill-session -p MainPID --value)
# Guard: never signal pid 0/empty (that hits the whole process group).
{ [ -n "$mp" ] && [ "$mp" -gt 1 ] 2>/dev/null; } || { echo "no valid compositor MainPID ($mp) — is rill-session active?"; exit 1; }
sess=$(loginctl list-sessions --no-pager | awk '/seat0/ && $0 !~ /closing/ {print $1; exit}')
shot(){ rm -f "$home"/rill-shot-*.png; kill -USR2 "$mp"; sleep 2; local f; f=$(ls -t "$home"/rill-shot-*.png 2>/dev/null|head -1); [ -n "$f" ] && cp "$f" "$home/$1" && echo "  -> $1"; }
echo "compositor pid $mp, seat session $sess"

echo "=== VT-switch survival"
echo "  Active before : $(loginctl show-session "$sess" -p Active --value)"
shot vt-0-before.png
chvt 2; sleep 1
echo "  Active away   : $(loginctl show-session "$sess" -p Active --value)"
sleep 2; chvt 1; sleep 3
echo "  Active back   : $(loginctl show-session "$sess" -p Active --value)"
echo "  compositor alive after switch: $(kill -0 "$mp" 2>/dev/null && echo yes || echo NO)"
shot vt-1-after.png

echo "=== input"
modprobe uinput 2>/dev/null && echo "  uinput loaded"
chgrp input /dev/uinput 2>/dev/null; chmod 0660 /dev/uinput 2>/dev/null
python3 "$home/deploy/uinput-inject.py" move 1900 1000; shot vt-2-pointer.png
python3 "$home/deploy/uinput-inject.py" key LEFTCTRL,LEFTSHIFT,R; sleep 1; shot vt-3-key.png
chown evan:evan "$home"/vt-*.png 2>/dev/null
echo "=== done"
