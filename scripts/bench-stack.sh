#!/usr/bin/env bash
# Measure a running Rill desktop against a recorded machine, so two runs on
# two days (or two machines) can be compared honestly.
#
#   scripts/bench-stack.sh specs                 # the machine block alone
#   scripts/bench-stack.sh run <label> [secs]    # stand up a stack, sample it
#   scripts/bench-stack.sh sample <label> [secs] # sample whatever is running
#   scripts/bench-stack.sh compare <a> <b>       # two runs, side by side
#
# Why the spec block exists: every number below is a property of *this*
# machine as much as of Rill. A PSS figure without the GPU that produced it,
# or a frame time without the CPU governor, is not a measurement anyone can
# check — it is an anecdote. docs/memory-footprint.md keeps the log; this
# writes the rows and the machine they were taken on.
#
# `run` is deliberately hermetic: its own config dir, its own content cache,
# its own server on its own port, its own data dir. It never touches
# ~/.config/rill, ~/.cache/rill, or a desktop you have running.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bench=${RILL_BENCH_ROOT:-$repo/target/bench}
runs=$bench/runs
port=${RILL_BENCH_PORT:-7521}

# The processes that make up a desktop, matched exactly. A pattern would
# catch this script's own command line — that lesson is already paid for.
procs=(rill-compositor rill-vector files-app rill-server)

# ---------------------------------------------------------------- specs ---

# One `key<TAB>value` line per fact. Everything downstream parses this, so
# unknown values are the literal string "unknown" rather than an empty field.
emit_specs() {
    local k v
    say() { printf '%s\t%s\n' "$1" "${2:-unknown}"; }

    say host "$(uname -n)"
    say kernel "$(uname -r)"
    say arch "$(uname -m)"
    say distro "$(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME")"

    # CPU. Governor and boost matter more than model for frame-time work:
    # the same box on `powersave` and `performance` is two machines.
    say cpu_model "$(lscpu 2>/dev/null | sed -n 's/^Model name: *//p' | head -1)"
    say cpu_cores "$(nproc --all 2>/dev/null)"
    say cpu_threads "$(lscpu 2>/dev/null | sed -n 's/^CPU(s): *//p' | head -1)"
    say cpu_mhz_max "$(lscpu 2>/dev/null | sed -n 's/^CPU max MHz: *//p' | head -1)"
    v=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || true)
    say cpu_governor "${v:-unknown}"

    # Memory. Totals in MiB, and the swap state, since a run that touched
    # swap is not comparable with one that did not.
    say ram_total_mib "$(awk '/^MemTotal:/ {printf "%d", $2/1024}' /proc/meminfo)"
    say ram_avail_mib "$(awk '/^MemAvailable:/ {printf "%d", $2/1024}' /proc/meminfo)"
    say swap_total_mib "$(awk '/^SwapTotal:/ {printf "%d", $2/1024}' /proc/meminfo)"
    say swap_used_mib "$(awk '/^SwapTotal:/{t=$2}/^SwapFree:/{f=$2} END{printf "%d",(t-f)/1024}' /proc/meminfo)"

    # GPU. Every card the machine has, not just the one in use — a laptop
    # that switched to its iGPU between runs explains a lot of deltas.
    local gpus
    gpus=$(lspci -nn 2>/dev/null | grep -iE 'vga|3d controller|display' \
           | sed 's/^[0-9a-f:.]* //' | paste -sd'; ' -)
    say gpu_devices "${gpus:-unknown}"
    if command -v nvidia-smi >/dev/null 2>&1; then
        v=$(nvidia-smi --query-gpu=name,memory.total,memory.used,driver_version \
              --format=csv,noheader,nounits 2>/dev/null | head -1 || true)
        if [[ -n $v ]]; then
            say gpu_nvidia_name "$(cut -d, -f1 <<<"$v" | xargs)"
            say vram_total_mib "$(cut -d, -f2 <<<"$v" | xargs)"
            say vram_used_mib "$(cut -d, -f3 <<<"$v" | xargs)"
            say gpu_driver "$(cut -d, -f4 <<<"$v" | xargs)"
        fi
    fi
    # Mesa's view, which is what an AMD/Intel box would report instead.
    if command -v glxinfo >/dev/null 2>&1; then
        say gl_renderer "$(glxinfo -B 2>/dev/null | sed -n 's/^ *Device: *//p' | head -1)"
        say mesa_version "$(glxinfo -B 2>/dev/null | sed -n 's/^OpenGL version string: *//p' | head -1)"
    fi
    # The Vulkan ICDs actually installed: wgpu enumerates all of them, and
    # loading an unused one costs tens of MB (see docs/memory-footprint.md).
    v=$(ls /usr/share/vulkan/icd.d/*.json 2>/dev/null | xargs -r -n1 basename | paste -sd, -)
    say vulkan_icds "${v:-unknown}"

    # Storage backing the repo — cache churn is a disk cost, and an NVMe
    # and a spinning disk are not the same machine for that number.
    local src dev
    src=$(df --output=source "$repo" 2>/dev/null | tail -1)
    dev=$(lsblk -no PKNAME "$src" 2>/dev/null | head -1)
    say fs_type "$(df --output=fstype "$repo" 2>/dev/null | tail -1 | xargs)"
    say fs_device "${src:-unknown}"
    if [[ -n ${dev:-} ]]; then
        say disk_model "$(cat "/sys/block/$dev/device/model" 2>/dev/null | xargs)"
        say disk_rotational "$(cat "/sys/block/$dev/queue/rotational" 2>/dev/null)"
    fi

    # Session. The host compositor is Rill's landlord in nested mode: its
    # vsync paces our present, so its identity belongs with the numbers.
    say session_type "${XDG_SESSION_TYPE:-unknown}"
    say host_compositor "${XDG_CURRENT_DESKTOP:-${DESKTOP_SESSION:-unknown}}"
    say wayland_display "${WAYLAND_DISPLAY:-unset}"
    # Resolution and refresh: the swapchain scales with the first, the frame
    # budget with the second.
    if [[ -r /sys/class/drm ]]; then
        v=$(for m in /sys/class/drm/*/modes; do
                [[ -s $m ]] && head -1 "$m"
            done 2>/dev/null | paste -sd, -)
        say display_modes "${v:-unknown}"
    fi

    # Build. Profile and commit are the two facts that make a number
    # reproducible at all.
    say rustc "$(rustc --version 2>/dev/null)"
    say git_commit "$(git -C "$repo" rev-parse --short HEAD 2>/dev/null)"
    say git_dirty "$(git -C "$repo" status --porcelain 2>/dev/null | wc -l)"
    say build_profile "${RILL_BENCH_PROFILE:-debug}"
}

