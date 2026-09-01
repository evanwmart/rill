# Pi soak — a week of nothing happening

Status: **protocol, no run yet.** Written 2026-08-17. Results get appended
here as dated entries, memory-footprint.md style; the protocol above the
first entry is frozen once a run starts, so entries stay comparable.

## Why this document exists

Success-ladder rung 2 (risks.md): *"Pi + server runs continuously for a
week."* Risk #4 is blunt about why: a 2 GB Chrome box that runs 180 days
beats a brilliant 120 MB platform that occasionally blanks, and the boring
suite must pass with *nothing happening* before anyone else is asked to
run this. The cache-growth bug fixed on 2026-08-13 (0.38 MiB/min ≈ 547
MiB/day) is exactly the class of defect only a soak finds — every bench
run before it was 30–60 seconds and saw nothing.

A soak costs zero dev hours. It is the Pi sitting there, plus a sampler
appending one CSV line every five minutes so that day 7 produces data
rather than "it didn't crash."

## What a soak tests, and does not

Tests: crashes, OOM, memory drift (leaks), cache growth, fd leaks, swap
creep, thermal behavior, whether the damage gate still holds after days,
whether the session is still *responsive* (not merely alive).

Does not test: performance (nested-in-labwc caveats apply as always),
power (no board sensor), boot time (milestone 15), bare metal. A soak
entry never upgrades a perf claim; it can only establish endurance.

## Two overlapping soaks — start the first one tonight

**Soak A — server only. Needs no display; start immediately.** Rung 2
names the *server*, and `files-app` runs headless. Launch it exactly the
way `demo-desktop.sh` does (its `==> starting files-app` line shows the
invocation), detached (`setsid`/`nohup` — see the shell-backgrounding
notes in the build-environment memory; bind 127.0.0.1, not localhost).
Point the sampler at it.

**Soak B — full desktop, nested in labwc.** Joins Soak A once the
headless display question (below) is settled. Workload decision, recorded
here so the entry is interpretable: **dock + one meter widget at 1 Hz** —
busy enough to exercise the whole fetch→compile→decode→layout→composite
pipeline continuously, calm enough that the run still speaks to the
battery/fanless story. Not pure idle (tests too little), not the 12 Hz
video workload (chosen for looks, not endurance).

## Headless display without the TV (software first)

The compositor nests in labwc, and labwc needs an output. No EDID means
no session — but the plug can be faked in software, and the $7 dummy
plug is only the fallback if this proves flaky.

```bash
# 1. Force an enabled HDMI output with no EDID. cmdline.txt is ONE line —
#    append, never add a second line.
sudo sed -i 's/$/ video=HDMI-A-1:1280x800M@60D/' /boot/firmware/cmdline.txt
sudo reboot
# (If the "D" flag doesn't take on this kernel, the second software lever
#  is vc4.force_hotplug=1 on the same line. Hardware dummy plug is third.)

# 2. Verify the session actually exists:
loginctl list-sessions                    # a Type=wayland session
ls /run/user/$(id -u)/wayland-*           # a socket
pgrep -a labwc

# 3. Eyes without the TV: raspi-config → Interface Options → VNC (wayvnc),
#    then peek from the workstation whenever curiosity strikes.
```

Acceptance: session up, `rill-compositor` prints the **V3D adapter line**
(a llvmpipe/lavapipe line invalidates the run — same rule as
bench-device.sh), and a short smoke run behaves normally. With those
three, forced mode is indistinguishable from a plug for what a soak
measures: the CRTC still scans out, vsync still paces present, the GPU
neither knows nor cares that nothing is listening.

**Do not use `WLR_BACKENDS=headless` for this.** It would work, but the
wlroots headless output is timer-paced rather than display-paced, which
changes present timing and adds a caveat the forced-DRM path avoids.
Keep the run comparable to the 2026-08-15 bundles.

## The sampler

One CSV line every five minutes. `%cpu` from `ps` is a lifetime average —
useful for drift, useless for spikes; the load average and the
compositor's own exit-time frame report cover the rest. Run it under
`setsid`, same as the server.

