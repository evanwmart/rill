#!/usr/bin/env bash
# soak-sample.sh — one CSV line every 5 minutes, for the pi-soak protocol
# (docs/pi-soak.md). Run detached on the Pi:
#
#   setsid ./soak-sample.sh >/dev/null 2>&1 &
#
# Promoted from the protocol doc's inline snippet before the first run, with
# two additions the protocol's test list names but the snippet didn't record:
#
#   * PIDs ride along with each process's PSS (`comm:pid:kib`). Without
#     them, a crash-and-restart shows as an innocent dip in one column; with
#     them, a changed pid IS the crash record, timestamped to five minutes.
#   * An fd count per process. "fd leaks" is on the protocol's tests list,
#     and a leak that would kill day 6 is visible as a slope by hour 12.
#   * History growth (history_kib). The always-on recorder postdates the
#     protocol; its segments are SUPPOSED to grow (~6 MiB/hour busy was the
#     measured figure), unlike the cache column where any growth is a
#     finding. Separate columns so the two claims stay separable.
#
# Everything else matches the doc: %cpu is deliberately absent (a lifetime
# average — drift is what load1 and the exit-time frame report are for),
# whole-box numbers are weather, per-process columns are the signal.
out=~/rill-soak-$(date +%Y%m%d).csv
if [ ! -s "$out" ]; then
  echo "ts,procs_comm_pid_psskib_fds,mem_avail_kib,swap_used_kib,temp_c,throttled,cache_kib,history_kib,load1" >> "$out"
fi
while true; do
  procs=$(for p in $(pgrep -f 'rill-compositor|rill-vector|files-app|rill-server'); do
            comm=$(cat /proc/$p/comm 2>/dev/null) || continue
            pss=$(awk '/^Pss:/{print $2}' /proc/$p/smaps_rollup 2>/dev/null)
            fds=$(ls /proc/$p/fd 2>/dev/null | wc -l)
            printf '%s:%s:%s:%s ' "$comm" "$p" "$pss" "$fds"
          done)
  mem=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
  swp=$(awk '/SwapTotal/{t=$2}/SwapFree/{f=$2}END{print t-f}' /proc/meminfo)
  tmp=$(vcgencmd measure_temp 2>/dev/null | tr -d "temp='C")
  thr=$(vcgencmd get_throttled 2>/dev/null | cut -d= -f2)
  cch=$(du -sk ~/.local/share/rill-demo/content 2>/dev/null | cut -f1)
  hst=$(du -sk ~/.local/share/rill/history 2>/dev/null | cut -f1)
  l1=$(cut -d' ' -f1 /proc/loadavg)
  echo "$(date -Is),\"$procs\",$mem,$swp,$tmp,$thr,$cch,${hst:-0},$l1" >> "$out"
  sleep 300
done
