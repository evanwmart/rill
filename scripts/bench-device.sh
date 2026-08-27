#!/usr/bin/env bash
# Rill cross-device benchmark — one reproducible measurement bundle per run.
#
#   scripts/bench-device.sh
#   scripts/bench-device.sh --profile release --idle-seconds 60 --scale 1,5,10,20
#   scripts/bench-device.sh --skip-busy --skip-scale        # environment + idle only
#   scripts/bench-device.sh --help
#
# It answers one question, the same way on every machine:
#
#   What does Rill cost here — at idle, under a stated workload, and as the
#   number of applications grows?
#
# It is a measurement instrument, not a score generator. Everything it prints
# was observed or is arithmetic over observations; anything the platform does
# not expose comes out `null`, never estimated. Raw /proc and /sys captures
# are kept so attribution can be improved later without touching hardware
# again.
#
# Units, everywhere, without exception:
#   memory   MiB (KiB/1024 — /proc reports KiB, not kB)
#   cpu      percent of ONE logical core (400% = four cores busy)
#   time     seconds, or milliseconds where the field says _ms
#   temp     °C
#
# See docs/memory-footprint.md for the log these runs feed, and
# docs/resource-envelope.md for what may and may not be claimed from them.
set -uo pipefail

readonly SCRIPT_VERSION=1
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# ------------------------------------------------------------------ options ---

profile=release
out_root=$repo/bench-results
resolution=
refresh=
idle_seconds=60
busy_seconds=60
settle_seconds=12
scale_list=1,5,10,20
ascii_rate=0.08
meter_hz=1
skip_busy=false
skip_scale=false
skip_power=false
skip_network=false
existing_session=false
notes=

usage() {
    sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
    cat <<'USAGE'

Options
  --profile debug|release   which build to measure (verified against the
                            binary's actual path, not taken on trust)
  --output DIR              where the result bundle goes
  --resolution WxH          recorded, and requested of the compositor
  --refresh HZ              recorded only
  --idle-seconds N          idle sampling window (default 60)
  --busy-seconds N          busy sampling window (default 60)
  --settle-seconds N        wait before each window (default 12)
  --scale LIST              app counts, comma separated (default 1,5,10,20)
  --ascii-rate SECONDS      ASCII widget period for the busy workload
  --skip-busy               skip the busy workload
  --skip-scale              skip the scaling test
  --skip-power              do not probe power sensors
  --skip-network            do not read interface counters
  --existing-session        measure a Rill already running (records
                            hermetic=false; the run is not comparable to a
                            hermetic one and says so)
  --notes TEXT              free text recorded with the run
USAGE
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --profile)         profile=${2:?}; shift 2 ;;
        --output)          out_root=${2:?}; shift 2 ;;
        --resolution)      resolution=${2:?}; shift 2 ;;
        --refresh)         refresh=${2:?}; shift 2 ;;
        --idle-seconds)    idle_seconds=${2:?}; shift 2 ;;
        --busy-seconds)    busy_seconds=${2:?}; shift 2 ;;
        --settle-seconds)  settle_seconds=${2:?}; shift 2 ;;
        --scale)           scale_list=${2:?}; shift 2 ;;
        --ascii-rate)      ascii_rate=${2:?}; shift 2 ;;
        --skip-busy)       skip_busy=true; shift ;;
        --skip-scale)      skip_scale=true; shift ;;
        --skip-power)      skip_power=true; shift ;;
        --skip-network)    skip_network=true; shift ;;
        --existing-session) existing_session=true; shift ;;
        --notes)           notes=${2:?}; shift 2 ;;
        -h|--help)         usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

case $profile in
    debug|release) ;;
    *) echo "--profile must be debug or release, got '$profile'" >&2; exit 2 ;;
esac

# --------------------------------------------------------------- run bundle ---

run_id="$(date +%Y-%m-%dT%H%M%S%z)_$(uname -n)"
rundir=$out_root/$run_id
# Never overwrite: a result bundle is evidence, and evidence that can be
# silently replaced is not evidence.
if [[ -e $rundir ]]; then
    rundir="${rundir}_$$"
fi
mkdir -p "$rundir"/{stdout,stderr,proc,sys,logs} || {
    echo "cannot write $rundir" >&2; exit 1; }

env_file=$rundir/environment.txt
cmd_file=$rundir/commands.txt
samples_csv=$rundir/samples.csv
procs_csv=$rundir/processes.csv
scaling_csv=$rundir/scaling.csv
flags_file=$rundir/flags.txt
: > "$env_file"; : > "$cmd_file"; : > "$flags_file"
echo "timestamp_s,phase,app_count,stack_pss_kib,stack_rss_kib,stack_swap_kib,mem_available_kib,swap_used_kib,cpu_percent_one_core,rx_bytes,tx_bytes,temperature_c,power_w" > "$samples_csv"
echo "timestamp_s,phase,pid,process,role,rss_kib,pss_kib,swap_kib,cpu_percent_one_core" > "$procs_csv"
echo "apps,stack_pss_kib,delta_from_baseline_kib,delta_from_previous_kib,mean_client_pss_kib,compositor_pss_kib,mem_available_kib,swap_used_kib,cpu_percent_one_core" > "$scaling_csv"

status=partial
failure_phase=null
failure_reason=null

# Every phase result, declared before anything can fail.
#
# `cleanup` writes the summary on *every* exit path, including one taken
# before a phase has run. Under `set -u` an unset variable inside
# `write_summary`'s heredoc aborts it — after the redirection has already
# truncated summary.json — so a run that failed early produced an empty file
# instead of the evidence it had collected. Declaring the results here means
# the heredoc always has something to interpolate, and the value it prints for
# a phase that never ran is the honest one: null.
idle_pss=; idle_rss=; idle_cpu=; idle_fps=; idle_temp=; idle_swap=; busy_swap=
idle_frame_ms=; busy_frame_ms=
idle_frames=; idle_damage=; idle_heartbeat=; idle_mem_available=
busy_pss=; busy_cpu=; busy_fps=; busy_temp=
scale_baseline=; scale_reached=0; scale_limit=null; post_close_delta=
# Which renderer the compositor actually got. A run on a software rasterizer
# measures a different machine than the one it claims to.
gpu_adapter=; software_renderer=false
# Raspberry Pi throttle/undervolt word, when the board exposes one. Empty
# everywhere else, and empty is not the same claim as "not throttled".
throttled_state=
# Declared here for the same reason: `${phase_watts[idle]:-}` on an array that
# does not exist yet is an unbound-variable error under `set -u`, not an empty
# string — the `:-` guard never gets the chance to apply.
declare -A phase_pss phase_cpu phase_rss phase_watts

