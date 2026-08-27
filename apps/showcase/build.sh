#!/usr/bin/env bash
# Build the showcase apps into a servable content tree.
#
#   apps/showcase/build.sh [OUTDIR]           # default: ./showcase-out
#   apps/showcase/build.sh --repin [OUTDIR]   # accept new hashes
#
# Produces OUTDIR/apps/<id>/{app.rillpack,manifest} — the layout rill-server
# serves and `rill app install` expects.
#
# Each app is a manifest plus KDL pages under src/. A page src/NAME.kdl is
# compiled to /app/NAME inside the pack, and the KDL itself ships alongside it
# at /NAME.kdl, so every published app carries its own source.
#
# Packs are reproducible: same sources, same bytes, same hash. Where a manifest
# pins `pack_hash`, the build *verifies* it and fails on a mismatch — so a
# silent drift in the document codec or the pack format is caught here rather
# than at install time.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
repin=false
args=()
for arg in "$@"; do
    case $arg in
        --repin) repin=true ;;
        *) args+=("$arg") ;;
    esac
done
out=${args[0]:-$repo/showcase-out}
rill=${RILL:-$repo/target/debug/rill}

if [[ ! -x $rill ]]; then
    echo "no rill binary at $rill — cargo build -p rill (or set RILL)" >&2
    exit 1
fi

mkdir -p "$out/apps"
status=0

for dir in "$here"/*/; do
    [[ -f $dir/manifest ]] || continue
    # Declarative apps only: notes-app is a Rust server app and builds itself.
    compgen -G "$dir/src/*.kdl" >/dev/null || continue
    id=$(sed -n 's/^app_id *= *"\(.*\)"/\1/p' "$dir/manifest")
    id=${id:-$(basename "$dir")}
    stage=$(mktemp -d)
    trap 'rm -rf "$stage"' EXIT

    mkdir -p "$stage/app"
    for page in "$dir"src/*.kdl; do
        name=$(basename "$page" .kdl)
        "$rill" doc compile "$page" --output "$stage/app/$name" >/dev/null
        cp "$page" "$stage/$name.kdl"
    done

    mkdir -p "$out/apps/$id"
    pack="$out/apps/$id/app.rillpack"
    "$rill" pack build "$stage" --output "$pack" >/dev/null
    hash=$("$rill" pack hash "$pack")

    pinned=$(sed -n 's/^pack_hash *= *"\(.*\)"/\1/p' "$dir/manifest")
    if [[ -n $pinned && $pinned != "$hash" ]]; then
        if $repin; then
            sed -i "s|^pack_hash *= *\".*\"|pack_hash = \"$hash\"|" "$dir/manifest"
            echo "$id: re-pinned $pinned -> $hash"
        else
            echo "$id: PACK HASH DRIFT" >&2
            echo "  manifest pins $pinned" >&2
            echo "  build produced $hash" >&2
            echo "  sources unchanged? then the document format moved:" >&2
            echo "    apps/showcase/build.sh --repin" >&2
            status=1
        fi
    fi

    # Ship the manifest with the hash the build actually produced. pack_hash
    # is a top-level key, so when the source manifest omits it, insert it
    # before the first [section] — appending would land it inside the last
    # table ([permissions]) and the manifest would parse without it.
    awk -v h="$hash" '
        /^pack_hash *=/ { print "pack_hash = \"" h "\""; done = 1; next }
        /^\[/ && !done  { print "pack_hash = \"" h "\""; done = 1 }
                        { print }
        END             { if (!done) print "pack_hash = \"" h "\"" }
    ' "$dir/manifest" > "$out/apps/$id/manifest"

    echo "$id -> $pack ($hash)"
    rm -rf "$stage"
    trap - EXIT
done

exit $status
