#!/usr/bin/env bash
# First run 2026-09-29 (workstation, release): profile at target/bench/profiles/ws-busy.json.gz.
# Stand up the hermetic bench desktop (same root/env as scripts/bench-stack.sh run)
# with the compositor under samply. Usage: profile-stack.sh <label> [secs]
# Needs: kernel.perf_event_paranoid <= 1, release build, samply on PATH.
set -euo pipefail
repo=/home/evan/Workspaces/nylumic/rill
label=$1; secs=${2:-60}
bench=${RILL_BENCH_ROOT:-$repo/target/bench}; root=$bench/stack; port=${RILL_BENCH_PORT:-7521}
out=${PROFILE_OUT:-$bench/profiles}; mkdir -p "$out" "$root/config/rill" "$root/cache"
export RILL_DEMO_ROOT=$root/demo RILL_DEMO_PORT=$port RILL_CACHE=$root/cache XDG_CONFIG_HOME=$root/config
export RILL_BENCH_LOG=$root/demo/server.log RILL_BENCH_DESKTOP_LOG=$root/desktop.log RILL_BENCH_SCOPE=$root
export RILL_BENCH_PROFILE=release
[[ -f $root/config/rill/theme.toml ]] || sed -n '/<<THEME/,/^THEME/p' "$repo/scripts/bench-stack.sh" | sed '1d;$d' | sed "s/\$port/$port/g" > "$root/config/rill/theme.toml"
"$repo/scripts/demo-desktop.sh" > "$root/setup.log" 2>&1 || { echo "setup failed, see $root/setup.log"; exit 1; }
if [[ -z ${WAYLAND_DISPLAY:-} ]]; then
  sock=$(ls "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"/wayland-[0-9] | head -1); export WAYLAND_DISPLAY=$(basename "$sock")
fi
bin=$repo/target/release
echo "==> launching desktop under samply ($secs s + 12 s settle)"
( LD_LIBRARY_PATH=/usr/lib64 samply record -s -r "${RATE:-997}" -o "$out/$label.json.gz" -- "$bin/rill-compositor" "$bin/rill-vector" --dock \
    --data "$root/demo/data" --identity "$root/demo/identity-device" > "$root/desktop.log" 2>&1 & echo $! > "$root/samply.pid" )
sleep $((12 + secs))
pkill -x rill-compositor || true; sleep 3; pkill -x rill-vector || true
[[ -f $root/demo/server.pid ]] && kill "$(cat "$root/demo/server.pid")" 2>/dev/null || true
grep -E 'frame_ms|frames=' "$root/desktop.log" | tail -2
ls -la "$out/$label.json.gz"
