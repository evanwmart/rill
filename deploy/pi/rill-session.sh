#!/usr/bin/env bash
# The Rill kiosk session (milestone 15c): what an autologin or the
# rill-session.service execs on the console. Starts the demo server, then
# the compositor on the DRM backend hosting the dock. Runs in the
# foreground so systemd owns its lifetime — when this exits, the service
# restarts it. Assumes a seat (the service gives it one; see the unit).
set -u
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
D="$HOME/.local/share/rill-demo"
BIN="$HOME/metal"

# The data server (the widget's source). Backgrounded, its own session.
if ! ss -ltn 2>/dev/null | grep -q ":7420 "; then
  setsid "$HOME/soak/bin/files-app" "$D/content" --identity "$D/identity-server" \
      --writable "$D/content/work" --bind 127.0.0.1 --port 7420 \
      >/tmp/rill-files.log 2>&1 </dev/null &
  for _ in $(seq 1 40); do ss -ltn 2>/dev/null | grep -q ":7420 " && break; sleep 0.2; done
fi

# The compositor in the FOREGROUND: systemd watches this pid. History on
# (a kiosk is a system of record); RILL_SHOT_DIR lets SIGUSR2 dump a frame.
export RILL_SHOT_DIR="$HOME"
exec "$BIN/rill-compositor" --backend drm \
    "$BIN/rill-vector" --dock --data "$D/data" --identity "$D/identity-device"