# `record KEY VALUE` — one fact per line, for the human-readable inventory.
record() { printf '%s\t%s\n' "$1" "${2:-unknown}" >> "$env_file"; }
# `flag NAME` — a condition that may make this run incomparable to another.
flag()   { printf '%s\n' "$1" >> "$flags_file"; }
# `run_cmd LABEL cmd...` — capture a command's output as raw evidence.
run_cmd() {
    local label=$1; shift
    printf '%s: %s\n' "$label" "$*" >> "$cmd_file"
    "$@" > "$rundir/stdout/$label.txt" 2> "$rundir/stderr/$label.txt" || true
}
say() { printf '%s\n' "$*" >&2; }

# Monotonic seconds, from /proc rather than `date`: cheap, and immune to the
# wall clock being adjusted mid-run.
mono() { awk '{printf "%.3f", $1}' /proc/uptime; }
start_mono=$(mono)
since() { awk -v a="$1" -v b="$(mono)" 'BEGIN{printf "%.3f", b-a}'; }

meminfo() { awk -v k="$1:" '$1==k{print $2; exit}' /proc/meminfo; }

# --------------------------------------------------------------- teardown ---

bench_root=
declare -a own_pids=()

cleanup() {
    local code=$?
    trap - EXIT INT TERM
    say "==> cleaning up"
    # Only ever kill what this run started, identified by the bench root on
    # the command line. A developer's own desktop must survive a benchmark.
    if [[ -n $bench_root ]]; then
        for name in rill-compositor rill-vector files-app rill-server; do
            for pid in $(pgrep -x "$name" 2>/dev/null || true); do
                if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF "$bench_root"; then
                    kill "$pid" 2>/dev/null || true
                fi
            done
        done
        sleep 2
        for name in rill-compositor rill-vector files-app rill-server; do
            for pid in $(pgrep -x "$name" 2>/dev/null || true); do
                if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF "$bench_root"; then
                    kill -9 "$pid" 2>/dev/null || true
                fi
            done
        done
    fi
    # Whatever was collected before a failure is still evidence: write the
    # summary regardless of how the run ended.
    write_summary || true
    say "==> results in $rundir"
    exit "$code"
}
fail_phase() {
    failure_phase=\"$1\"
    failure_reason=\"$2\"
    say "!! $1: $2"
}

# ---------------------------------------------- sensors (summary inputs) ---
#
# These live above the trap because `write_summary` calls them, and the
# summary is written on every exit path — including one taken before the
# sampling section below has been reached. A helper defined after the trap
# is a helper that does not exist when a run fails early.

# Every temperature sensor the machine exposes, as "name=celsius" lines.
# Both trees are read: `/sys/class/thermal` alone misses the CPU entirely on
# some machines (an AMD desktop may expose only `acpitz`, a motherboard
# sensor sitting 40 degrees below the package), while hwmon carries k10temp,
# coretemp, and the Pi's cpu_thermal.
thermal_readings() {
    local t name
    for z in /sys/class/thermal/thermal_zone*; do
        [[ -r $z/temp ]] || continue
        t=$(cat "$z/temp" 2>/dev/null) || continue
        name=$(cat "$z/type" 2>/dev/null || basename "$z")
        (( t > 1000 )) && t=$((t / 1000))
        echo "$name=$t"
    done
    for h in /sys/class/hwmon/hwmon*; do
        [[ -r $h/name ]] || continue
        name=$(cat "$h/name" 2>/dev/null)
        for f in "$h"/temp*_input; do
            [[ -r $f ]] || continue
            t=$(cat "$f" 2>/dev/null) || continue
            (( t > 1000 )) && t=$((t / 1000))
            echo "$name.$(basename "$f" _input)=$t"
        done
    done
}
# The hottest sensor on the machine, in °C. Which one it was is recorded
# separately rather than assumed to be the CPU.
temperature_c() {
    thermal_readings | awk -F= '{ if ($2+0 > best) { best=$2+0 } } END { if (best) print best }'
}
temperature_source() {
    thermal_readings | awk -F= '{ if ($2+0 > best) { best=$2+0; who=$1 } } END { if (who) print who }'
}

# Whole-package energy, in microjoules, from RAPL. Power is a *delta* over
# time, so this returns the counter and the caller differences it.
#
# Deliberately narrow: the only sources trusted here are ones that mean the
# machine (or its CPU package). An `amdgpu power1_input` rail is a real
# sensor reading a real thing, but reporting a GPU rail as the machine's
# power draw would be exactly the kind of number this script exists to stop
# anyone publishing. Everything found is still written to raw evidence.
energy_uj() {
    $skip_power && { echo ""; return; }
    local f
    for f in /sys/class/powercap/intel-rapl:0/energy_uj \
             /sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj; do
        [[ -r $f ]] && { cat "$f" 2>/dev/null; return; }
    done
    echo ""
}
power_source() {
    [[ -n $(energy_uj) ]] && echo "rapl package" || echo ""
}


# ------------------------------------------------------------- reporting ---

# KiB → MiB. An empty input is a phase that did not run: that is `null`, never
# 0.0. A measured zero still prints 0.0 — the two are different claims, and the
# default-to-zero this used to do made a run that never started look like a run
# that measured nothing.
mib() { awk -v k="${1:-}" 'BEGIN{ if (k=="") print "null"; else printf "%.1f", k/1024 }'; }
jnum() { [[ -z ${1:-} || $1 == unknown ]] && echo null || echo "$1"; }
jstr() { [[ -z ${1:-} ]] && echo null || printf '"%s"' "${1//\"/\\\"}"; }
# "frame_ms mean=1.2 p50=… " -> {"mean":1.2,"p50":…}, or null if the
# compositor did not report one (an older build, or a run with no frames).
jframe() {
    [[ -z ${1:-} ]] && { echo null; return; }
    awk -v s="$1" 'BEGIN{
        n = split(s, f, " "); out = "{"; sep = "";
        for (i = 1; i <= n; i++) if (split(f[i], kv, "=") == 2) {
            out = out sep "\"" kv[1] "\": " kv[2]; sep = ", ";
        }
        print out "}"
    }'
}

