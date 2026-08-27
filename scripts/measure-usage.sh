#!/usr/bin/env bash
# Measure the running Rill stack's memory, with attribution.
#
#   scripts/measure-usage.sh              # human table
#   scripts/measure-usage.sh --markdown   # rows for docs/memory-footprint.md
#
# Naive RSS is misleading for GPU processes: most of it is driver userspace,
# not Rill. This walks /proc/<pid>/smaps and buckets every mapping, so the
# output separates the driver's bill from Rill's actual working data — the
# same attribution docs/memory-footprint.md is built on. Quote PSS (shared
# pages counted proportionally), and append new runs to that doc rather
# than rewriting history.
set -euo pipefail

markdown=false
[[ ${1:-} == --markdown ]] && markdown=true

# The stack's process names, matched exactly (a pattern would catch this
# script's own shell — that lesson is already paid for).
procs=(rill-compositor rill-vector rill-server files-app)

# Bucket one smaps file: NAME KB pairs on stdout.
attribute() {
    awk '
        /^[0-9a-f]+-[0-9a-f]+ / {
            path = $6
            if (path ~ /nvidia|libcuda|libgl|GLX|glcore|glvkspirv|gpucomp|rtcore/) b = "driver"
            else if (path ~ /LLVM|llvm/)                                          b = "llvm"
            else if (path ~ /vulkan_radeon|lavapipe|radv|libvulkan/)              b = "mesa"
            else if (path ~ /target\/(debug|release)/)                            b = "binary"
            else if (path == "" || path ~ /^\[heap\]|^\[anon/)                    b = "anon"
            else                                                                  b = "other"
        }
        /^Pss:/ { pss[b] += $2; total += $2 }
        END {
            for (k in pss) printf "%s %d\n", k, pss[k]
            printf "total %d\n", total
        }
    ' "$1"
}

stack_total=0
rows=()
for name in "${procs[@]}"; do
    for pid in $(pgrep -x "$name" 2>/dev/null || true); do
        smaps=/proc/$pid/smaps
        [[ -r $smaps ]] || continue
        declare -A kb=()
        while read -r bucket val; do kb[$bucket]=$val; done < <(attribute "$smaps")
        rss_kb=$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status")
        total=${kb[total]:-0}
        # A process that is exiting reads as all zeros; a zero row is noise.
        [[ $total -eq 0 ]] && { unset kb; continue; }
        stack_total=$((stack_total + total))
        mb() { echo $(( ${1:-0} / 1024 )); }
        # "Rill's own" = heap/anon + our binary. Everything else is the
        # driver stack's bill, paid once per process.
        own=$(( ${kb[anon]:-0} + ${kb[binary]:-0} ))
        rows+=("$(printf '%-16s %6s %8s %8s %8s %7s %6s %6s' \
            "$name" "$pid" "$(mb "$rss_kb")" "$(mb "$total")" "$(mb "$own")" \
            "$(mb "${kb[driver]:-0}")" "$(mb "${kb[llvm]:-0}")" "$(mb "${kb[other]:-0}")")")
        unset kb
    done
done

if [[ ${#rows[@]} -eq 0 ]]; then
    echo "no rill processes running — start the desktop first" >&2
    exit 1
fi

if $markdown; then
    echo "| process | pid | RSS MB | PSS MB | rill MB | driver MB | llvm MB | other MB |"
    echo "|---|---|---|---|---|---|---|---|"
    for r in "${rows[@]}"; do
        # shellcheck disable=SC2086
        set -- $r; echo "| $1 | $2 | $3 | $4 | $5 | $6 | $7 | $8 |"
    done
    echo
    echo "Whole-stack PSS: **$((stack_total / 1024))MB** ($(date +%F), $(
        [[ -n $(find target/debug -maxdepth 1 -name rill-compositor -newer target 2>/dev/null) ]] \
            && echo debug || echo 'debug or release — note which') builds)"
else
    printf '%-16s %6s %8s %8s %8s %7s %6s %6s\n' \
        process pid RSS_MB PSS_MB rill_MB drv_MB llvm other
    printf '%s\n' "${rows[@]}"
    echo
    echo "whole-stack PSS: $((stack_total / 1024))MB"
    echo
    echo "rill_MB = anon heap + our binary — the number that is ours to defend."
    echo "PSS counts shared pages proportionally; quote it, not RSS."
    echo "Append notable runs to docs/memory-footprint.md (--markdown emits rows)."
fi
