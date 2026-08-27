#!/usr/bin/env bash
# Stand up a full Rill desktop from a clean slate: build the showcase apps,
# create server + device identities, serve the apps over rill://, install
# them, and print the command that launches the desktop.
#
#   scripts/demo-desktop.sh                 # set up, then print the launch line
#   scripts/demo-desktop.sh --launch        # set up and launch it
#   scripts/demo-desktop.sh --clean         # start over from nothing
#
# Everything lands under ~/.local/share/rill-demo, on /home — deliberately
# not /tmp, which is a tmpfs here and takes the tree with it when cleaned.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
root=${RILL_DEMO_ROOT:-$HOME/.local/share/rill-demo}
port=${RILL_DEMO_PORT:-7420}
launch=false

for arg in "$@"; do
    case $arg in
        --launch) launch=true ;;
        --clean) rm -rf "$root"; echo "removed $root" ;;
        *) echo "unknown argument: $arg" >&2; exit 1 ;;
    esac
done

content=$root/content
data=$root/data
server_id=$root/identity-server
device_id=$root/identity-device
log=$root/server.log

# Which build to launch. `RILL_BENCH_PROFILE=release` has to select the
# binaries and not merely label them: bench-stack.sh records the profile in
# every run file, and a run that says "release" while launching debug
# binaries writes a number into docs/memory-footprint.md that nobody can
# ever reproduce or correct.
profile=${RILL_BENCH_PROFILE:-debug}
case $profile in
    debug)   cargo_profile_flag=() ;;
    release) cargo_profile_flag=(--release) ;;
    *) echo "unknown RILL_BENCH_PROFILE '$profile' (want debug|release)" >&2; exit 1 ;;
esac
bin=$repo/target/$profile

rill=$bin/rill
# files-app is rill-server plus a /files handler, so it serves the apps *and*
# the explorer from one process — an example app composing with the server
# rather than replacing it.
server=$bin/files-app
compositor=$bin/rill-compositor
vector=$bin/rill-vector

# Build rather than merely check. Packs and clients must move together: the
# document format is versioned, so a stale binary now refuses the pack outright
# instead of misparsing it — but refusing is still a broken desktop, and the
# only reason they ever diverged was a forgotten rebuild.
# This box has no unversioned dev symlinks for xkbcommon-x11 and friends, so
# linking needs a shim directory of libFOO.so -> /usr/lib64/libFOO.so.N. Added
# rather than replaced, so an existing RUSTFLAGS still applies.
libshim=${RILL_LIBSHIM:-$HOME/.cache/rill-libshim}
if [[ -d $libshim ]]; then
    export RUSTFLAGS="${RUSTFLAGS:-} -L $libshim"
fi
# The shim may also carry a stub alsa.pc (alsa-devel is not installed on
# this box; alsa-sys ships pregenerated bindings and only needs pkg-config
# to emit link flags — rodio in music-app pulls it in). Only exported where
# the stub exists; a box with real alsa-devel never grows one and is
# untouched. Installing alsa-devel retires the stub.
if [[ -f $libshim/pkgconfig/alsa.pc ]]; then
    export PKG_CONFIG_PATH="$libshim/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi
# This box needs /usr/lib64 on the loader path (see the libshim note above);
# Debian-family systems, including Raspberry Pi OS, do not have that
# directory and do not need it. Set it only when it exists, so the same
# script runs unchanged on both.
rill_ld_path=$([[ -d /usr/lib64 ]] && echo /usr/lib64 || echo "")

# `RILL_SKIP_BUILD=1` uses whatever is already in target/$profile instead of
# building. For cross-compiled binaries: a Raspberry Pi that received its
# binaries over rsync has no cargo fingerprints for them, so an unconditional
# `cargo build` would rebuild the whole graph on the slowest machine in the
# room — which is the entire thing cross-compiling was avoiding. The binaries
# are still checked for existence below, so this skips the build, never the
# verification.
if [[ ${RILL_SKIP_BUILD:-0} == 1 ]]; then
    echo "==> using prebuilt binaries in $bin (RILL_SKIP_BUILD=1)"
else
    echo "==> building binaries ($profile)"
    ( cd "$repo" && cargo build --locked "${cargo_profile_flag[@]}" \
        -p rill -p files-app -p rill-compositor -p rill-vector )
fi
for bin in "$rill" "$server" "$compositor" "$vector"; do
    [[ -x $bin ]] || {
        echo "no $profile binary at $bin" >&2
        [[ ${RILL_SKIP_BUILD:-0} == 1 ]] &&
            echo "(RILL_SKIP_BUILD=1 is set — copy the binaries there first)" >&2
        exit 1
    }
done

mkdir -p "$root" "$data"