write_summary() {
    local slope=null mean_marginal=null
    if [[ -s $scaling_csv ]] && [[ $(wc -l < "$scaling_csv") -gt 1 ]]; then
        # Least squares over the measured points: PSS ≈ intercept + slope × N.
        # DERIVED, and labelled as such wherever it is shown.
        read -r slope mean_marginal < <(awk -F, 'NR>1 {
                n++; x+=$1; y+=$2; xx+=$1*$1; xy+=$1*$2; m+=$3/($1?$1:1)
            } END {
                if (n>1 && (n*xx - x*x) != 0)
                    printf "%.2f %.2f", ((n*xy - x*y)/(n*xx - x*x))/1024, (m/n)/1024;
                else printf "null null"
            }' "$scaling_csv")
    fi

    cat > "$rundir/summary.json" <<JSON
{
  "schema_version": 1,
  "script_version": $SCRIPT_VERSION,
  "run_id": "$run_id",
  "status": "$status",
  "failure_phase": $failure_phase,
  "failure_reason": $failure_reason,
  "hermetic": $($existing_session && echo false || echo true),
  "notes": $(jstr "$notes"),
  "flags": [$(paste -sd, /dev/null; awk '{printf "%s\"%s\"", (NR>1?",":""), $0}' "$flags_file" 2>/dev/null)],

  "environment": {
    "hostname": "$(uname -n)",
    "architecture": "$(uname -m)",
    "os": "$( . /etc/os-release 2>/dev/null && echo "$PRETTY_NAME")",
    "kernel": "$(uname -r)",
    "cpu": "$(awk -F': ' '/^model name|^Model/{print $2; exit}' /proc/cpuinfo)",
    "logical_cpus": $(getconf _NPROCESSORS_ONLN 2>/dev/null || echo null),
    "memory_total_mib": $(mib "$(meminfo MemTotal)"),
    "swap_total_mib": $(mib "$(meminfo SwapTotal)"),
    "cpu_governor": $(jstr "$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null)")
  },

  "graphics": {
    "adapter": $(jstr "${gpu_adapter:-}"),
    "software_renderer": ${software_renderer:-false},
    "throttled": $(jstr "${throttled_state:-}")
  },

  "rill": {
    "git_commit": "$(git -C "$repo" rev-parse --short HEAD 2>/dev/null)",
    "git_dirty": $([[ ${git_dirty:-0} -gt 0 ]] && echo true || echo false),
    "profile": "$profile",
    "binary_root": "${bin:-}"
  },

  "workload": {
    "idle": "idle-v1",
    "busy": "widgets-v1",
    "meter_hz": $meter_hz,
    "ascii_seconds": $ascii_rate,
    "settle_seconds": $settle_seconds,
    "idle_seconds": $idle_seconds,
    "busy_seconds": $busy_seconds
  },

  "idle": {
    "stack_pss_mib": $(mib "${idle_pss:-}"),
    "stack_rss_mib": $(mib "${idle_rss:-}"),
    "cpu_percent_one_core": $(jnum "${idle_cpu:-}"),
    "mean_fps": $(jnum "${idle_fps:-}"),
    "frames_total": $(jnum "${idle_frames:-}"),
    "frames_damage": $(jnum "${idle_damage:-}"),
    "frames_heartbeat": $(jnum "${idle_heartbeat:-}"),
    "available_memory_mib": $(mib "${idle_mem_available:-}"),
    "swap_used_mib": $(mib "${idle_swap:-}"),
    "frame_ms": $(jframe "${idle_frame_ms:-}")
  },

  "busy": {
    "stack_pss_mib": $(mib "${busy_pss:-}"),
    "cpu_percent_one_core": $(jnum "${busy_cpu:-}"),
    "mean_fps": $(jnum "${busy_fps:-}"),
    "swap_used_mib": $(mib "${busy_swap:-}"),
    "frame_ms": $(jframe "${busy_frame_ms:-}")
  },

  "scaling": {
    "baseline_pss_mib": $(mib "${scale_baseline:-}"),
    "reached_apps": ${scale_reached:-0},
    "limit_reason": ${scale_limit:-null},
    "mean_marginal_pss_mib": $(jnum "${mean_marginal:-null}"),
    "linear_slope_mib_per_app": $(jnum "${slope:-null}"),
    "post_close_delta_mib": $(mib "${post_close_delta:-}")
  },

  "network": {
    "available": $($skip_network && echo false || echo true),
    "scope": "loopback interface totals — includes any other local traffic",
    "rx_bytes": $(jnum "$(( ${end_rx:-0} - ${base_rx:-0} ))"),
    "tx_bytes": $(jnum "$(( ${end_tx:-0} - ${base_tx:-0} ))"),
    "server_wire": $(cat "$RILL_STATS" 2>/dev/null || echo null)
  },

  "cache": {
    "growth_mib": $(mib "$(( ${cache_after:-0} - ${cache_before_idle:-0} ))")
  },

  "power": {
    "available": $([[ -n $(power_source) ]] && echo true || echo false),
    "source": $(jstr "$(power_source)"),
    "reason": $([[ -n $(power_source) ]] && echo null || echo '"no whole-system or package power counter (GPU rails are not reported as system power)"'),
    "idle_watts": $(jnum "${phase_watts[idle]:-}"),
    "busy_watts": $(jnum "${phase_watts[busy]:-}")
  },

  "thermal": {
    "available": $([[ -n $(temperature_c) ]] && echo true || echo false),
    "hottest_sensor": $(jstr "$(temperature_source)"),
    "idle_peak_c": $(jnum "${idle_temp:-}"),
    "busy_peak_c": $(jnum "${busy_temp:-}")
  }
}
JSON

    {
        echo "Rill device benchmark"
        echo "====================="
        echo
        echo "Run"
        printf '  Host:              %s (%s)\n' "$(uname -n)" "$(uname -m)"
        printf '  Profile:           %s @ %s\n' "$profile" "$(git -C "$repo" rev-parse --short HEAD 2>/dev/null)"
        printf '  Status:            %s\n' "$status"
        printf '  Hermetic:          %s\n' "$($existing_session && echo no || echo yes)"
        printf '  Renderer:          %s\n' "${gpu_adapter:-unknown}"
        ${software_renderer:-false} && printf '  !! SOFTWARE RENDERER — GPU numbers describe llvmpipe, not this machine\n'
        [[ -n ${throttled_state:-} ]] && printf '  Throttling:        %s\n' "$throttled_state"
        echo
        echo "Idle desktop (idle-v1)"
        printf '  Stack PSS:         %s MiB\n' "$(mib "${idle_pss:-}")"
        printf '  CPU:               %s%% of one core\n' "${idle_cpu:-null}"
        printf '  Mean FPS:          %s\n' "${idle_fps:-null}"
        printf '  Frames:            %s total, %s damage, %s heartbeat\n' \
            "${idle_frames:-?}" "${idle_damage:-?}" "${idle_heartbeat:-?}"
        printf '  Available RAM:     %s MiB\n' "$(mib "${idle_mem_available:-}")"
        echo
        if ! $skip_busy; then
            echo "Busy desktop (widgets-v1, ascii ${ascii_rate}s)"
            printf '  Stack PSS:         %s MiB\n' "$(mib "${busy_pss:-}")"
            printf '  CPU:               %s%% of one core\n' "${busy_cpu:-null}"
            printf '  Mean FPS:          %s\n' "${busy_fps:-null}"
            echo
        fi
        if [[ $(wc -l < "$scaling_csv") -gt 1 ]]; then
            echo "Scaling (MEASURED points; slope is DERIVED)"
            printf '  %-8s %-14s %s\n' apps "stack PSS" "from baseline"
            awk -F, 'NR>1 { printf "  %-8s %-14s %s\n", $1, sprintf("%.1f MiB", $2/1024), sprintf("%+.1f MiB", $3/1024) }' "$scaling_csv"
            printf '  Slope:             %s MiB/app (DERIVED, least squares)\n' "${slope:-null}"
            printf '  After close:       %s MiB from baseline\n' "$(mib "${post_close_delta:-}")"
            [[ $scale_limit != null ]] && printf '  Limit:             %s\n' "$scale_limit"
            echo
        fi
        printf 'Cache growth        %s MiB\n' "$(mib "$(( ${cache_after:-0} - ${cache_before_idle:-0} ))")"
        printf 'Network (loopback)  rx %s  tx %s bytes\n' \
            "$(( ${end_rx:-0} - ${base_rx:-0} ))" "$(( ${end_tx:-0} - ${base_tx:-0} ))"
        if [[ -n $(power_source) ]]; then
            printf 'Power               idle %s W, busy %s W (%s)\n' \
                "${phase_watts[idle]:-?}" "${phase_watts[busy]:-?}" "$(power_source)"
        else
            printf 'Power               unavailable (no package counter)\n'
        fi
        printf 'Peak temperature    %s °C (%s)\n' \
            "$(temperature_c || echo '?')" "$(temperature_source || echo 'no sensor')"
        echo
        if [[ -s $flags_file ]]; then
            echo "Flags (annotations, not failures)"
            sed 's/^/  /' "$flags_file"
            echo
        fi
        echo "Bundle: $rundir"
    } > "$rundir/summary.txt"
}


