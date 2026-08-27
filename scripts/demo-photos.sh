#!/usr/bin/env bash
# A roll of photographs, for watching what a window keeps in memory while you
# scroll it.
#
#   scripts/demo-photos.sh [COUNT]           # build it, print the launch line
#   scripts/demo-photos.sh [COUNT] --launch  # build it and put it on screen
#   scripts/demo-photos.sh [COUNT] --before  # the same, on the older client
#   scripts/demo-photos.sh [COUNT] --log     # launch and time every slow frame
#
# COUNT defaults to 60 photographs.
#
# The residency rule is invisible in a screenshot and obvious in a meter: a
# window showing two photographs out of sixty should cost two photographs, not
# sixty. This builds a page long enough to tell the difference, serves it from
# the demo server, and puts it on screen next to the resource meter.
#
# `--log` is for "it feels janky": it prints every frame over 8 ms split by
# phase — a stall in `composite` is the compositor's drawing, one in `acquire`
# is waiting on the display, one in `present` is the driver — and reports any
# window the compositor killed. Idle on this machine reads 0.3 ms of composite
# against 15.5 ms of acquire, which is vsync and not a problem.
#
# `--before` runs the identical page against a client built before the change,
# which is the comparison worth watching: scroll to the bottom and the meter
# climbs the whole way. It needs target/demo-before/rill-vector, which is a
# `git worktree add` at the older commit plus `cargo build -p rill-vector`.
#
# Needs the demo tree — run scripts/demo-desktop.sh first (it builds the
# binaries, creates the identities and starts the server). Photographs land in
# the demo's content tree under /public, which the default policy serves to
# anyone, so nothing here touches the grant model.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
root=${RILL_DEMO_ROOT:-$HOME/.local/share/rill-demo}
port=${RILL_DEMO_PORT:-7420}
profile=${RILL_BENCH_PROFILE:-debug}

count=60
launch=false
before=false
log=false
for arg in "$@"; do
    case $arg in
        --launch) launch=true ;;
        --before) launch=true; before=true ;;
        --log) launch=true; log=true ;;
        [0-9]*) count=$arg ;;
        *) echo "unknown argument: $arg" >&2; exit 1 ;;
    esac
done

content=$root/content
device_id=$root/identity-device
bin=$repo/target/$profile
rill=$bin/rill

[[ -d $content ]] || {
    echo "no demo content tree at $content — run scripts/demo-desktop.sh first" >&2
    exit 1
}
[[ -x $rill ]] || {
    echo "no $profile rill binary at $rill — run scripts/demo-desktop.sh first" >&2
    exit 1
}

photos=$content/public/photos
mkdir -p "$photos"

# Photographs, not test patterns: 1600x1200 is about what a phone gives you
# once, and JPEG of a smooth-ish image is what a real one weighs. Regenerated
# only when the count changes, since sixty of these take a moment.
have=$(find "$photos" -name 'p*.jpg' | wc -l)
if [[ $have -ne $count ]]; then
    echo "==> generating $count photographs (1600x1200) in $photos"
    rm -f "$photos"/p*.jpg
    python3 - "$photos" "$count" <<'PY'
import sys
import numpy as np
from PIL import Image, ImageDraw

out, count = sys.argv[1], int(sys.argv[2])
w, h = 1600, 1200
ys = np.linspace(0, 1, h)[:, None, None]
xs = np.arange(w)[None, :, None]
for i in range(count):
    rng = np.random.default_rng(i)
    # A two-tone wash with a bright disc and some grain. Structure rather than
    # a flat fill, so the JPEG weighs about what a photograph weighs, and
    # different enough frame to frame to see where you are while scrolling.
    top = rng.integers(40, 210, 3)[None, None, :]
    bottom = rng.integers(20, 120, 3)[None, None, :]
    img = top * (1 - ys) + bottom * ys
    img = img + 18 * np.sin(xs / 40.0 + i) + rng.integers(-10, 10, (h, w, 1))
    frame = Image.fromarray(np.clip(img, 0, 255).astype(np.uint8), "RGB")
    d = ImageDraw.Draw(frame)
    d.ellipse((w * 0.6, h * 0.1, w * 0.6 + 220, h * 0.1 + 220), fill=(250, 244, 220))
    d.text((40, 40), f"{i:03d}", fill=(255, 255, 255))
    frame.save(f"{out}/p{i:03d}.jpg", quality=82)
PY
fi
bytes=$(du -sh "$photos" | cut -f1)
echo "    $count photographs, $bytes on disk"

# The page: one column, every photograph in it, a caption under each so you
# can see where you are while scrolling.
page=$(mktemp)
{
    echo 'style "cap" size=12 color="#8a8a99"'
    echo 'style "title" size=24 weight="bold"'
    echo 'column gap=20 padding=24 {'
    echo "	text \"A roll of $count photographs\" style=\"title\""
    echo "	text \"Scroll it. The meter should not care how far you get.\" style=\"cap\""
    for i in $(seq 0 $((count - 1))); do
        printf '\timage "/public/photos/p%03d.jpg"\n' "$i"
        printf '\ttext "photo %03d of %d" style="cap"\n' "$i" "$count"
    done
    echo '}'
} > "$page"
"$rill" doc compile "$page" --output "$content/public/roll" >/dev/null
rm -f "$page"
echo "==> page compiled to $content/public/roll"

