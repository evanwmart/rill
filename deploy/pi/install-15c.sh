#!/usr/bin/env bash
# The privileged half of 15b/15c, batched for one review + run. Installs the
# kiosk session service (logind seat0 on tty1), disables the display manager
# so Rill owns the card at boot, and opens /dev/uinput to the input group for
# synthetic-input testing. Idempotent; prints what it did. RUN AS ROOT:
#   sudo bash ~/deploy/install-15c.sh
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# rill-session.sh already lives at ~/deploy/ (where this script runs from);
# the service execs it in place. Just make sure it is executable and owned
# by evan, rather than copying it onto itself.
chmod 0755 "$here/rill-session.sh"; chown evan:evan "$here/rill-session.sh"
install -m 0644 "$here/rill-session.service" /etc/systemd/system/rill-session.service
install -m 0644 "$here/99-rill-uinput.rules" /etc/udev/rules.d/99-rill-uinput.rules

udevadm control --reload-rules && udevadm trigger /dev/uinput || true
chgrp input /dev/uinput 2>/dev/null || true
chmod 0660 /dev/uinput 2>/dev/null || true

systemctl daemon-reload
# Do NOT auto-start here: starting rill-session while a compositor is already
# running by hand would fight over the card. Enable it, and the operator
# starts it (or reboots) when ready.
systemctl disable --now lightdm.service || true
systemctl enable rill-session.service

cat <<'DONE'

installed. next, either:
  sudo systemctl start rill-session      # bring Rill up on seat0/tty1 now
  # or reboot — graphical.target now starts rill-session instead of lightdm

to undo:
  sudo systemctl disable --now rill-session
  sudo systemctl enable --now lightdm
DONE
