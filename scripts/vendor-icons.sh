#!/usr/bin/env bash
# Vendor Phosphor icons (MIT) as plain SVGs, pinned to a release tag.
#
#   scripts/vendor-icons.sh            # fetch the names listed below
#
# Rill's icon names stay stable; the table below maps them to Phosphor
# file names. Add a pair, re-run, commit the SVG.
set -euo pipefail
tag=v2.0.2
base="https://raw.githubusercontent.com/phosphor-icons/core/$tag/assets/regular"
dest="$(dirname "$0")/../crates/rill-ui/phosphor"
mkdir -p "$dest"

# rill-name  phosphor-name
pairs=(
    "folder folder"
    "file file"
    "home house"
    "world globe"
    "lock lock"
    "trash trash"
    "plus plus"
    "minus minus"
    "pencil pencil"
    "search magnifying-glass"
    "dots-vertical dots-three-vertical"
    "chevron-up caret-up"
    "chevron-down caret-down"
    "chevron-left caret-left"
    "chevron-right caret-right"
    "close x"
    "list list"
    "grid squares-four"
    "refresh arrow-clockwise"
    "music-note music-note"
    "play play"
    "pause pause"
    "skip-back skip-back"
    "skip-forward skip-forward"
    "speaker speaker-high"
    "speaker-mute speaker-simple-slash"
    # Signage (the gate display): travel and weather glyphs.
    "plane airplane-tilt"
    "ticket ticket"
    "suitcase suitcase-rolling"
    "people users-three"
    "gate door-open"
    "warning warning"
    "info info"
    "sun sun"
    "cloud cloud"
    "cloud-sun cloud-sun"
    "cloud-rain cloud-rain"
    "pin map-pin"
    "clock clock"
    "plane-flight airplane-in-flight"
    "takeoff airplane-takeoff"
    "landing airplane-landing"
)
for pair in "${pairs[@]}"; do
    read -r ours theirs <<<"$pair"
    curl -sfL --max-time 30 "$base/$theirs.svg" -o "$dest/$ours.svg"
    echo "  $ours <- $theirs"
done

# Fill-weight variants — solid glyphs, same renderer. Named "<ours>-fill".
fillbase="https://raw.githubusercontent.com/phosphor-icons/core/$tag/assets/fill"
fills=(
    "folder-fill folder-fill"
    "file-fill file-fill"
    "home-fill house-fill"
    "world-fill globe-fill"
    "lock-fill lock-fill"
    "trash-fill trash-fill"
    "plane-fill airplane-tilt-fill"
    "ticket-fill ticket-fill"
    "suitcase-fill suitcase-rolling-fill"
    "people-fill users-three-fill"
    "gate-fill door-open-fill"
    "warning-fill warning-fill"
    "info-fill info-fill"
    "sun-fill sun-fill"
    "cloud-fill cloud-fill"
    "cloud-sun-fill cloud-sun-fill"
    "cloud-rain-fill cloud-rain-fill"
    "pin-fill map-pin-fill"
    "plane-flight-fill airplane-in-flight-fill"
    "takeoff-fill airplane-takeoff-fill"
    "landing-fill airplane-landing-fill"
)
for pair in "${fills[@]}"; do
    read -r ours theirs <<<"$pair"
    curl -sfL --max-time 30 "$fillbase/$theirs.svg" -o "$dest/$ours.svg"
    echo "  $ours <- fill/$theirs"
done
curl -sfL --max-time 30 "https://raw.githubusercontent.com/phosphor-icons/core/$tag/LICENSE" -o "$dest/LICENSE"
echo "pinned $tag -> $dest"