# The same photographs as a thumbnail grid — the page the style-sized image
# box exists for. Each picture is a 240x180 slot, so what the client keeps and
# sends for a visible thumbnail is ~470 KB instead of the ~7 MB a full-width
# photo costs.
grid=$(mktemp)
{
    echo 'style "title" size=24 weight="bold"'
    echo 'style "cap" size=12 color="#8a8a99"'
    echo 'style "thumb" width=240 height=180 background="#1a1a22" corner=6'
    echo 'style "grid" wrap=#true'
    echo 'column gap=16 padding=24 {'
    echo "	text \"The same $count photographs, as a grid\" style=\"title\""
    echo "	text \"Every slot is 240x180 whatever the photo is.\" style=\"cap\""
    echo '	row style="grid" gap=12 {'
    for i in $(seq 0 $((count - 1))); do
        printf '\t\timage "/public/photos/p%03d.jpg" style="thumb"\n' "$i"
    done
    echo '	}'
    echo '}'
} > "$grid"
"$rill" doc compile "$grid" --output "$content/public/grid" >/dev/null
rm -f "$grid"
echo "==> grid compiled to $content/public/grid"

# Prove the server actually serves both halves before claiming it works: a
# page that 404s and a photograph that 404s look identical on screen (a
# placeholder box), and only one of them is a broken demo. The trust store
# lives in the identity directory, so these need it as much as the client does.
if ! "$rill" head "rill://127.0.0.1:$port/public/roll" --identity "$device_id" >/dev/null 2>&1; then
    echo "server is not serving the page — is it running? (scripts/demo-desktop.sh)" >&2
    exit 1
fi
"$rill" head "rill://127.0.0.1:$port/public/photos/p000.jpg" --identity "$device_id" >/dev/null 2>&1 || {
    echo "server is not serving the photographs" >&2
    exit 1
}
echo "==> served at rill://127.0.0.1:$port/public/roll"

rill_ld_path=$([[ -d /usr/lib64 ]] && echo /usr/lib64 || echo "")
data=$root/data
compositor=$bin/rill-compositor
vector=$bin/rill-vector

# Which client shows the roll. Only this one changes between the two runs —
# same compositor, same server, same page — so anything that differs is the
# client's residency and not the environment.
roll_client=$vector
if $before; then
    roll_client=$repo/target/demo-before/rill-vector
    [[ -x $roll_client ]] || {
        echo "no before-client at $roll_client" >&2
        echo "build one: git worktree add /tmp/rill-before <commit> &&" >&2
        echo "  (cd /tmp/rill-before && cargo build -p rill-vector) &&" >&2
        echo "  mkdir -p $repo/target/demo-before &&" >&2
        echo "  cp /tmp/rill-before/target/debug/rill-vector $roll_client" >&2
        exit 1
    }
fi

cmd=(env LD_LIBRARY_PATH="$rill_ld_path" "$compositor"
     "$vector" --dock --data "$data" --identity "$device_id" +
     "$roll_client" --widget "rill://127.0.0.1:$port/public/roll"
       --widget-place center:1100x820+0+0 --data "$data" --identity "$device_id" +
     "$vector" --widget "rill://127.0.0.1:$port/meter"
       --widget-place top-right:360x260+16+16 --data "$data" --identity "$device_id")

# `--log` is for "it feels janky": every frame over 8 ms, split by phase, plus
# whatever the compositor says about a window that goes away. A stutter you can
# see but cannot time is a guessing game — a list of outliers with timestamps
# has a period in it, and a period names the cause.
if $log; then
    cmd=(env "RILL_FRAME_LOG=8" "${cmd[@]:1}")
fi

echo
if $launch; then
    if $log; then
        out=$root/frames.log
        echo "==> logging slow frames to $out"
        echo "    scroll and drag until it feels bad, then quit and read it"
        echo
        "${cmd[@]}" 2>&1 | tee "$out"
        echo
        echo "slowest frames:"
        grep "slow frame" "$out" | sort -t' ' -k4 -rn | head -10
        grep -E "protocol error|disconnected" "$out" | head -5
        exit 0
    fi
    $before && echo "==> launching with the PRE-CHANGE client (target/demo-before)"
    $before || echo "==> launching"
    echo
    echo "   Scroll the roll to the bottom and back. The meter counts the whole"
    echo "   Rill process family — client, compositor and server — and in a"
    echo "   $profile build the compositor is most of it, so watch which way the"
    echo "   number moves rather than what it starts at."
    echo
    exec "${cmd[@]}"
fi

echo "==> to watch it"
echo
printf '   '
printf '%q ' "${cmd[@]}"
printf '\n\n   or: scripts/demo-photos.sh %s --launch\n' "$count"
if [[ -x $repo/target/demo-before/rill-vector ]]; then
    printf '   and the same page on the client from before the change:\n'
    printf '       scripts/demo-photos.sh %s --before\n' "$count"
fi
echo
echo "   The meter counts the whole process family. In a $profile build the"
echo "   compositor dominates it, so the number to watch is which way it moves."
if [[ $profile == debug ]]; then
    echo "   RILL_BENCH_PROFILE=release makes the resize path about 15x faster;"
    echo "   scripts/demo-desktop.sh has to be re-run under it first."
fi
echo