```bash
#!/usr/bin/env bash
# soak-sample.sh — append one line every 5 min. Promote into scripts/ if kept.
out=~/rill-soak-$(date +%Y%m%d).csv
echo "ts,pss_kib_by_proc,mem_avail_kib,swap_used_kib,temp,throttled,cache_kib,load1" >> "$out"
while true; do
  pss=$(for p in $(pgrep -f 'rill-compositor|rill-vector|files-app|rill-server'); do
          printf '%s:%s ' "$(cat /proc/$p/comm)" \
            "$(awk '/^Pss:/{print $2}' /proc/$p/smaps_rollup 2>/dev/null)"
        done)
  mem=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
  swp=$(awk '/SwapTotal/{t=$2}/SwapFree/{f=$2}END{print t-f}' /proc/meminfo)
  tmp=$(vcgencmd measure_temp 2>/dev/null | tr -d "temp='C")
  thr=$(vcgencmd get_throttled 2>/dev/null | cut -d= -f2)
  cch=$(du -sk ~/.local/share/rill-demo/content 2>/dev/null | cut -f1)
  l1=$(cut -d' ' -f1 /proc/loadavg)
  echo "$(date -Is),\"$pss\",$mem,$swp,$tmp,$thr,$cch,$l1" >> "$out"
  sleep 300
done
```

## Pass / fail, stated before the run so it cannot drift

A run **passes** when, over 7 days:

* no process crash, restart, or OOM kill (check `dmesg`/journal at exit);
* per-process PSS drift is bounded — flat or oscillating is fine; a
  monotonic upward slope of any size is a finding, not a pass;
* cache growth is 0 MiB (the 64 MiB budget sweeper holding for a week,
  not thirty seconds);
* `throttled=0x0` at exit, and temperature never sustained above the
  low 60s °C seen in the bench runs;
* the session is still *responsive* at day 7 — a VNC interaction, not
  just a live pid;
* (Soak B) the compositor's exit report still shows heartbeat-dominated
  idle frames — the damage gate holding after a week, not a minute.

A partial run is kept and recorded with why it stopped — same rule as
bench-device.sh. A failed run is a *successful soak*: it found the thing
before a stranger did.

## Known confounds to note in every entry

The 1 GB board runs the Pi OS desktop session underneath and starts
110–138 MiB into swap (memory-footprint.md 2026-08-15) — host swap
motion is not Rill drift; the per-process PSS columns are the honest
signal, the whole-box numbers are weather. SIGTERM at collection time
loses BufWriter tails (recorder caveat); stop processes gently if a
recording is running.

---

*Entries append below, dated.*

## 2026-08-24 — the run is live (launch log + 2-hour checkpoint)

**Status: running.** Both soaks started 18:36 PT on the reference Pi 5 1 GB
(Debian 13, kernel 6.18.34, V3DV Mesa 25.0.7), binaries cross-built the
same day at the pinned 1.98.0 toolchain, `--locked`. Soak A = files-app on
127.0.0.1:7420 against the bench-era demo tree; Soak B = compositor + dock
+ one meter widget at 1 Hz (the frozen workload), nested in labwc on the
forced 1280x800 output. wayvnc on :5900 for eyes. The sampler is the
promoted `scripts/soak-sample.sh` — protocol snippet plus pids, fd counts,
and a `history_kib` column (the always-on recorder postdates the protocol;
its growth is expected, unlike the cache's, so the two claims get separate
columns).

**Deviations from a pristine protocol, recorded up front:**

* The always-on history recorder is live and UNENCRYPTED — the device
  identity lives in the demo tree, not `~/.config/rill`, so the recorder
  fell back. Fine for a soak; would not be fine for a user.
* The run may be read at ~5 days rather than 7 (travel). A 5-day entry is
  a partial run under the protocol's own rule, and rung 2's full week
  would then be the next run.
* No audio tap (no parec on the Pi) — irrelevant to this workload.

### Launch found two environment failures before the soak could start

Neither is a Rill defect; both are exactly what the appliance image must
pin down, and one exposed a real product bug.

1. **Cross-build failed twice.** The container baked rustc 1.94.0 and the
   repo now pins 1.98.0 — every crate failed until the image followed the
   pin (the toolchain version is in the image tag now, so a pin bump
   rebuilds instead of silently compiling on the old compiler). Then
   alsa-sys: the music app's ALSA dependency postdates the container's
   package list; `libasound2-dev:arm64` added.
2. **The desktop could not start: `wgpu error: Out of Memory` at
   `Surface::configure`.** Chain, established by measurement: no display
   attached and no forced mode (the bench-era runs had a physical TV; the
   `video=` cmdline trick was never actually applied) → the display stack
   has no mode → CMA starves (`CmaFree: 0` vs the bench bundles' healthy
   30,960 KiB at boot) → V3DV cannot allocate a swapchain → wgpu panics
   three layers below the cause, taking the dock down with it
   (`ConnectionReset` panic in rill-vector). Fixed by applying the
   protocol's own step 1 (`video=HDMI-A-1:1280x800M@60D`) and rebooting:
   CmaFree 33,888 KiB, connector `connected`, desktop up. **The panic is
   ours to fix** — a kiosk's screen gets unplugged mid-run, and "degrade
   and wait" is a product requirement; filed in TODO.md.