# Rices and the shaders they name. A rice is a whole theme.toml filed under a
# name; the studio saves and loads them, and Ctrl+Shift+R cycles. They refer
# to shaders by a path relative to the config dir, so both have to be
# installed together or a preset loads to a desktop with no wallpaper.
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
mkdir -p "$config_home/rill/rices" "$config_home/rill/shaders"
cp -n "$repo"/assets/rices/*.toml   "$config_home/rill/rices/"   2>/dev/null || true
cp    "$repo"/assets/shaders/*.wgsl "$config_home/rill/shaders/" 2>/dev/null || true
echo "==> rices in $config_home/rill/rices (Ctrl+Shift+R cycles)"

# The out-of-box look (decided 2026-08-21): a machine with no active theme
# gets rill-green. `cp -n`, so a machine that has chosen its own keeps it —
# this seeds a default, it never overrides a decision. The code-level
# fallback (builtin_dark, when no theme file exists at all) stays what it is:
# a theme file should never be *required* for a desktop to boot.
cp -n "$repo"/assets/rices/rill-green.toml "$config_home/rill/theme.toml" 2>/dev/null || true

# 1. Apps. build.sh verifies each pinned pack_hash, so a codec drift fails
#    here rather than at install time.
# Build the packs with *this* profile's `rill`. build.sh defaults to
# target/debug/rill, which is only ever right by accident: a machine that
# built release-only — a Pi following the bring-up doc, say — has no debug
# binary at all, and the setup failed at its first step with a message about
# a build it had already done.
echo "==> building showcase apps"
RILL=$rill "$repo/apps/showcase/build.sh" "$content"

# 2. Identities. The server has one; this device has another and enrolls with
#    it, which is what makes the install authenticated rather than anonymous.
if [[ ! -d $server_id ]]; then
    echo "==> creating server identity"
    "$rill" auth init-server "$server_id" --name "rill-demo" >/dev/null
fi
if [[ ! -d $device_id ]]; then
    echo "==> creating device identity"
    "$rill" auth init --identity "$device_id" >/dev/null
    fingerprint=$("$rill" auth fingerprint --identity "$device_id")
    "$rill" auth enroll "$server_id" "demo-device" "$fingerprint" >/dev/null
    echo "    enrolled device $fingerprint"
fi

# The default policy only publishes /public/**, and everything else is denied
# (and *hidden* — an unauthorized path answers NOT_FOUND, so it never admits
# that it exists). Grant the apps to this enrolled device by name rather than
# to anonymous: these are personal apps, and that is the model worth
# demonstrating.
if ! grep -q '"/apps/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /apps/** and /files/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/apps/**"
allow = ["demo-device"]

[[rule]]
path = "/files/**"
allow = ["demo-device"]

[[rule]]
path = "/work/**"
allow = ["demo-device"]
POLICY
fi

# The resource meter rides along too — a widget's document, read-only.
if ! grep -q '"/meter/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /meter/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/meter/**"
allow = ["demo-device"]
POLICY
fi

# The history app: the machine's memory, granted to the device that owns it.
# Deny-by-default is the gate until brokered reads land — anonymous and
# other devices get NOT_FOUND, which does not even admit the corpus exists.
if ! grep -q '"/history/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /history/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/history/**"
allow = ["demo-device"]
POLICY
fi

# The editor: same trust boundary as the terminal — the device that owns
# this desktop may edit this machine's files; nobody else may even ask.
if ! grep -q '"/edit/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /edit/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/edit/**"
allow = ["demo-device"]
POLICY
fi

# The app menu: readable by anyone enrolled — it lists what is published,
# which the manifest files already tell any device that can fetch them.
if ! grep -q '"/launcher/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /launcher/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/launcher/**"
allow = ["demo-device"]
POLICY
fi

# The ASCII art widget, read-only like the meter.
if ! grep -q '"/ascii/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /ascii/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/ascii/**"
allow = ["demo-device"]
POLICY
fi

# The music player: browsing and transport live under one prefix.
if ! grep -q '"/music/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /music/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/music/**"
allow = ["demo-device"]
POLICY
fi

# The terminal rides the same server; grant its prefix like /studio. Note
# what this grant is: the shell runs as whoever started the server, so
# /term/** is the whole machine. It belongs to the device that owns it.
if ! grep -q '"/term/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /term/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/term/**"
allow = ["demo-device"]
POLICY
fi

# The theme studio rides the same server; grant its prefix like /files.
if ! grep -q '"/studio/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting /studio/** to demo-device"
    cat >> "$server_id/policy.toml" <<'POLICY'

[[rule]]
path = "/studio/**"
allow = ["demo-device"]
POLICY
fi

# The standard places, so the explorer's sidebar shows the Linux-like row of
# home folders with their icons. Granted to the device like /work.
if ! grep -q '"/Documents/\*\*"' "$server_id/policy.toml"; then
    echo "==> granting the standard folders to demo-device"
    for place in Downloads Documents Pictures Videos Music; do
        cat >> "$server_id/policy.toml" <<POLICY

[[rule]]
path = "/$place/**"
allow = ["demo-device"]
POLICY
    done
fi
mkdir -p "$content"/{Downloads,Documents,Pictures,Videos,Music}
[[ -f $content/Documents/welcome.txt ]] \
    || echo "A place for documents." > "$content/Documents/welcome.txt"
[[ -f $content/Downloads/sample.tar.gz ]] \
    || echo "not really an archive" > "$content/Downloads/sample.tar.gz"
[[ -f $content/Pictures/wallpaper-notes.txt ]] \
    || echo "Wallpapers live in the theme dir; pictures live here." > "$content/Pictures/wallpaper-notes.txt"
# An empty granted directory is invisible (nothing in it to serve — the
# policy is the UI), so every place gets one file to exist for.
[[ -f $content/Videos/placeholder.txt ]] \
    || echo "Video lives with mpv until a media app exists." > "$content/Videos/placeholder.txt"
[[ -f $content/Music/placeholder.txt ]] \
    || echo "A place for music." > "$content/Music/placeholder.txt"

# Two branches with different visibility, so the file explorer has something
# to demonstrate: /public is anonymous, /private is granted to nobody.
# Somewhere the file explorer may actually write. Deliberately one subtree:
# the policy has no concept of write permission, so confinement is the app's
# job until it does.
mkdir -p "$content/work"
mkdir -p "$content/public" "$content/private"
[[ -f $content/public/notice.txt ]] \
    || echo "Anyone enrolled or not may read this." > "$content/public/notice.txt"
[[ -f $content/private/secret.txt ]] \
    || echo "No device holds a grant for this path." > "$content/private/secret.txt"

# 3. Serve. Bind explicitly to 127.0.0.1: `localhost` resolves IPv6-first on
#    this box and the connection fails.
#
#    Always restart rather than reusing a running server — it holds the policy
#    in memory from startup, so a stale one silently ignores the rule above.
#    Track it by pidfile; `pkill -f` would match this script's own command line.
#    Reclaim the port from whatever holds it, not just from our own pidfile:
#    a server left over from an earlier run (or from before the pidfile
#    existed) would keep the port, the new one would fail to bind, and every
#    request would be answered by the *stale* policy — which looks exactly
#    like a missing file, because denials are hidden.
for pid in $(ss -ltnp 2>/dev/null | grep -o "pid=[0-9]*" | cut -d= -f2 | sort -u); do
    [[ $(ss -ltnp 2>/dev/null | grep "pid=$pid," | grep -c ":$port ") -gt 0 ]] || continue
    [[ $(cat "/proc/$pid/comm" 2>/dev/null) =~ ^(rill-server|files-app)$ ]] || continue
    echo "==> stopping stale server (pid $pid) on port $port"
    kill "$pid" 2>/dev/null || true
    sleep 1
done
echo "==> starting files-app (server + /files) on 127.0.0.1:$port"
# The dev trail: one merged JSONL stream of every process's events, for
# debugging sessions where the causal order is the diagnosis. Scaffolding,
# not product — delete the file whenever; it is recreated on demand.
export RILL_DEV_LOG="$root/dev-trail.jsonl"
nohup env RILL_DEV_LOG="$RILL_DEV_LOG" "$server" "$content" --identity "$server_id" --writable "$content/work" \
    --bind 127.0.0.1 --port "$port" >"$log" 2>&1 &
echo $! > "$root/server.pid"
for _ in $(seq 1 50); do
    ss -ltn 2>/dev/null | grep -q ":$port " && break
    sleep 0.2
done
ss -ltn 2>/dev/null | grep -q ":$port " || { echo "server did not come up; see $log" >&2; exit 1; }

# 4. Trust the server's key, then install each app by its manifest URL.
echo "==> trusting server + installing apps"
"$rill" auth trust "rill://127.0.0.1:$port" --identity "$device_id" --yes >/dev/null

for dir in "$content"/apps/*/; do
    id=$(basename "$dir")
    "$rill" app install "rill://127.0.0.1:$port/apps/$id/manifest" \
        --identity "$device_id" --data "$data" >/dev/null && echo "    installed $id"
done

"$rill" app list --data "$data" || true

# Just the dock: the system monitor ("Rill — System") used to ride along
# here, and every restart opened it whether or not anyone wanted it. It is
# still one launch away when it is wanted: rill-vector --dashboard.
cmd=(env LD_LIBRARY_PATH="$rill_ld_path" RILL_DEV_LOG="$RILL_DEV_LOG" "$compositor"
     "$vector" --dock --data "$data" --identity "$device_id")

echo
echo "==> desktop ready"
if $launch; then
    exec "${cmd[@]}"
else
    printf '   '
    printf '%q ' "${cmd[@]}"
    printf '\n\n   or: scripts/demo-desktop.sh --launch\n   (server log: %s)\n' "$log"
fi