trap cleanup EXIT INT TERM

# ------------------------------------------------- phase 0: preflight ---
#
# After the trap, deliberately: `cleanup` writes the summary, so every phase —
# including the first one — has to be able to fail into a bundle that says why.

bin=$repo/target/$profile
say "==> preflight"
missing=0
for exe in rill-compositor rill-vector rill files-app; do
    if [[ ! -x $bin/$exe ]]; then
        say "missing $profile binary: $bin/$exe"
        missing=1
    fi
done
if [[ $missing -eq 1 ]]; then
    say "build them first:  cargo build ${profile/debug/} ${profile/release/--release} --workspace"
    fail_phase preflight "missing $profile binaries"
    exit 1
fi
# The profile is verified from the path the binary actually lives at, not from
# a label. A run marked `release` that measured target/debug is worse than no
# run at all, and the previous tooling permitted exactly that.
record script_version "$SCRIPT_VERSION"
record run_id "$run_id"
record profile "$profile"
record binary_root "$bin"
record notes "${notes:-none}"
[[ $profile == debug ]] && flag "debug build — not comparable with a release run"

for opt in vulkaninfo nvidia-smi vcgencmd lspci glxinfo systemd-analyze journalctl; do
    command -v "$opt" >/dev/null 2>&1 || { record "OPTIONAL_MISSING" "$opt"; }
done

# --------------------------------------- phase 1: environment inventory ---

say "==> environment"
record hostname "$(uname -n)"
record architecture "$(uname -m)"
record kernel "$(uname -r)"
record kernel_full "$(uname -a)"
record os "$( . /etc/os-release 2>/dev/null && echo "$PRETTY_NAME" )"
record cpu_model "$(awk -F': ' '/^model name|^Model/{print $2; exit}' /proc/cpuinfo)"
record logical_cpus "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
record cpu_governor "$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown)"
record cpu_freq_max_khz "$(cat /sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq 2>/dev/null || echo unknown)"
record memory_total_kib "$(meminfo MemTotal)"
record memory_available_kib "$(meminfo MemAvailable)"
record swap_total_kib "$(meminfo SwapTotal)"
record clock_ticks_per_sec "$(getconf CLK_TCK)"
record fs_type_repo "$(df --output=fstype "$repo" 2>/dev/null | tail -1 | tr -d ' ')"
record fs_avail_kib_repo "$(df --output=avail "$repo" 2>/dev/null | tail -1 | tr -d ' ')"
record session_type "${XDG_SESSION_TYPE:-unknown}"
record host_desktop "${XDG_CURRENT_DESKTOP:-${DESKTOP_SESSION:-none}}"
record git_commit "$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)"
record git_branch "$(git -C "$repo" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
git_dirty=$(git -C "$repo" status --porcelain 2>/dev/null | wc -l)
record git_dirty_files "$git_dirty"
[[ ${git_dirty:-0} -gt 0 ]] && flag "dirty git tree — the binaries may not match the recorded commit"
[[ -n ${SSH_CONNECTION:-} ]] && flag "run over SSH"
[[ -n ${WAYLAND_DISPLAY:-} ]] && flag "nested inside a host compositor (WAYLAND_DISPLAY=$WAYLAND_DISPLAY)"
[[ $(meminfo SwapTotal) -gt 0 ]] && flag "swap is enabled"
$existing_session && flag "not hermetic — measured an already-running session"
cp /proc/meminfo "$rundir/proc/meminfo.boot" 2>/dev/null || true
cp /proc/cpuinfo "$rundir/proc/cpuinfo" 2>/dev/null || true
run_cmd lscpu lscpu

