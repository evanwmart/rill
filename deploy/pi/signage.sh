#!/usr/bin/env bash
# The signage server on the glass: start it, pin it, point the kiosk at it.
#
#   deploy/pi/signage.sh gate|airport|ad|museum   # start (or restart) + choose the page
#   deploy/pi/signage.sh reset                    # restart the server: the demo loop starts over
#   deploy/pi/signage.sh gate --from HOST:PORT    # read the page from another machine's signage
#                                                 # server instead (pins it; local server left alone)
#   deploy/pi/signage.sh stop                  # stop the server; kiosk.url stays
#   deploy/pi/signage.sh feed down|up          # the airport board's feed switch
#
# First run creates the server's own identity (cert + key) under
# ~/.local/share/rill-signage/identity with the public policy shipped in
# apps/signage-app/policy.toml, starts signage-app on 127.0.0.1:7440, and
# pins its fingerprint for the device identity the kiosk client uses. Then
# it writes ~/kiosk.url so rill-session.sh boots into that page; changing
# the page later is this script again plus a session restart
# (`sudo systemctl restart rill-session`).
#
# Binaries: ~/metal/signage-app and ~/soak/bin/rill (the CLI, for `auth`).
set -eu
BIN="$HOME/metal"
RILL="$HOME/soak/bin/rill"
BASE="$HOME/.local/share/rill-signage"
ID="$BASE/identity"
DATA="$BASE/data"
DEVICE="$HOME/.local/share/rill-demo/identity-device"
PORT=7440
LOG="$BASE/signage.log"

stop() {
  pkill -x signage-app 2>/dev/null && echo "stopped signage-app" || true
}

# `--from HOST:PORT`: the glass as pure glass — the page comes from a
# signage server elsewhere (the workstation, say: `signage-app --bind
# 0.0.0.0 --demo` there, and its firewall open on the port). Pinned for
# the device identity like the local one; the local server is not touched.
FROM=""
if [ "${2:-}" = "--from" ] && [ -n "${3:-}" ]; then FROM=$3; fi
case "${1:-}" in
  stop) stop; exit 0 ;;
  feed)
    mkdir -p "$DATA"
    case "${2:-}" in
      down) touch "$DATA/feed-down"; echo "feed down (the board goes stale)" ;;
      up) rm -f "$DATA/feed-down"; echo "feed up" ;;
      *) echo "usage: $0 feed down|up" >&2; exit 2 ;;
    esac
    exit 0 ;;
  gate|airport|ad|museum) page=$1 ;;
  reset) page=$(sed 's|.*/||' "$HOME/kiosk.url" 2>/dev/null); page=${page:-gate} ;;
  *) echo "usage: $0 gate|airport|ad|museum | reset | stop | feed down|up" >&2; exit 2 ;;
esac

if [ -n "$FROM" ]; then
  "$RILL" auth trust "rill://$FROM" --identity "$DEVICE" --yes | tail -1
  echo "rill://$FROM/$page" > "$HOME/kiosk.url"
  echo "kiosk.url → $(cat "$HOME/kiosk.url")"
  echo "restart the session to show it: sudo systemctl restart rill-session"
  exit 0
fi

mkdir -p "$BASE" "$DATA"
if [ ! -s "$ID/server-cert.pem" ]; then
  "$RILL" auth init-server "$ID" --name signage
fi
# The policy travels with the app; the identity dir gets a copy each run so
# an edit to the shipped file takes effect on the next start.
install -m 0644 "$(dirname "$0")/signage-policy.toml" "$ID/policy.toml"

stop
# Detached from this shell (and from SSH): its own session, logs to $BASE.
# --demo: the gate runs its scripted loop (one state a minute, a new
# flight every eight); drop it for real-world pacing.
setsid nohup "$BIN/signage-app" --identity "$ID" --data "$DATA" --port "$PORT" --demo \
  >> "$LOG" 2>&1 < /dev/null &
sleep 1
pgrep -x signage-app > /dev/null || { echo "signage-app did not start; see $LOG" >&2; tail -5 "$LOG" >&2; exit 1; }

# Pin the server for the kiosk's device identity (idempotent).
"$RILL" auth trust "rill://127.0.0.1:$PORT" --identity "$DEVICE" --yes | tail -1
echo "rill://127.0.0.1:$PORT/$page" > "$HOME/kiosk.url"
echo "kiosk.url → $(cat "$HOME/kiosk.url")"
echo "restart the session to show it: sudo systemctl restart rill-session"
