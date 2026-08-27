#!/usr/bin/env bash
# Sample named processes' CPU and memory once a second, the same way
# bench-device.sh does, and write a CSV that can be joined to its samples.csv
# on `timestamp_s`.
#
#   scripts/sample-host.sh 300 labwc wayfire Xorg rill-compositor > host.csv
#
# It exists for one question the bundle cannot answer about itself: on a
# nested run, `rill-compositor` is a Wayland *client* of the host compositor,
# so the cost of presenting each frame into that host is inside our number,
# and the host's own cost is inside nobody's — it is not a Rill process, so
# bench-device.sh never sees it. Sampling it alongside is the difference
# between "our rasterizer is expensive" and "being a guest is expensive".
#
# CPU is percent of ONE core over the preceding second (bench-device.sh
# reports a cumulative mean since its phase began; averaging these per-second
# figures across a phase gives the same thing). Clock base is /proc/uptime, as
# there, so timestamps line up.
set -uo pipefail

seconds=${1:?usage: sample-host.sh SECONDS NAME...}
shift
names=("$@")
[[ ${#names[@]} -eq 0 ]] && names=(labwc wayfire Xorg)

tick=$(getconf CLK_TCK)
mono() { awk '{printf "%.3f", $1}' /proc/uptime; }
# utime+stime for a pid, in ticks. Fields 14 and 15 after the (comm) field,
# which is parenthesised and may itself contain spaces — so split on ')'.
cpu_ticks() {
    awk '{ n=split($0, a, ") "); split(a[n], f, " "); print f[12] + f[13] }' \
        "/proc/$1/stat" 2>/dev/null || echo 0
}
mem_kib() {   # -> "pss rss"
    awk '/^Pss:/{p+=$2} /^Rss:/{r+=$2} END{printf "%d %d", p, r}' \
        "/proc/$1/smaps_rollup" 2>/dev/null || echo "0 0"
}

echo "timestamp_s,name,pid,cpu_percent_one_core,pss_kib,rss_kib"

declare -A prev_t prev_at
for (( s = 0; s < seconds; s++ )); do
    now=$(mono)
    for name in "${names[@]}"; do
        for pid in $(pgrep -x "$name" 2>/dev/null); do
            t=$(cpu_ticks "$pid")
            key="$name:$pid"
            if [[ -n ${prev_t[$key]:-} ]]; then
                dt=$(awk -v a="${prev_at[$key]}" -v b="$now" 'BEGIN{printf "%.3f", b-a}')
                cpu=$(awk -v d="$(( t - ${prev_t[$key]} ))" -v k="$tick" -v e="$dt" \
                    'BEGIN{ if (e <= 0) print "0.00"; else printf "%.2f", (d/k)/e*100 }')
                read -r pss rss <<<"$(mem_kib "$pid")"
                printf '%s,%s,%s,%s,%s,%s\n' "$now" "$name" "$pid" "$cpu" "$pss" "$rss"
            fi
            prev_t[$key]=$t
            prev_at[$key]=$now
        done
    done
    sleep 1
done