# ------------------------------------------- phase 2: GPU/display inventory ---

say "==> graphics"
command -v lspci >/dev/null 2>&1 && run_cmd lspci lspci -nn
command -v vulkaninfo >/dev/null 2>&1 && run_cmd vulkaninfo vulkaninfo --summary
command -v glxinfo  >/dev/null 2>&1 && run_cmd glxinfo glxinfo -B
command -v nvidia-smi >/dev/null 2>&1 && run_cmd nvidia-smi nvidia-smi -q
record vulkan_icds "$(ls /usr/share/vulkan/icd.d/*.json 2>/dev/null | xargs -r -n1 basename | paste -sd, - || echo none)"
# `vcgencmd get_throttled` is a Raspberry Pi firmware call; on every other
# board the tool is absent and this records nothing. Kept in the normal path
# (guarded like vulkaninfo/nvidia-smi above) rather than behind a --pi flag,
# because a throttled board looks exactly like a slow one in every other
# number this script prints. Read twice — before any load, and again after the
# busy phase — since throttling is a during-load fact, and the firmware word
# is sticky (bit 16+ mean "has been throttled since boot").
read_throttled() {   # $1 = when
    command -v vcgencmd >/dev/null 2>&1 || return 0
    local word
    word=$(vcgencmd get_throttled 2>/dev/null | cut -d= -f2)
    [[ -n $word ]] || return 0
    record "throttled_$1" "$word"
    throttled_state="$1=$word"
    [[ $word != 0x0 ]] && flag "board reports throttling/undervoltage ($1=$word)"
    return 0
}
read_throttled before_load
# Modes, from DRM, so a headless or nested run still records something real.
if [[ -d /sys/class/drm ]]; then
    for m in /sys/class/drm/*/modes; do
        [[ -s $m ]] || continue
        record "drm_mode_$(basename "$(dirname "$m")")" "$(head -1 "$m")"
    done
    cp -r /sys/class/drm "$rundir/sys/drm" 2>/dev/null || true
fi
record resolution_requested "${resolution:-unset}"
record refresh_hz_recorded "${refresh:-unknown}"

# --------------------------------------------- phase 3: binary inventory ---

say "==> binaries"
for exe in rill-compositor rill-vector rill-server rill files-app; do
    p=$bin/$exe
    [[ -x $p ]] || continue
    record "binary_${exe}_path" "$p"
    record "binary_${exe}_bytes" "$(stat -c%s "$p")"
    record "binary_${exe}_sha256" "$(sha256sum "$p" | cut -d' ' -f1)"
    if command -v file >/dev/null 2>&1; then
        record "binary_${exe}_file" "$(file -b "$p")"
    fi
done

# ---------------------------------------------------- sampling primitives ---

# Sum PSS/RSS/Swap over a set of pids, from smaps_rollup — one file per
# process, cheap enough for a 1 Hz loop on a small board. Full smaps is only
# read at checkpoints.
stack_memory() {   # -> "pss_kib rss_kib swap_kib"
    local pss=0 rss=0 swap=0 v
    for pid in "$@"; do
        [[ -r /proc/$pid/smaps_rollup ]] || continue
        while read -r key v _; do
            case $key in
                Pss:)  pss=$((pss + v)) ;;
                Rss:)  rss=$((rss + v)) ;;
                Swap:) swap=$((swap + v)) ;;
            esac
        done < "/proc/$pid/smaps_rollup"
    done
    echo "$pss $rss $swap"
}
proc_mem() {       # pid -> "pss_kib rss_kib swap_kib"
    local pss=0 rss=0 swap=0 v
    [[ -r /proc/$1/smaps_rollup ]] || { echo "0 0 0"; return; }
    while read -r key v _; do
        case $key in
            Pss:)  pss=$v ;;
            Rss:)  rss=$v ;;
            Swap:) swap=$v ;;
        esac
    done < "/proc/$1/smaps_rollup"
    echo "$pss $rss $swap"
}
cpu_ticks() { awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null || echo 0; }

# Every Rill process belonging to *this* run, found by the bench root on its
# command line. Name matching alone would sweep in a desktop the developer
# already had open — a mistake this tooling has made before.
own_processes() {
    local pid
    for name in rill-compositor rill-vector files-app rill-server; do
        for pid in $(pgrep -x "$name" 2>/dev/null || true); do
            if [[ -z $bench_root ]] || tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null \
                 | grep -qF "$bench_root"; then
                echo "$pid $name"
            fi
        done
    done
}
# compositor | dock | widget | app | server | other_rill
role_of() {
    local pid=$1 name=$2 cmd
    cmd=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null)
    case $name in
        rill-compositor) echo compositor ;;
        files-app|rill-server) echo server ;;
        rill-vector)
            case $cmd in
                *--dock*)   echo dock ;;
                *--widget*) echo widget ;;
                *--app*)    echo app ;;
                *)          echo other_rill ;;
            esac ;;
        *) echo other_rill ;;
    esac
}

net_counters() {   # -> "rx tx" over loopback, where a local Rill's traffic is
    local rx=0 tx=0
    if ! $skip_network && [[ -r /sys/class/net/lo/statistics/rx_bytes ]]; then
        rx=$(cat /sys/class/net/lo/statistics/rx_bytes)
        tx=$(cat /sys/class/net/lo/statistics/tx_bytes)
    fi
    echo "$rx $tx"
}
swap_used_kib() {
    local t f
    t=$(meminfo SwapTotal); f=$(meminfo SwapFree)
    echo $(( ${t:-0} - ${f:-0} ))
}

# Copy the raw evidence for a named checkpoint. Full smaps only here, never
# in the 1 Hz loop.
checkpoint() {
    local name=$1
    local dir=$rundir/proc/$name
    mkdir -p "$dir"
    cp /proc/meminfo "$dir/meminfo" 2>/dev/null || true
    cp /proc/stat    "$dir/stat"    2>/dev/null || true
    cp /proc/vmstat  "$dir/vmstat"  2>/dev/null || true
    for f in /proc/pressure/*; do
        [[ -r $f ]] && cp "$f" "$dir/pressure.$(basename "$f")" 2>/dev/null
    done
    while read -r pid name2; do
        [[ -r /proc/$pid/status ]] || continue
        cp "/proc/$pid/status"        "$dir/$name2.$pid.status"        2>/dev/null || true
        cp "/proc/$pid/smaps_rollup"  "$dir/$name2.$pid.smaps_rollup"  2>/dev/null || true
        cp "/proc/$pid/smaps"         "$dir/$name2.$pid.smaps"         2>/dev/null || true
    done < <(own_processes)
}

# Sample one phase for N seconds at 1 Hz, writing both CSVs. Echoes the
# phase's aggregate: "mean_cpu peak_temp final_pss final_rss".
# (phase_pss/cpu/rss/watts are declared with the other summary inputs, above.)
sample_phase() {
    local phase=$1 seconds=$2 apps=${3:-0}
    local -a pids=() names=()
    local -A t0=()
    local pid name
    while read -r pid name; do
        pids+=("$pid"); names+=("$name"); t0[$pid]=$(cpu_ticks "$pid")
    done < <(own_processes)

    local tick; tick=$(getconf CLK_TCK)
    local cpu_sum=0 cpu_n=0 peak_temp= last_pss=0 last_rss=0
    local e_prev; e_prev=$(energy_uj)
    local pw_sum=0 pw_n=0
    local i began; began=$(mono)
    for (( i = 0; i < seconds; i++ )); do
        sleep 1
        local now; now=$(since "$start_mono")
        local elapsed; elapsed=$(since "$began")
        read -r pss rss swap <<<"$(stack_memory "${pids[@]}")"
        read -r rx tx <<<"$(net_counters)"
        local temp; temp=$(temperature_c)
        # Watts from the energy counter's delta over this one-second step.
        local pw=""
        local e_now; e_now=$(energy_uj)
        if [[ -n $e_now && -n ${e_prev:-} ]]; then
            pw=$(awk -v a="$e_prev" -v b="$e_now" 'BEGIN{
                d = b - a; if (d < 0) exit; printf "%.2f", d/1000000 }')
        fi
        e_prev=$e_now
        # Whole-stack CPU since the phase began, as percent of one core.
        local ticks=0
        for pid in "${pids[@]}"; do
            local t1; t1=$(cpu_ticks "$pid")
            ticks=$(( ticks + t1 - ${t0[$pid]:-0} ))
        done
        local cpu; cpu=$(awk -v d="$ticks" -v t="$tick" -v e="$elapsed" \
            'BEGIN{ if (e <= 0) print "0.00"; else printf "%.2f", (d/t)/e*100 }')
        printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
            "$now" "$phase" "$apps" "$pss" "$rss" "$swap" \
            "$(meminfo MemAvailable)" "$(swap_used_kib)" "$cpu" \
            "$rx" "$tx" "${temp:-}" "${pw:-}" >> "$samples_csv"

        # Per-process, same instant.
        local idx=0
        for pid in "${pids[@]}"; do
            name=${names[$idx]}; idx=$((idx + 1))
            [[ -r /proc/$pid/stat ]] || continue
            read -r ppss prss pswap <<<"$(proc_mem "$pid")"
            local pt; pt=$(cpu_ticks "$pid")
            local pcpu; pcpu=$(awk -v d="$(( pt - ${t0[$pid]:-0} ))" -v t="$tick" -v e="$elapsed" \
                'BEGIN{ if (e <= 0) print "0.00"; else printf "%.2f", (d/t)/e*100 }')
            printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
                "$now" "$phase" "$pid" "$name" "$(role_of "$pid" "$name")" \
                "$prss" "$ppss" "$pswap" "$pcpu" >> "$procs_csv"
        done

        cpu_sum=$(awk -v a="$cpu_sum" -v b="$cpu" 'BEGIN{print a+b}'); cpu_n=$((cpu_n + 1))
        [[ -n $temp && ( -z $peak_temp || $temp -gt $peak_temp ) ]] && peak_temp=$temp
        if [[ -n $pw ]]; then
            pw_sum=$(awk -v a="$pw_sum" -v b="$pw" 'BEGIN{print a+b}'); pw_n=$((pw_n + 1))
        fi
        last_pss=$pss; last_rss=$rss
    done
    local mean_cpu; mean_cpu=$(awk -v s="$cpu_sum" -v n="$cpu_n" \
        'BEGIN{ if (n == 0) print "0.00"; else printf "%.2f", s/n }')
    phase_pss[$phase]=$last_pss
    phase_rss[$phase]=$last_rss
    phase_cpu[$phase]=$mean_cpu
    phase_watts[$phase]=$(awk -v s="$pw_sum" -v n="$pw_n" \
        'BEGIN{ if (n == 0) print ""; else printf "%.2f", s/n }')
    echo "$mean_cpu ${peak_temp:-} $last_pss $last_rss"
}

# ------------------------------------------- phase 4: hermetic environment ---

bench_root=${TMPDIR:-/tmp}/rill-bench-$run_id
export RILL_DEMO_ROOT=$bench_root/demo
export RILL_DEMO_PORT=${RILL_BENCH_PORT:-7599}
export RILL_CACHE=$bench_root/cache
export XDG_CONFIG_HOME=$bench_root/config
export RILL_BENCH_PROFILE=$profile
# Rill-attributed wire bytes: the server snapshots its protocol-byte totals
# (post-TLS plaintext) into this file every 5 s. Distinct from the loopback
# interface totals, which include non-Rill traffic.
export RILL_STATS=$rundir/server-wire.json
mkdir -p "$XDG_CONFIG_HOME/rill" "$RILL_CACHE" "$bench_root/demo"
record hermetic "$($existing_session && echo false || echo true)"
record bench_root "$bench_root"
record port "$RILL_DEMO_PORT"

# The two workloads, written here so a run defines its own inputs rather than
# inheriting whatever theme the machine happened to have.
theme_idle() {
    cat > "$XDG_CONFIG_HOME/rill/theme.toml" <<EOF
# bench workload: idle-v1 — dock and wallpaper, nothing animating.
[colors]
page = "#0a0a0a"
EOF
}
theme_busy() {
    cat > "$XDG_CONFIG_HOME/rill/theme.toml" <<EOF
# bench workload: widgets-v1 — a meter and an ASCII widget on a clock.
[colors]
page = "#0a0a0a"

[desktop.ascii]
art = "cube"
seconds = $ascii_rate

[[desktop.widgets]]
app = "rill://127.0.0.1:$RILL_DEMO_PORT/meter"
anchor = "top-right"
width = 300
height = 160
x = 20
y = 20

[[desktop.widgets]]
app = "rill://127.0.0.1:$RILL_DEMO_PORT/ascii"
anchor = "bottom-left"
width = 380
height = 240
x = 20
y = 20
EOF
}

# The nested compositor's own display, discovered from its log. Without this
# an app window connects to whatever session the script was started from and
# opens on the host desktop — where it is neither measured nor, since it
# finds no rill_stream_manager_v1 there, even able to start.
nested_display() {
    local label=$1
    grep -o 'listening on WAYLAND_DISPLAY=[^ ]*' \
        "$rundir/logs/$label.compositor.log" 2>/dev/null | tail -1 | cut -d= -f2
}

launch_desktop() {   # $1 = log label
    local label=$1
    ( LD_LIBRARY_PATH="${RILL_LD_PATH:-$([[ -d /usr/lib64 ]] && echo /usr/lib64)}" "$bin/rill-compositor" \
        "$bin/rill-vector" --dock \
            --data "$bench_root/demo/data" \
            --identity "$bench_root/demo/identity-device" \
        > "$rundir/logs/$label.compositor.log" 2>&1 & echo $! > "$bench_root/desktop.pid" )
    sleep "$settle_seconds"
    pgrep -x rill-compositor >/dev/null 2>&1
}
stop_desktop() {
    [[ -f $bench_root/desktop.pid ]] && kill "$(cat "$bench_root/desktop.pid")" 2>/dev/null
    sleep 2
    for name in rill-vector rill-compositor; do
        for pid in $(pgrep -x "$name" 2>/dev/null || true); do
            tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF "$bench_root" \
                && kill "$pid" 2>/dev/null
        done
    done
    sleep 1
}

if ! $existing_session; then
    say "==> preparing hermetic stack (this builds and installs apps; not measured)"
    if ! "$repo/scripts/demo-desktop.sh" > "$rundir/logs/setup.log" 2>&1; then
        fail_phase setup "demo-desktop.sh failed — see logs/setup.log"
        exit 1
    fi
fi

# Nested runs need a host compositor to present into.
if [[ -z ${WAYLAND_DISPLAY:-} ]]; then
    sock=$(ls "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"/wayland-[0-9] 2>/dev/null | head -1)
    if [[ -n ${sock:-} ]]; then
        export WAYLAND_DISPLAY=$(basename "$sock")
        flag "nested in $WAYLAND_DISPLAY"
    fi
fi
[[ -n $resolution ]] && export RILL_BENCH_RESOLUTION=$resolution

# ------------------------------------------ phase 5: pre-Rill baseline ---

say "==> baseline (${idle_seconds}s, no Rill running)"
thermal_readings > "$rundir/sys/thermal_readings.txt" 2>/dev/null || true
for f in /sys/class/hwmon/hwmon*/power*_input; do
    [[ -r $f ]] && echo "$f = $(cat "$f")" >> "$rundir/sys/power_sensors.txt"
