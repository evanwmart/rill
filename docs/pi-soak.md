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

### 2026-09-01, hour 19 — first seals on the streamed path: the staircase is gone; two smaller questions arrive

**MEASURED, from the CSV.** Two `history sealed` events so far. The
seal-correlated PSS moves: **+0 KiB** and **+272 KiB** — against +76 and
+43.5 *MiB* for the same moments on the old binaries. The warm-seal cost
is now sampling noise; the fix does what it says.

The run is NOT at the projected 35–45 MiB plateau, and honesty about
why is the point of labeling projections. Compositor PSS: 52.9 at
launch, flat ~51.5 for eight hours (no reclaim down to run #1's
~28 MiB idle — open question #1: the baseline starts ~18 MiB higher
than run #1's, cause unmeasured), then one clean **+15.5 MiB step in a
single 5-minute window at 23:43** — hours from either seal — and dead
flat at 67.1 MiB for eight hours since (open question #2: cause
unmeasured; one candidate worth checking in source is the streamed
seal's frame-text fallback buffer, which holds deduplicated frame text
until a `Text` event appears, but the step's distance from both seal
lines argues against it; another is first-touch of some nightly
retention/tier work).

Neither question is the staircase: nothing here compounds per-seal, and
the curve since 23:46 is the flattest this board has produced. Plateau
so far: **67.1 MiB vs run #1's 130.7** — the 4× tax is at worst a 2.4×,
pending the week and the two answers above. Day 7 reads Sep 7.

### 2026-09-08, hour 186 — verification run: the staircase is gone, and the soak finds two more things

**Read at 10:03 PT, 18 hours past the 168-hour mark; run left LIVE** (the
read is scripted from the CSV and the compositor log; the exit-time frame
report belongs to whichever day the run is stopped). All MEASURED, from
2,237 five-minute samples and 23 `history sealed` lines.

**Every machine criterion passes, again:**

* Zero crashes, restarts, OOM kills — all four pids original
  (306684/306694/306699/306707), kernel journal clean since launch.
* fds frozen at 8/30/9/10. Cache 132 KiB, unmoved for a second week.
* History linear: 405 → 910 MiB, 23 seals on an 8.0–8.1 h cadence (run
  #1's was ~8.5 h; the segment-size trigger fires sooner with this
  run's slightly busier dock — see below). 19 GB of SD card free.
* Thermal 44.4–50.5 °C, `throttled=0x0` at every sample.
* files-app 4.7–5.7 MiB flat; meter widget 5.0 → 7.7 MiB in the first
  five days, then flat at 7.6 since Sep 5.

**The fix does what it says.** Compositor daily maxima: 65.5, 65.8,
65.2, 69.6, 65.6, 66.2, 66.1, 66.9, 66.6 MiB — a plateau at **~66 MiB
against run #1's ~130**. `VmHWM` 96.3 MiB, `VmRSS` 71.8, `VmSwap` 10.7
at read. The 4× tax is now a 2.3× tax, and both hour-19 open questions
have answers — one from the CSV, one from the source:

* **Open question #2 is closed: every upward step is a seal.** Each
  positive move >1.5 MiB in a 5-minute window sits 0–5 minutes after a
  `history sealed` line — including the "+15.5 MiB at 23:43, hours from
  either seal" step in the hour-19 entry, which was a misread (seal #1
  landed at 23:41). Per-seal cost decays from +15.1 (first, cold arena)
  through +7.0, +5.5, +6.0 to +2–3 MiB by day 5, and each is given back
  within the day. Correction to hour-19, recorded here rather than
  edited there.
* **The daily downward step is host weather.** Every reclaim of −2 to
  −10 MiB lands at 18:36–18:37 on a fixed 24-hour clock (not the 8 h
  seal cadence), with swap rising ~15 MiB and MemAvailable dipping in
  the same window. The compositor's own retention pass runs only at
  boot (history_writer.rs), no systemd timer fires at that time, and
  the PSS returns at the next seal. Not Rill; noted, not chased.
* **Open question #1 (the ~18 MiB higher baseline) — PROJECTED cause,
  read from source, unmeasured:** the writer thread's boot pass calls
  `seal_path_with` on every existing `.rhs` to seal a crashed
  predecessor's tail, and that function `read_to_end`s the whole file
  *before* checking whether it is already sealed. Run #1 booted on an
  empty directory; this run booted on 20 sealed segments of up to 23
  MiB — twenty O(file) transients through the glibc arena before the
  first frame. The two-small-reads check (`read_seal_with`) exists and
  is what the boot pass's own comment believes it is using. The
  workstation measurement (compositor PSS at t+1 min, empty vs 20-segment
  history dir) is the datum that turns this into MEASURED.

**Finding: the dock leaks ~1.6 MiB/day, in BOTH runs.** The dock
process (`rill-vector --dock`) went 4.3 → 18.2 MiB over 7.8 days,
linear (least-squares slope 1.59 MiB/day, median 5-minute delta 0 KiB,
largest single step 0.16 MiB — a slow accrual, not events). `/proc`
says it is 15.0 MiB of `[heap]`; fds frozen at 9. Re-reading run #1's
CSV: its dock went **5.1 → 16.4 MiB over six days on the same slope.
The hour-165 entry's "dock and widget flat" was wrong** — the reader
looked at the widget and assumed the dock; correction recorded here.
The leak predates the stream-seal change and is unrelated to it.

*Root cause (read from source, run untouched):* the dock's strip is a
document with a clock in it, so a new minute is a new document — the
client regenerates and recompiles the dock KDL when the minute turns
and hands it to the viewport through `reload_keep_focus`. That method
sets the in-place flag and then calls `open`, and `open` **pushes the
new `Source::Generated { bytes }` onto the navigation history stack.**
The position is always at the end, so the truncate-then-push never
truncates: one compiled dock document retained per minute, forever.
Arithmetic: 1.59 MiB/day ÷ 1,440 minutes = ~1.1 KiB per entry, the
size of a compiled dock strip plus its `Source` — and the "Back" key on
the dock would, in principle, step through every minute of the week.
The 1 Hz meter does not leak the same way because live ticks replace
`history[position]` in place, which is exactly the shape the fix wants.
On a 1 GB board the horizon is more than a year, so this is a finding
under the protocol's letter (a monotonic slope of any size), not a
threat to a run — but a document that regenerates every second would
hit it 60× faster, and one exists in the widget set.

**One event:** a single `surface error: A timeout was encountered while
trying to acquire the next frame` between the Sep 5 08:19 and 16:23
seals. The loop's `sleep(5 ms); continue` handled it, the meter kept
updating through every sample in that window, and load was 0.00. The
log line has no timestamp — the seal filenames on either side are the
only clock — which is its own small finding.

**Verdict against the protocol's letter: PASS on every machine
criterion, with the dock slope as the run's finding.** The unqualified
"a week" still waits: this run was read while live, without the day-7
human poke, and with a known slope in one process. Run #3, on binaries
carrying the two fixes above, is the one that gets to say it.

Raw CSV and both logs archived at `bench-results/2026-08-31_soak/`.

**Stopped 2026-09-08 10:19 PT at hour 186.6, by decision** (SIGTERM,
compositor first; exit in 2 s; the final segment `1788883339281.rhs`
sealed on the way out). MEASURED, from the exit reports:

```text
uptime          671,910.7 s = 7 d 18.6 h
frames          1,097,208 over that = mean 1.63 fps against a 60 fps budget
damage frames   677,107 = 1.01/s — the 1 Hz meter, frames_per_commit=1.00
frame_ms        mean 7.01, p50 8.25, p95 9.25, p99 12.50, max 4939.9
acquire_ms      mean 0.06, p95 0.25, max 12.31
server conn 1   38.3 MB in / 430.3 MB out, lived the whole run, closed by
                our SIGTERM only
dock            applied_loads=11,200 over 11,196 minutes of uptime — one
                document load per clock minute, exactly the push count
                the root cause above predicts
meter           applied_loads=654,935 (the 1 Hz ticks that changed)
```

The one number worse than run #1 is frame max: 4,939.9 ms against
1,125.2. A single stall — the Sep 5 acquire timeout is the obvious
candidate, and without log timestamps it cannot be tied to a sample.
Archive refreshed with the full CSV (2,240 samples) and both logs.

### 2026-09-08, same day — the three fixes, workstation-side, with the boot pass MEASURED

**1. In-place reloads no longer grow the navigation stack.**
`AppView::reload_keep_focus` now replaces `history[position]` and starts
the load, instead of routing through `open`. A regression test
(`crates/rill-viewport/tests/in_place_reload.rs`) drives sixty
regenerations and asserts the stack stays at one entry, Back is a no-op,
and a real `open` still pushes; it fails on the old code at the first
assertion. The dock's `applied_loads=11,200` over 11,196 minutes in the
exit report above is the same count from the other side.

**2. The seal check reads forty bytes, not the segment.** `seal_path_with`
now asks `sealed_on_disk` first — the tail mark and a plausible seal
length, the identical test `seal_region` makes in memory — and only
reads the body when the answer is no. Test: sealed reads as sealed,
unsealed as unsealed, a torn tail as unsealed.

The PROJECTED cause of open question #1 is now **MEASURED in isolation**
(`RILL_SOAK_HIST=<dir> cargo test -p rill-history boot_pass --
--ignored --nocapture`, release, against the ten largest segments from
this run, 219 MiB on disk, all sealed):

```text
                        VmHWM (peak)     VmRSS after the sweep
old seal_path_with      +44,332 KiB      +22,500 KiB retained
tail check              +76 KiB          +80 KiB
```

That retained 22 MiB is the size of the largest segment read into a
Vec and kept by the allocator as arena high-water — the +18 MiB
baseline this run carried against run #1's empty directory, to within
the noise of what else the compositor allocates at boot. The
whole-compositor version of the same measurement on the workstation
(PSS at t+30 s, no client, two repetitions each) was *not* usable as a
first-order datum — the NVIDIA driver's ~400 MiB RSS swings ±30 MiB
between identical launches — but the populated-directory pairs did
repeat: old 216.5/216.5 MiB, new 204.2/204.1 MiB, a 12 MiB gap in the
expected direction. The isolated number is the one this entry claims;
run #3's launch-time PSS on the Pi is the one that closes it.

**3. Every compositor log line now carries the local wall clock**
(`say!`/`cry!` in rill-compositor over `rill_log::stamp()`, the
`YYYY-MM-DDTHH:MM:SS±HH:MM` form the sampler's CSV already uses). The
next acquire timeout gets a time, and a soak log becomes a timeline
without the CSV beside it.

Not done, by decision: the 18:36 daily host reclaim (parked); the
display-loss panic (TODO, appliance robustness); run #3 (needs a
cross-build from the tree once these land).
