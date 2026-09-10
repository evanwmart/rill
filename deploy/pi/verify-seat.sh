#!/usr/bin/env bash
# Finish 15b in the correct (post-boot, seat-active) environment: restart the
# session with logging on, confirm it came up in SEAT mode (not the Direct
# force-master fallback), then exercise a real VT switch and input.
# Run: sudo bash ~/deploy/verify-seat.sh   (pull: scp rill-pi:~/seat-*.png .)
set -u
home=/home/evan
: > "$home/rill-session.log"
install -m0755 -o evan -g evan "$home/deploy/rill-session.sh" "$home/deploy/rill-session.sh" 2>/dev/null || true
systemctl restart rill-session
sleep 6
mp=$(systemctl show rill-session -p MainPID --value)
sess=$(loginctl list-sessions --no-pager | awk '/seat0/{print $1; exit}')
shot(){ rm -f "$home"/rill-shot-*.png; kill -USR2 "$mp" 2>/dev/null; sleep 2; local f; f=$(ls -t "$home"/rill-shot-*.png 2>/dev/null|head -1); [ -n "$f" ]&&cp "$f" "$home/$1"; echo "  -> $1"; }
log(){ grep -aoE 'rill-compositor(\[drm\])?: .*|rill-vector: .*' "$home/rill-session.log" | grep -iE "$1"; }

echo "=== mode at startup (want: seat enabled / DRM master via seat, NOT 'never became active')"
log 'seat|master|Direct|lighting|never became' | head
echo "session $sess Active=$(loginctl show-session "$sess" -p Active --value)"
shot seat-0-baseline.png

echo "=== VT switch: away to VT2, back to VT1"
chvt 2; sleep 1; echo "  Active while away: $(loginctl show-session "$sess" -p Active --value)"
sleep 2; chvt 1; sleep 3; echo "  Active after back: $(loginctl show-session "$sess" -p Active --value)"
echo "  -- compositor across the switch:"; log 'pausing|resuming|seat (disabled|enabled)|modeset|lighting' | tail
shot seat-1-after-vt.png

echo "=== input"
modprobe uinput 2>/dev/null && echo "  uinput loaded"; chgrp input /dev/uinput 2>/dev/null; chmod 0660 /dev/uinput 2>/dev/null
python3 "$home/deploy/uinput-inject.py" move 1900 1000; shot seat-2-pointer.png
python3 "$home/deploy/uinput-inject.py" key LEFTCTRL,LEFTSHIFT,R; shot seat-3-key.png
chown evan:evan "$home"/seat-*.png 2>/dev/null
echo "=== done"