done
checkpoint system_baseline
read -r base_rx base_tx <<<"$(net_counters)"
baseline_mem_available=$(meminfo MemAvailable)
sample_phase system_baseline "$idle_seconds" 0 > /dev/null
record baseline_mem_available_kib "$baseline_mem_available"

# ------------------------------------------------ phase 6: idle desktop ---

cache_bytes() { [[ -d $RILL_CACHE ]] && du -sk "$RILL_CACHE" 2>/dev/null | cut -f1 || echo 0; }
cache_before_idle=$(cache_bytes)

say "==> idle desktop (settle ${settle_seconds}s, sample ${idle_seconds}s)"
theme_idle
if ! launch_desktop idle; then
    fail_phase idle "compositor did not stay up"
    exit 1
fi
checkpoint idle_settled
read -r idle_cpu idle_temp idle_pss idle_rss <<<"$(sample_phase idle "$idle_seconds" 0)"
idle_mem_available=$(meminfo MemAvailable)
idle_swap=$(swap_used_kib)
stop_desktop
# The compositor prints its lifetime frame count by cause as it exits, which
# is the only honest way to ask whether the damage gate held for a run.
frames_line=$(grep -o 'frames=[0-9]* heartbeat=[0-9]* damage=[0-9]* uptime=[0-9.]*s mean_fps=[0-9.]*' \
    "$rundir/logs/idle.compositor.log" 2>/dev/null | tail -1)