# --------------------------------------------------------------- sampling ---

# Bucket one process's smaps into Rill-vs-driver, the attribution
# docs/memory-footprint.md is built on. PSS, not RSS: shared driver pages
# counted once per sharer is the only honest whole-stack sum.
attribute() {
    awk '
        /^[0-9a-f]+-[0-9a-f]+ / {
            path = $6
            if (path ~ /nvidia|libcuda|libgl|GLX|glcore|glvkspirv|gpucomp|rtcore/) b = "driver"
            else if (path ~ /LLVM|llvm/)                             b = "llvm"
            else if (path ~ /vulkan_radeon|lavapipe|radv|libvulkan/) b = "mesa"
            else if (path ~ /target\/(debug|release)/)               b = "binary"
            else if (path == "" || path ~ /^\[heap\]|^\[anon/)       b = "anon"
            else                                                     b = "other"
        }
        /^Pss:/ { pss[b] += $2; total += $2 }
        END { for (k in pss) printf "%s %d\n", k, pss[k]; printf "total %d\n", total }
    ' "$1" 2>/dev/null
}

# utime+stime in clock ticks, and the context-switch counters. Ticks are
# what /proc gives; the caller converts with getconf CLK_TCK.
cpu_ticks() { awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null || echo 0; }
ctx_switches() {
    awk '/^voluntary_ctxt_switches:/{v=$2}/^nonvoluntary_ctxt_switches:/{n=$2}
         END{print v+0, n+0}' "/proc/$1/status" 2>/dev/null || echo "0 0"
}

# Count files and bytes under a tree without shelling out per file.
tree_stat() {
    [[ -d $1 ]] || { echo "0 0"; return; }
    find "$1" -type f -printf '%s\n' 2>/dev/null \
        | awk '{n++; b+=$1} END {print n+0, b+0}'
}

# Sample the running stack for `secs` seconds and write a run file.
# Everything is a *delta* across the window except memory, which is a level;
# a rate needs two readings and a level needs one.
do_sample() {
    local label=$1 secs=${2:-30}
    local out=$runs/$label.tsv
    mkdir -p "$runs"

    local tick; tick=$(getconf CLK_TCK)
    local pids=() names=()
    for name in "${procs[@]}"; do
        for pid in $(pgrep -x "$name" 2>/dev/null || true); do
            # Scope to this run's stack. Without it, a desktop or server you
            # already had running is silently folded into the totals — which
            # is how the first baseline here came to include an idle
            # files-app that had nothing to do with the measurement. Every
            # process in a bench stack carries the bench root on its command
            # line (--data, --identity, or the content root itself).
            if [[ -n ${RILL_BENCH_SCOPE:-} ]]; then
                tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null \
                    | grep -qF "$RILL_BENCH_SCOPE" || continue
            fi
            pids+=("$pid"); names+=("$name")
        done
    done
    if [[ ${#pids[@]} -eq 0 ]]; then
        echo "bench: no rill processes running — nothing to sample" >&2
        return 1
    fi

    # Opening readings.
    local -a t0 v0 n0
    local i
    for i in "${!pids[@]}"; do
        t0[i]=$(cpu_ticks "${pids[i]}")
        read -r v n <<<"$(ctx_switches "${pids[i]}")"
        v0[i]=$v; n0[i]=$n
    done
    read -r cache_n0 cache_b0 <<<"$(tree_stat "${RILL_CACHE:-$HOME/.cache/rill}")"
    local log=${RILL_BENCH_LOG:-}
    local log0=0 req0=0
    if [[ -n $log && -f $log ]]; then
        log0=$(stat -c%s "$log"); req0=$(wc -l <"$log")
    fi
    local wall0; wall0=$(date +%s.%N)

    sleep "$secs"

    local wall1; wall1=$(date +%s.%N)
    local elapsed; elapsed=$(awk -v a="$wall0" -v b="$wall1" 'BEGIN{printf "%.3f", b-a}')

    {
        printf '# run\t%s\n' "$label"
        printf 'run_label\t%s\n' "$label"
        printf 'run_date\t%s\n' "$(date -Is)"
        printf 'run_seconds\t%s\n' "$elapsed"
        emit_specs
        printf '#\n# per-process: name pid cpu_pct_of_one_core pss_kib rill_kib driver_kib rss_kib threads ctx_vol ctx_invol\n'
        local stack_pss=0 stack_own=0 stack_cpu=0
        for i in "${!pids[@]}"; do
            local pid=${pids[i]} name=${names[i]}
            [[ -r /proc/$pid/stat ]] || continue
            local t1; t1=$(cpu_ticks "$pid")
            read -r v1 n1 <<<"$(ctx_switches "$pid")"
            local cpu; cpu=$(awk -v d="$((t1 - t0[i]))" -v t="$tick" -v e="$elapsed" \
                'BEGIN{printf "%.2f", (d/t)/e*100}')
            declare -A kb=()
            while read -r bucket val; do kb[$bucket]=$val; done \
                < <(attribute "/proc/$pid/smaps")
            local total=${kb[total]:-0}
            [[ $total -eq 0 ]] && { unset kb; continue; }
            local own=$(( ${kb[anon]:-0} + ${kb[binary]:-0} ))
            local rss; rss=$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status" 2>/dev/null)
            local thr; thr=$(awk '/^Threads:/ {print $2}' "/proc/$pid/status" 2>/dev/null)
            printf 'proc\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
                "$name" "$pid" "$cpu" "$total" "$own" "${kb[driver]:-0}" \
                "${rss:-0}" "${thr:-0}" "$((v1 - v0[i]))" "$((n1 - n0[i]))"
            stack_pss=$((stack_pss + total))
            stack_own=$((stack_own + own))
            stack_cpu=$(awk -v a="$stack_cpu" -v b="$cpu" 'BEGIN{printf "%.2f", a+b}')
            unset kb
        done
        printf 'stack_pss_kib\t%s\n' "$stack_pss"
        printf 'stack_rill_kib\t%s\n' "$stack_own"
        printf 'stack_cpu_pct\t%s\n' "$stack_cpu"

        # Cache growth is the disk cost of a live desktop: a content-addressed
        # store that never collects turns an idle terminal into a disk writer.
        read -r cache_n1 cache_b1 <<<"$(tree_stat "${RILL_CACHE:-$HOME/.cache/rill}")"
        printf 'cache_objects\t%s\n' "$cache_n1"
        printf 'cache_bytes\t%s\n' "$cache_b1"
        printf 'cache_objects_added\t%s\n' "$((cache_n1 - cache_n0))"
        printf 'cache_bytes_added\t%s\n' "$((cache_b1 - cache_b0))"
        printf 'cache_bytes_per_min\t%s\n' \
            "$(awk -v d="$((cache_b1 - cache_b0))" -v e="$elapsed" 'BEGIN{printf "%d", d/e*60}')"

        # Server log volume — one line per request is a real syscall cost and
        # an unbounded file; the rate is the thing to look at.
        if [[ -n $log && -f $log ]]; then
            local log1 req1; log1=$(stat -c%s "$log"); req1=$(wc -l <"$log")
            printf 'server_log_bytes\t%s\n' "$log1"
            printf 'server_log_lines_added\t%s\n' "$((req1 - req0))"
            printf 'server_req_per_sec\t%s\n' \
                "$(awk -v d="$((req1 - req0))" -v e="$elapsed" 'BEGIN{printf "%.1f", d/e}')"
        fi

    } > "$out"

    echo "bench: wrote $out"
    render_run "$out"
}

# ------------------------------------------------------------ presentation ---

field() { sed -n "s/^$2\t//p" "$1" | head -1; }

render_run() {
    local f=$1
    printf '\n== %s  (%s, %ss)\n' "$(field "$f" run_label)" "$(field "$f" run_date)" \
        "$(field "$f" run_seconds)"
    printf '   %s | %s cores @ %s MHz (%s)\n' \
        "$(field "$f" cpu_model)" "$(field "$f" cpu_cores)" \
        "$(field "$f" cpu_mhz_max)" "$(field "$f" cpu_governor)"
    printf '   %s MiB RAM | GPU %s | VRAM %s MiB | driver %s\n' \
        "$(field "$f" ram_total_mib)" "$(field "$f" gpu_nvidia_name)" \
        "$(field "$f" vram_total_mib)" "$(field "$f" gpu_driver)"
    printf '   %s @ %s | build %s %s\n\n' \
        "$(field "$f" fs_type)" "$(field "$f" disk_model)" \
        "$(field "$f" build_profile)" "$(field "$f" git_commit)"
    printf '   %-16s %7s %9s %9s %9s %5s\n' process cpu% PSS_MiB rill_MiB drv_MiB thr
    awk -F'\t' '$1=="proc" {printf "   %-16s %6s%% %9.1f %9.1f %9.1f %5s\n",
        $2, $4, $5/1024, $6/1024, $7/1024, $9}' "$f"
    printf '   %-16s %6s%% %9.1f %9.1f\n' TOTAL \
        "$(field "$f" stack_cpu_pct)" \
        "$(awk -v k="$(field "$f" stack_pss_kib)" 'BEGIN{print k/1024}')" \
        "$(awk -v k="$(field "$f" stack_rill_kib)" 'BEGIN{print k/1024}')"
    printf '\n   cache      %s objects, %.1f MiB total, +%.2f MiB during run (%.2f MiB/min)\n' \
        "$(field "$f" cache_objects)" \
        "$(awk -v b="$(field "$f" cache_bytes)" 'BEGIN{print b/1048576}')" \
        "$(awk -v b="$(field "$f" cache_bytes_added)" 'BEGIN{print b/1048576}')" \
        "$(awk -v b="$(field "$f" cache_bytes_per_min)" 'BEGIN{print b/1048576}')"
    local rps; rps=$(field "$f" server_req_per_sec)
    [[ -n $rps ]] && printf '   server     %s log lines/s, log now %.1f MiB\n' "$rps" \
        "$(awk -v b="$(field "$f" server_log_bytes)" 'BEGIN{print b/1048576}')"
    echo
}

do_compare() {
    local a=$runs/$1.tsv b=$runs/$2.tsv
    for f in "$a" "$b"; do
        [[ -f $f ]] || { echo "bench: no run '$f'" >&2; exit 1; }
    done
    # Refuse to compare across machines: the whole point of the spec block is
    # that two numbers taken on different hardware are not a delta.
    local ka kb
    ka=$(field "$a" cpu_model)$(field "$a" gpu_devices)
    kb=$(field "$b" cpu_model)$(field "$b" gpu_devices)
    [[ $ka == "$kb" ]] || echo "WARNING: different machines — this is not a delta" >&2

    printf '\n%-26s %14s %14s %12s\n' metric "$1" "$2" change
    row() {
        local name=$1 va=$2 vb=$3 unit=${4:-}
        awk -v n="$name" -v a="$va" -v b="$vb" -v u="$unit" 'BEGIN{
            d = (a+0 == 0) ? 0 : (b-a)/a*100
            printf "%-26s %14s %14s %11.1f%%\n", n u, a, b, d
        }'
    }
    row "stack PSS (MiB)" \
        "$(awk -v k="$(field "$a" stack_pss_kib)" 'BEGIN{printf "%.1f",k/1024}')" \
        "$(awk -v k="$(field "$b" stack_pss_kib)" 'BEGIN{printf "%.1f",k/1024}')"
    row "rill-attributable (MiB)" \
        "$(awk -v k="$(field "$a" stack_rill_kib)" 'BEGIN{printf "%.1f",k/1024}')" \
        "$(awk -v k="$(field "$b" stack_rill_kib)" 'BEGIN{printf "%.1f",k/1024}')"
    row "stack CPU (% of 1 core)" "$(field "$a" stack_cpu_pct)" "$(field "$b" stack_cpu_pct)"
    row "cache growth (MiB/min)" \
        "$(awk -v b="$(field "$a" cache_bytes_per_min)" 'BEGIN{printf "%.2f",b/1048576}')" \
        "$(awk -v b="$(field "$b" cache_bytes_per_min)" 'BEGIN{printf "%.2f",b/1048576}')"
    local ra rb
    ra=$(field "$a" server_req_per_sec); rb=$(field "$b" server_req_per_sec)
    row "server log (lines/s)" "${ra:-0}" "${rb:-0}"
    ra=$(field "$a" compositor_mean_fps); rb=$(field "$b" compositor_mean_fps)
    row "compositor mean fps" "${ra:-0}" "${rb:-0}"
    echo
    echo "Machine: $(field "$a" cpu_model) / $(field "$a" gpu_nvidia_name) / $(field "$a" build_profile) build"
    echo "Commits: $1 @ $(field "$a" git_commit)   →   $2 @ $(field "$b" git_commit)"
    echo
}

# ------------------------------------------------------------------- run ---

# Stand up an isolated desktop, let it settle, sample it, tear it down.
# Isolated means: own port, own content root, own data dir, own cache, own
# theme. A bench run must never be the reason your desktop changed.
do_run() {
    local label=$1 secs=${2:-30}
    local root=$bench/stack
    mkdir -p "$root/config/rill" "$root/cache" "$runs"

    export RILL_DEMO_ROOT=$root/demo
    export RILL_DEMO_PORT=$port
    export RILL_CACHE=$root/cache
    export XDG_CONFIG_HOME=$root/config
    export RILL_BENCH_LOG=$root/demo/server.log
    export RILL_BENCH_DESKTOP_LOG=$root/desktop.log
    export RILL_BENCH_SCOPE=$root

    # A fixed theme: the workload has to be the same shape in both runs, so
    # it is written here rather than copied from whatever you are ricing.
    if [[ ! -f $root/config/rill/theme.toml ]]; then
        cat > "$root/config/rill/theme.toml" <<THEME
# Bench workload — fixed on purpose. Two widgets on live clocks plus a
# dock is the steady state we are measuring; changing it changes the
# baseline, so change it deliberately and re-run both sides.
[colors]
page = "#0a0a0a"

[[desktop.widgets]]
app = "rill://127.0.0.1:$port/meter"
anchor = "top-right"
width = 300
height = 132
x = 20
y = 20

[[desktop.widgets]]
app = "rill://127.0.0.1:$port/ascii"
anchor = "bottom-left"
width = 380
height = 210
x = 20
y = 20
THEME
    fi

    echo "==> preparing isolated stack in $root (port $port)"
    "$repo/scripts/demo-desktop.sh" >"$root/setup.log" 2>&1 \
        || { echo "bench: setup failed, see $root/setup.log" >&2; exit 1; }

    # Nested mode needs a host compositor to present into. A tty shell has
    # no WAYLAND_DISPLAY of its own, so fall back to the session's socket.
    if [[ -z ${WAYLAND_DISPLAY:-} ]]; then
        local sock
        sock=$(ls "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"/wayland-[0-9] 2>/dev/null | head -1)
        [[ -n $sock ]] || { echo "bench: no wayland socket to nest in" >&2; exit 1; }
        export WAYLAND_DISPLAY=$(basename "$sock")
        echo "==> nesting in WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
    fi

    echo "==> launching desktop"
    # Same profile the setup step built and the spec block records — see
    # demo-desktop.sh. Hardcoding target/debug here would have made
    # `RILL_BENCH_PROFILE=release` a label on a debug measurement.
    local bin=$repo/target/${RILL_BENCH_PROFILE:-debug}
    ( LD_LIBRARY_PATH="${RILL_LD_PATH:-$([[ -d /usr/lib64 ]] && echo /usr/lib64)}" "$bin/rill-compositor" \
        "$bin/rill-vector" --dock \
            --data "$root/demo/data" --identity "$root/demo/identity-device" \
        >"$root/desktop.log" 2>&1 & echo $! > "$root/desktop.pid" )

    # Settle: first frames build glyph atlases and compile shaders, and
    # measuring that transient as if it were steady state is how you get a
    # number that never reproduces.
    echo "==> settling (12s)"
    sleep 12
    if ! pgrep -x rill-compositor >/dev/null; then
        echo "bench: compositor did not stay up, see $root/desktop.log" >&2
        tail -20 "$root/desktop.log" >&2
        exit 1
    fi

    echo "==> sampling ${secs}s"
    do_sample "$label" "$secs"

    echo "==> tearing down"
    [[ -f $root/desktop.pid ]] && kill "$(cat "$root/desktop.pid")" 2>/dev/null || true
    sleep 2
    pkill -x rill-vector 2>/dev/null || true
    pkill -x rill-compositor 2>/dev/null || true
    [[ -f $root/demo/server.pid ]] && kill "$(cat "$root/demo/server.pid")" 2>/dev/null || true

    # The compositor reports its lifetime frame count as it exits, so this
    # can only be collected after the teardown above. It covers settle plus
    # sample, which is comparable as long as both runs use the same timings.
    local frames uptime
    frames=$(sed -n 's/.*frames=\([0-9]*\).*/\1/p' "$root/desktop.log" | tail -1)
    uptime=$(sed -n 's/.*uptime=\([0-9.]*\)s.*/\1/p' "$root/desktop.log" | tail -1)
    if [[ -n $frames ]]; then
        {
            printf 'compositor_frames\t%s\n' "$frames"
            printf 'compositor_uptime_s\t%s\n' "${uptime:-0}"
            printf 'compositor_mean_fps\t%s\n' \
                "$(awk -v f="$frames" -v u="${uptime:-1}" 'BEGIN{printf "%.2f", f/u}')"
        } >> "$runs/$label.tsv"
        printf '   compositor %s frames in %ss (mean %.2f fps over settle+sample)\n\n' \
            "$frames" "${uptime:-0}" \
            "$(awk -v f="$frames" -v u="${uptime:-1}" 'BEGIN{print f/u}')"
    fi
    true
}

case ${1:-} in
    specs)   emit_specs ;;
    sample)  shift; do_sample "$@" ;;
    run)     shift; do_run "$@" ;;
    compare) shift; do_compare "$@" ;;
    *)
        sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
        exit 1
        ;;
esac