### 2-hour checkpoint (26 samples): PASS on every machine criterion

```text
pids            1453/1470/1479/1484 — all original, zero restarts
PSS             compositor 35.1 → 28.4 MiB by 0:30, then the IDENTICAL
                value (28,407 KiB) at three consecutive half-hour marks;
                files-app 6.29→6.36 MiB; dock 5.0; meter 6.1 (±16 KiB)
fds             8/30/9/10 — identical all run
cache           132 KiB, unmoved (the 64 MiB sweeper holding)
history         ~2.1 MiB/h and linear to the kilobyte — a steady writer
temp/throttle   46–49 °C, throttled=0x0
crashes/OOM     none (logs and dmesg both)
swap            spiked to 90 MiB in the first half hour, then DECLINED
                to 88.7 — equilibrium, not creep
responsive      verified by VNC poke (human criterion)
```

**The compositor's PSS *decrease* is reclaim, not frugality** — measured,
because "less resident is not less used" cuts both ways: `VmSwap` grew 0 →
4,512 KiB (cold startup pages — pipeline-compile scratch — evicted under
the host session's pressure) and clean file pages of the 20 MB binary were
dropped. `VmHWM` 73.9 MiB against 39.7 MiB resident says startup nearly
doubles the footprint and the steady state never touches it again. The
honest sentence: *the hot working set of a composited 1 Hz desktop is
~28 MiB PSS, and the kernel found ~11 MiB it never needs resident.* Claim
"reclaimed", never "freed".

### 2026-08-25, hour 18 — the soak earns its keep: seal-time memory staircase

**Finding (the first real one): each history segment seal permanently
grew the compositor's resident memory by tens of MiB.** The curve is a
step function, not a slope — dead flat at ~28.5 MiB for eight hours, then:

```text
seal #1  03:08  →  PSS  28.5 → 104.9 MiB   (+76)
(kernel claws back cold pages: 104.9 → 84.5, then flat five hours)
seal #2  11:41  →  PSS  84.5 → 130.7 MiB   (+46)
```

Correlation is exact: the compositor log's `history sealed` lines match
the steps to the minute. Everything else stayed clean at hour 18 — all
original pids, fds frozen, cache at 132 KiB unmoved, history file growth
linear, 47.7 °C, no OOM — so this is one defect, isolated, on a run that
was otherwise passing. At one seal per ~8.5 h and ~50 MiB retained each,
a 1 GB board meets the OOM killer around day 3–4. *A failed run is a
successful soak: it found the thing before a stranger did.*

**Root cause** (read from the source, run untouched): `seal_path_with`
decoded the *entire segment's events into memory at once* — the whole
file read in, then every chunk decompressed and every event accumulated
into one Vec, then per-tier indexes built over it. Rust frees it all at
function end, but glibc keeps the transient peak as arena high-water, so
each seal's O(segment-decoded) spike became retained RSS. The partial
dip between seals is the kernel swapping the cold arenas out — reclaim,
not recovery.

**Fix (same day, workstation-side): the seal now streams.**
`scan_chunks` was refactored into `walk_chunks` — chunks decode one at a
time into a sink and drop immediately — and sealing feeds incremental
accumulators: running span, tier set, event count, and a new
`index::Builder` that builds each tier's index one event at a time.
`index::build` is now that Builder driven in a loop, so batch and stream
cannot drift, and the existing stored-equals-rebuilt seal test pins the
equivalence. The one subtlety (batch decided the frame-text fallback by
looking at the whole segment upfront; a stream cannot look ahead) is
handled by buffering deduplicated frame text and discarding it the
moment a `Text` event appears — with a regression test for the case
where a frame precedes an identical `Text`. `malloc_trim(0)` after each
seal returns the remaining O(file) transient to the OS (gnu-linux only,
cfg-gated).

**Run disposition: left running on the OLD binaries, deliberately.**
Seal #3 (~20:10) confirms the accumulation trajectory for this entry; if
the run later OOMs while unattended, the sampler records the death to
the five-minute mark and the partial run is kept under the protocol's
own rule. The streamed-seal binaries deploy at the next natural restart,
and the re-run becomes the fix's verification.

### 2026-08-26, hour 40 — correction: the staircase is bounded

Seals #3 (20:14) and #4 (04:49) landed on the old binaries and the
projection above was **wrong in the direction that matters**: they cost
+43.5 and +0.7 MiB respectively against arena the kernel had reclaimed,
and the compositor now oscillates 89–131 MiB around a high-water mark
instead of accumulating without bound. glibc reuses the arena once it is
warm; the day-3–4 OOM forecast is withdrawn, and the run is expected to
survive the week.

What stands unchanged: four seals every ~8.5 h to the minute, all pids
original at hour 40, fds frozen, cache unmoved at 132 KiB, history file
growth linear (~97 MiB across five segments), 47.7 °C, throttled=0x0,
zero OOM. And the fix stands too — a ~130 MiB permanent hoard against a
28 MiB working set is still the difference between "fits on 512 MB
boards" and "needs a gigabyte" — but its label corrects from
*prevents an OOM* to *removes a 4× memory tax*. Projections earn their
labels; this one is why entries record them.

### 2026-08-31, hour 165 — verdict: PASS, with the staircase as its finding

**Read at 15:27–15:38 PT, 3.1 hours before the 168-hour mark, by
decision** — six consecutive flat daily maxima made the final three hours
non-informative. A 165-hour run is recorded as a 165-hour run; the
unqualified "a week" belongs to the verification run below.

All MEASURED, from 1,979 five-minute samples plus the exit reports:

* **Zero crashes, restarts, OOM kills.** All four pids original at read
  (1453/1470/1479/1484); dmesg and journal both clean. Uptime from the
  compositor's own exit report: 594,279.7 s = 6 d 21 h.
* **PSS plateau, not slope.** Compositor daily maxima: 35.1 MiB (launch
  day) → 130.7 (day 1, the staircase) → then 128.6–129.5 MiB for six
  straight days — a 0.9 MiB band, and the all-time high-water mark was
  six days old at read. files-app 6.3 → 5.8 MiB; dock and widget flat.
* **fds frozen** at 8/30/9/10 the entire run. **Cache 132 KiB, unmoved**
  — the 64 MiB sweeper held for a week, not thirty seconds.
* **History linear:** 20 seals on the ~8.5 h cadence to the minute,
  ~404 MiB total, and the shutdown path sealed the final segment cleanly
  (SIGTERM exit wrote `1788205555947.rhs` before the report — the
  BufWriter-tail caveat did not bite).
* **Damage gate held for a week:** frames=953,266 over 594,280 s =
  mean 1.60 fps against a 60 fps budget; damage frames 570,035 = 0.96/s,
  exactly the 1 Hz meter; frames_per_commit=1.00. The "heartbeat-
  dominated" criterion was written for idle — with a 1 Hz workload the
  equivalent statement is *frames tracked the workload, not the budget*,
  and they did. frame_ms mean 6.88, p95 9.0, p99 12.25, max 1125.2
  (isolated spikes; seal + swap-in moments).
* **Thermal:** 45–50 °C all week, throttled=0x0 at every sample.
* **Server:** the widget's TLS connection lived the full 6.9 days,
  33.9 MB in / 363.7 MB out, and closed only at our SIGTERM.

**Deviations recorded:** the day-7 VNC responsiveness poke was not
re-performed at read (the read was scripted; last human poke was mid-run)
— the 1 Hz meter updating in every sample is the machine's version of the
same evidence. And the compositor's plateau sits at ~129 MiB instead of
~28 because of the seal staircase — a real finding already root-caused,
fixed in-tree, and NOT deployed to this run on purpose. Verdict against
the protocol's letter: every machine criterion passes; the staircase is
the run's finding, not its failure.

Raw CSV archived at `bench-results/2026-08-24_soak/` (workstation).

### 2026-08-31, 15:41 PT — verification run live: same protocol, stream-seal binaries

The fix's re-run, started the same afternoon. Same board, same workload
(files-app + compositor + dock + one 1 Hz meter, nested in labwc on the
forced 1280x800 output), same sampler (`rill-soak-20260831.csv`).
Binaries cross-built at the pinned 1.98.0 from `b32e63a` — which carries
the streamed seal (`walk_chunks` + incremental `index::Builder` +
`malloc_trim(0)`) — deployed 15:40, V3D adapter line confirmed at launch.

**What this run exists to answer:** seal cost. Old binaries: +76 MiB
first seal, +43.5 warm, ~131 MiB permanent plateau. PROJECTED for these
binaries: seals cost O(chunk) transient, plateau ~35–45 MiB. The prior
run's history is retained (~404 MiB, 20 sealed segments), so seal #1
lands on the usual ~8.5 h cadence tonight — the first datum.

**Launch notes, for honest reading of the first samples:** first sample
caught the compositor mid-startup at 52.9 MiB PSS (the old run sampled
35.1 at launch and reclaimed to 28.4 by 0:30 — watch for the same
settle). And the meter widget is spawned *by the dock* on a `--widget`
hand-off; the detached CLI process that requested it lingered ~60 s
before being killed, so the first sample line carries one extra
`rill-vector` and a stray `bash` — launch noise, not a crash record.