idle_frames=$(sed -n 's/.*frames=\([0-9]*\).*/\1/p' <<<"$frames_line")
idle_heartbeat=$(sed -n 's/.*heartbeat=\([0-9]*\).*/\1/p' <<<"$frames_line")
idle_damage=$(sed -n 's/.*damage=\([0-9]*\).*/\1/p' <<<"$frames_line")
idle_fps=$(sed -n 's/.*mean_fps=\([0-9.]*\).*/\1/p' <<<"$frames_line")
# Frame-time distribution. A mean hides the stall that makes a desktop feel
# broken: 30 fps with an occasional 120 ms frame reads as broken while 30 fps
# flat reads as fine, and both report 30.
idle_frame_ms=$(grep -o 'frame_ms mean=[0-9.]* p50=[0-9.]* p95=[0-9.]* p99=[0-9.]* max=[0-9.]*' \
    "$rundir/logs/idle.compositor.log" 2>/dev/null | tail -1)
cache_after_idle=$(cache_bytes)
# Checkpoint. `cleanup` writes the summary on every *handled* exit, but a
# SIGKILL — which is how the OOM killer ends things, and a 1 GB board running
# the scaling sweep is exactly where that happens — runs no trap at all. The
# first Pi run died that way at N=10 and left a bundle with three complete
# phases of CSV and no summary.json to name them. Writing after each phase
# costs a few milliseconds and means a killed run still reports everything it
# finished.
write_summary || true

