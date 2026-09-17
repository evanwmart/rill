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
# Kiosk mode (the display profile): if ~/kiosk.url names a page, the glass
# boots into that one document, full-screen and chromeless, no dock — the
# kiosk window kind: the compositor sizes it to the output. 1080p, not 4K:
# signage is read from across a room and the V3D has headroom to spare at
# that size. Remove the file to get the desktop back.
if [ -s "$HOME/kiosk.url" ]; then
  export RILL_DRM_MODE=1920x1080
  exec "$BIN/rill-compositor" --backend drm \
      "$BIN/rill-vector" --kiosk "$(cat "$HOME/kiosk.url")" \
      --data "$D/data" --identity "$D/identity-device" \
      --cache "$HOME/.cache/rill" --theme "$HOME/.config/rill/theme.toml"
fi
exec "$BIN/rill-compositor" --backend drm \
    "$BIN/rill-vector" --dock --data "$D/data" --identity "$D/identity-device"