# Which renderer the compositor actually bound. It prints this itself
# ("rill-compositor: wgpu on <name> (Vulkan, <type>, driver …)"), and it is
# the single most useful line in the bundle when the numbers look wrong:
# wgpu will happily pick a software Vulkan ICD, and a run on llvmpipe or
# lavapipe measures a CPU rasterizer while claiming to measure a GPU.
gpu_adapter=$(sed -n 's/^rill-compositor: wgpu on \(.*\)$/\1/p' \
    "$rundir/logs/idle.compositor.log" 2>/dev/null | tail -1)
record gpu_adapter "${gpu_adapter:-unknown}"
if grep -qiE 'llvmpipe|lavapipe|swiftshader|softpipe' <<<"${gpu_adapter:-}"; then
    software_renderer=true
    flag "SOFTWARE RENDERER ($gpu_adapter) — GPU-side numbers are not this machine's"
    say "!! software renderer: $gpu_adapter"
fi

# ------------------------------------------------ phase 7: busy desktop ---

if ! $skip_busy; then
    say "==> busy desktop (widgets-v1: meter ${meter_hz}Hz, ascii ${ascii_rate}s)"
    theme_busy
    if launch_desktop busy; then
        checkpoint busy_settled
        read -r busy_cpu busy_temp busy_pss _ <<<"$(sample_phase busy "$busy_seconds" 0)"
        busy_swap=$(swap_used_kib)
        stop_desktop
        busy_fps=$(sed -n 's/.*mean_fps=\([0-9.]*\).*/\1/p' \
            <<<"$(grep -o 'mean_fps=[0-9.]*' "$rundir/logs/busy.compositor.log" | tail -1)")
        busy_frame_ms=$(grep -o 'frame_ms mean=[0-9.]* p50=[0-9.]* p95=[0-9.]* p99=[0-9.]* max=[0-9.]*' \
            "$rundir/logs/busy.compositor.log" 2>/dev/null | tail -1)
    else
        fail_phase busy "compositor did not stay up"
    fi
fi
# The reading that matters: whether sustained load made the board throttle.
read_throttled after_load

write_summary || true   # checkpoint after busy (see the note after idle)

# --------------------------------------------- phase 8+9: scaling, recovery ---

if ! $skip_scale; then
    say "==> scaling ($scale_list apps)"
    theme_idle
    if launch_desktop scale; then
        nested=$(nested_display scale)
        if [[ -z $nested ]]; then
            fail_phase scale "could not find the compositor's wayland socket"
        fi
        record nested_display "${nested:-unknown}"
        read -r scale_baseline _ _ <<<"$(stack_memory $(own_processes | cut -d' ' -f1))"
        prev=$scale_baseline
        # App keys are derived from the server fingerprint, so they differ per
        # bench root and must be discovered rather than assumed. Terminals are
        # skipped: each one forks a shell, which is a different measurement.
        mapfile -t keys < <("$bin/rill" app list --data "$bench_root/demo/data" 2>/dev/null \
            | awk '{print $1}' | grep -v '^term' | grep -v '^$')
        if [[ ${#keys[@]} -eq 0 ]]; then
            fail_phase scale "no installed apps to open"
        else
            opened=0
            IFS=',' read -ra targets <<<"$scale_list"
            for target in "${targets[@]}"; do
                while (( opened < target )); do
                    key=${keys[$(( opened % ${#keys[@]} ))]}
                    ( LD_LIBRARY_PATH="${RILL_LD_PATH:-$([[ -d /usr/lib64 ]] && echo /usr/lib64)}" \
                      WAYLAND_DISPLAY="$nested" \
                      "$bin/rill-vector" --app "$key" \
                        --data "$bench_root/demo/data" \
                        --identity "$bench_root/demo/identity-device" \
                        --cache "$RILL_CACHE" \
                        >> "$rundir/logs/scale.apps.log" 2>&1 & )
                    opened=$((opened + 1))
                    sleep 1
                done
                sleep "$settle_seconds"
                # Did they all actually come up? A launch that failed is data.
                live=$(own_processes | while read -r pid name; do
                        [[ $(role_of "$pid" "$name") == app ]] && echo x; done | wc -l)
                if (( live < target )); then
                    scale_limit="\"only $live of $target app windows stayed up\""
                fi
                checkpoint "scale_${target}"
                read -r cpu _ pss _ <<<"$(sample_phase "scale_$target" "$settle_seconds" "$target")"
                comp_pid=$(own_processes | awk '$2=="rill-compositor"{print $1; exit}')
                read -r comp_pss _ _ <<<"$(proc_mem "${comp_pid:-0}")"
                mean_client=$(awk -v p="$pss" -v c="$comp_pss" -v n="$live" \
                    'BEGIN{ if (n<=0) print 0; else printf "%d", (p-c)/n }')
                printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
                    "$target" "$pss" "$((pss - scale_baseline))" "$((pss - prev))" \
                    "$mean_client" "$comp_pss" "$(meminfo MemAvailable)" \
                    "$(swap_used_kib)" "$cpu" >> "$scaling_csv"
                prev=$pss
                scale_reached=$live
                [[ $scale_limit != null ]] && break
            done

            # Recovery: close every app window, settle, re-measure the desktop.
            say "==> close and recover"
            for pid in $(own_processes | while read -r p n; do
                    [[ $(role_of "$p" "$n") == app ]] && echo "$p"; done); do
                kill "$pid" 2>/dev/null || true
            done
            sleep "$settle_seconds"
            checkpoint post_close
            read -r _ _ post_pss _ <<<"$(sample_phase post_close "$settle_seconds" 0)"
            post_close_delta=$((post_pss - scale_baseline))
        fi
        stop_desktop
    else
        fail_phase scale "compositor did not stay up"
    fi
fi

read -r end_rx end_tx <<<"$(net_counters)"
cache_after=$(cache_bytes)
status=complete

write_summary
cat "$rundir/summary.txt"
