# Memory footprint — measured attribution

Status: **measurement log**, not a spec. First entry 2026-08-08. Re-measure
after release builds / driver changes and append; don't rewrite history.

## Why this document exists

Naive `ps` RSS for the demo stack reads ~300–350MB per big process, which
sounds heavy. Attribution via `/proc/<pid>/smaps` shows most of that is the
GPU driver's userspace, not Rill — and the per-app marginal cost is single-
digit MB. The headline numbers are misleading without this breakdown; quote
the attributed ones.

## 2026-08-08 — full demo stack, debug builds, NVIDIA proprietary driver

Setup: `rill-compositor` + `rill-shell` + two `rill-vector` clients
(dashboard + one app). **Debug builds** (binaries and heap are inflated vs
release). NVIDIA proprietary Vulkan ICD.

Whole-stack PSS (shared pages counted proportionally): **~565MB** for the
entire desktop — compositor, shell, and both apps.

### rill-compositor — 292MB RSS

| MB  | What                                                              |
|-----|-------------------------------------------------------------------|
| 138 | `/dev/nvidiactl` mappings — driver GPU working set (swapchain, command buffers, arenas) |
| ~72 | NVIDIA userspace libs (`gpucomp`, `glcore`, `rtcore`, `glvkspirv`, `GLX`) |
| 22  | libLLVM — Mesa shader compiler, pulled in by ICD enumeration       |
| ~6  | Mesa ICDs (`libvulkan_radeon`, `lavapipe`) — loaded but unused     |
| 31  | anonymous/heap — **Rill's actual working data** (wgpu arenas, glyph atlas, scene) |
| 14  | the binary itself (debug; release will be a fraction)              |

Rill-attributable: **~45MB of 292MB**. The rest is driver + shader
compilers, mostly shared clean file pages.

### rill-shell — 347MB RSS

Same shape: ~200MB NVIDIA (including 56MB `libnvidia-rtcore` and 15MB
`libcuda`, both unused) + 22MB LLVM, vs **85MB heap** (gpui layout/text) and
a 17MB debug binary.

### rill-vector clients — 9MB and 13MB RSS

Only **4–8MB PSS each** once shared libraries are counted fairly. This is
the flat-scaling property measured, not estimated: each additional app costs
single-digit MB; the compositor/driver fixed cost is paid once.

## Implications — stated carefully

* ~70–75% of the two big processes' RSS is NVIDIA driver/library mappings.
  On Mesa hardware (AMD/Intel iGPU, Pi), RADV/ANV userspace is tens of MB
  and ACO removes the LLVM dependency — expect the driver bill to shrink
  substantially. (Projection, not measurement.)
* **Do not say "Rill is ~45MB."** That figure is the *compositor's* directly
  attributable working data + debug binary. rill-shell separately carries
  ~85MB heap + 17MB binary, and the whole measured stack is ~565MB PSS.
  The defensible sentence: *"the compositor's own working data was ~45MB in
  this run; most of its much larger RSS is graphics-driver mappings, and
  each additional app cost 4–8MB PSS."*
* Shared driver/libraries are real dependencies of the running stack — they
  count against hardware requirements even though Rill didn't allocate them.
* The architectural result to defend is the **slope, not the intercept**:
  memory ≈ fixed platform cost + N × small per-app cost. The 4–8MB/app
  marginal figure is the measurement that matters; the base cost is
  driver-dominated and hardware-specific.

## Claim categories — never blur these

```text
MEASURED   (this doc, dated entries)
    NVIDIA workstation, debug builds: tables above.
    4–8MB PSS per additional vector app. ~565MB whole-stack PSS.

PROJECTED  (extrapolation, awaiting measurement)
    Release builds smaller (binary + allocator behavior).
    Mesa/AMD/Pi: compositor ~100–150MB at 1080p. Appliance <250MB total.
    Power figures for SBC deployments.

TARGET     (engineering goals, not predictions)
    <250MB entire appliance. Fast boot. 60fps composite. <5W.
```

Public claims quote MEASURED only; PROJECTED is labeled as such; hitting a
TARGET is only credible if it was never retroactively blurred into the
other two.

## 2026-08-10 — stack change (no new numbers yet)

The gpui stack was removed (appliance ladder rung 1): rill-shell (the 347MB
RSS process above) and rill-view are deleted; the dock is now
`rill-vector --dock`, a stream client in the same class as the 9–13MB
rill-vector processes measured on 08-08; the wallpaper is compositor-
painted. The next measurement should show the desktop as compositor + N
small stream clients — re-run the same smaps method and append the entry
here before quoting any new totals.

## 2026-08-10 — first post-gpui measurement (same box, debug, NVIDIA)

Desktop = compositor + `rill-vector --dock` + `rill-vector --dashboard`,
launched via scripts/demo-desktop.sh:

```text
rill-compositor        268MB RSS   238MB PSS   (driver-dominated, as before)
rill-vector --dock      11.5MB RSS   6.2MB PSS  ← replaces the 347MB gpui shell
rill-vector --dashboard 10.1MB RSS   4.9MB PSS
whole desktop           ~250MB PSS  (was ~565MB PSS on 08-08)
```

The gpui removal cut whole-stack PSS by more than half, and the shell went
from the largest process to another single-digit-PSS stream client. Same
caveats as always: debug builds, NVIDIA driver, one sample, no apps open —
but the "fixed platform cost + tiny per-client cost" shape now includes the
shell itself.

## 2026-08-11 — verification re-run (5-app scaling test; units corrected)

**Units correction for all prior entries:** `/proc` "kB" is KiB, and earlier
entries quoted "MB" as kB/1000 — a ~2.4% systematic overstatement. From here
on figures are MiB. Corrected prior headline: the 08-08 whole-stack figure
was 551.7 MiB PSS (564,940 KiB), not "~565MB".

Same box, debug builds, NVIDIA. Baseline desktop (compositor + dock +
dashboard): **230.9 MiB PSS**. Then five vector app windows opened one at a
time (files, specimen, demo, specimen, demo):

```text
after app:   #1      #2      #3      #4      #5
marginal:  +4.8    +5.1    +2.3    +3.0    +2.2   MiB PSS
total desktop with 5 apps: 248.2 MiB PSS  (mean +3.5 MiB/app)
```

* Marginals SHRINK as clients multiply — shared pages divide across more
  processes; each app's own PSS drops too (dock 6.4 → 3.3 MiB with 5 apps
  running). The per-app claim ("4–8 MiB") was conservative; measured mean
  is ~3.5 MiB at N=5.
* Compositor PSS attribution with 5 apps live (223.2 MiB): 166.6 NVIDIA
  driver, 19.1 Mesa ICDs+LLVM (loaded, unused — `VK_DRIVER_FILES` pinning
  would remove), 20.2 anon/heap (Rill working data *plus* driver arenas),
  14.6 debug binary, 2.8 other. Rill-attributable ceiling ≈ 35 MiB, of
  which part of the heap is really the driver's.
* Closing all 5 apps returned the desktop to within 4 MiB of baseline —
  no leak signal at this scale.
* Compositor grew only +3.8 MiB across all five windows (its per-window
  scene state), consistent with windows-as-command-lists.

## 2026-08-13 — release builds, and the idle floor (same box, NVIDIA)

Closes the "re-measure with `--release`" follow-up, and adds the
measurement that was missing rather than wrong: **an idle desktop**. Every
prior entry sampled a desktop with live widgets on it, which is a busy
workload by construction — it could never have shown whether the damage
gate holds.

Method note first, because it invalidates nothing but would have:
`RILL_BENCH_PROFILE` used to be a *label*. Both `bench-stack.sh` and
`demo-desktop.sh` hardcoded `target/debug`, so a run marked `release`
would have written debug numbers into this file under a release heading.
Both scripts now select binaries by profile; the numbers below are the
first that were actually built the way they say they were.

Four runs, `scripts/bench-stack.sh`, 12s settle + 30s sample, hermetic
(own port, cache, config, data). Ryzen 9 9950X (powersave governor),
31179 MiB RAM, RTX 5070, NVIDIA 580.159.03, btrfs on Samsung 9100 PRO.
Workloads: **idle** = dock + wallpaper, nothing animating; **busy** =
the standing bench theme, two live widgets (meter, and ASCII at ~12 Hz).

```text
run              stack PSS   rill-attrib   CPU (of 1 core)   mean fps
idle,   release    258.2         38.7           0.97%          1.13
idle,   debug      267.0         47.2           0.57%          2.15
busy,   release    310.2         61.2           6.07%         28.74
busy,   debug      273.9         53.7           8.00%         29.69
```

**The damage gate, measured.** The compositor now reports lifetime frames
split by cause on exit. Idle release: **48 frames in 42.3s — 41 heartbeat,
7 damage.** That is the 1 Hz self-heal and essentially nothing else, which
is what "a quiet desktop renders nothing" has to mean to be a claim. Busy
release: 1213 frames, **0 heartbeat, 1213 damage** — with two live widgets
the desktop never goes quiet long enough for the heartbeat to fire.

**Idle vs busy is the number that matters for battery/SBC work:** the same
desktop is 0.97% and 1.13 fps doing nothing, 6.07% and 28.7 fps with two
widgets. The widgets, not the desktop, are the load — and the ASCII widget's
`seconds = 0.08` (~12 fetches/s) was chosen for a video, not for battery.

**Release vs debug is NOT a clean comparison here, and shouldn't be quoted
as one.** Per-client PSS did drop unambiguously (rill-vector 4.6/5.4/5.8 →
3.6/4.5/4.6 MiB; files-app 8.3 → 7.4), and busy CPU dropped 8.00% → 6.07%.
But busy-release's *total* is higher than busy-debug's, entirely because
that run's compositor carried 216.2 MiB of NVIDIA mappings against 186.7
MiB in the debug run. Driver arenas move run to run and dwarf the build
difference; the "rill-attributable" bucket includes them (it is anon+binary,
and anon holds driver arenas). On this driver, only the *client* figures and
the CPU figures separate the two builds honestly.

**Binary size, release, stripped** — the appliance-relevant disk number:

```text
rill-compositor  14.9 MiB      rill-server   3.1 MiB
rill-vector       8.5 MiB      rill          4.6 MiB
                               files-app     4.4 MiB
core desktop (compositor + vector + server): 26.5 MiB
```

Unstripped release is 19.2 / 10.6 / 3.9 / 5.5 MiB. Debug binaries are
405 / 339 / 68 MiB — which is why debug *binary* PSS was never the story.

**Cache growth is zero in all four runs** (was 0.38 MiB/min ≈ 547 MiB/day
before this week's `get_uncached` change). Server log volume is zero
(was 9.7 lines/s) with `RILL_LOG` unset.

Caveats, same as always: one box, one sample per cell, NVIDIA, nested in a
host compositor whose vsync paces our present. The sub-1% CPU cells are
noise-dominated — do not read 0.97% vs 0.57% as a regression.

## 2026-08-14 — first full `bench-device.sh` run (same box, release, NVIDIA)

The instrument's own first real output: release builds at 1ab9d6e, 60-second
idle and busy windows, scaling to twenty apps. Bundle kept locally; the
numbers that matter are here.

```text
idle    (idle-v1)      263.1 MiB stack PSS   1.56% of one core   1.08 fps
busy    (widgets-v1)   230.1 MiB stack PSS   3.42% of one core  15.32 fps
                       (meter 1 Hz, ascii seconds=0.08)
```

**The damage gate, over a minute rather than thirty seconds:** 85 frames in
the idle run, of which **78 were the 1 Hz heartbeat and 7 were damage**. The
earlier 42-second sample gave 41/7; a window twice as long moves the
heartbeat count and leaves the damage count where it was, which is what a
desktop that genuinely stops drawing looks like.

**Scaling, now to N=20** (baseline ≈ 221.7 MiB):

```text
apps        1        5       10       20
from base  +3.3    +20.9   +34.1   +59.8   MiB PSS
least-squares slope: 2.88 MiB/app   (DERIVED)
```

That corroborates the +3.5 MiB/app mean measured at N=5 on 08-11 and extends
it four times further. Per-client PSS keeps shrinking as clients multiply —
an app's own PSS is 4.3 MiB at N=1 and 2.6 MiB at N=20 — so the marginal
cost falls with scale rather than rising. Closing all twenty returned to
within 15.2 MiB of baseline; larger than the ~4 MiB seen at N=5, and still
not evidence of a leak, only of not having returned all the way.

**The important caveat, and the reason to read per-process numbers rather
than the total.** Compositor PSS by phase, same binary, four launches:

```text
idle 250.8   busy 210.8   scale_1 210.0   scale_20 219.0   MiB
```

A 40 MiB spread between launches of the same binary, which is why busy
appears to use *less* memory than idle in the table above. The NVIDIA
driver's arenas move more from one launch to the next than either workload
moves them. So on this machine:

* **whole-stack PSS cannot distinguish idle from busy** — the noise floor is
  larger than the signal;
* the figures that *are* comparable across phases here are the client
  processes (dock, widgets, apps) and CPU;
* and the slope, being a difference measured within one compositor launch,
  is unaffected.

Do not quote an idle-vs-busy memory delta from this hardware. Quote the
slope, the client costs, and the CPU.

Cache growth across the whole run: **0.00 MiB**. Peak temperature 46 °C
(`nvme.temp2` — the CPU never rose enough to be the hottest sensor). Power
unavailable: the only hwmon power rail here is the amdgpu one, and RAPL is
not readable unprivileged.

## The instrument: `scripts/bench-device.sh` (2026-08-14)

The protocol below is now a script rather than a description. One run
produces one immutable bundle — `summary.json`, `summary.txt`, per-second
`samples.csv` and `processes.csv`, `scaling.csv`, and raw `/proc` and `/sys`
captures at every checkpoint — so a run can be re-analysed later, or its
attribution improved, without touching the hardware again.

```bash
scripts/bench-device.sh --profile release --idle-seconds 60 --scale 1,5,10,20
```

It measures: environment and GPU inventory, verified build profile, a
pre-Rill system baseline, idle desktop, a *named* busy workload, app-count
scaling with least-squares slope, close-and-recover, cache growth, loopback
bytes, and thermals. What the platform does not expose comes out `null`.

Things it refuses to do, each because this log has been misled by one of
them before:

* **Label a run `release` while measuring `target/debug`.** The profile is
  resolved from the binary's actual path.
* **Quote kB as MiB.** Everything is KiB/1024, from `/proc`, at the source.
* **Call driver memory Rill's.** Stack PSS and per-process attribution are
  reported separately, and no single "Rill owns this much" figure is
  synthesised.
* **Report a GPU rail as system power.** On this workstation the only hwmon
  power sensor is `amdgpu power1_input`; the script reports power as
  *unavailable* rather than quoting it.
* **Assume `thermal_zone0` is the CPU.** It reads hwmon too and records
  which sensor was hottest — here `acpitz` reads 16 °C while `k10temp`
  reads 58 °C.
* **Treat a partial run as a failure.** An OOM-limited or short run keeps
  everything collected and records why it stopped.

Runs are gitignored: a bundle is evidence about one machine at one moment.
When a run matters, append it to this file as a dated entry.

## 2026-08-15 — the Pi, at last: **33.6 MiB idle** (Pi 5 1 GB, V3DV/Mesa)

The measurement every projection in
[resource-envelope.md](resource-envelope.md) was standing on. Raspberry Pi 5
Model B Rev 1.1, **1 GB**, Debian 13 trixie, kernel 6.18, V3D 7.1.10.2 on
V3DV Mesa 25.0.7, nested in labwc, release binaries cross-compiled on the
workstation (`scripts/cross-build-pi.sh`).

Two runs. Run 1 was **killed by the OOM killer during the N=10 scaling step**
and left no `summary.json` — a SIGKILL runs no trap; its three completed
phases are intact in its CSVs. Run 2 capped the sweep at three apps and
completed (`status: complete`), and is the canonical bundle
(`2026-08-15T113153-0700_raspberrypi`).

```text
                     run 2 (complete)        run 1 (OOM at N=10)
                    PSS    CPU     fps      PSS    CPU     fps
system baseline      —      —       —      5.9M   0.00%     —
idle    (idle-v1)  27.8M   0.90%   1.08   33.6M   1.17%   1.10
busy    (widgets)  40.6M  34.91%  29.92   36.0M  34.29%  29.75
peak temperature    62 °C                  60 °C
swap in use        110 → 183 MiB          138 → 209 MiB
```

Idle differs by 5.8 MiB between runs on a box that is already swapping, which
is the honest precision here: **idle is ~28–34 MiB**, and the second decimal
place is noise, not signal.

**Idle is 27.8 MiB against a PROJECTED 120–200 MiB.** The projection was
built as "Rill's own ~39 MiB measured + Mesa userspace, tens of MiB +
swapchain" and it was too pessimistic: the whole stack on V3DV costs less
than Rill's attributable share did on NVIDIA. For comparison on the same
workload, the workstation measured **263.1 MiB** idle — the NVIDIA driver is
most of that, and V3DV simply does not have an equivalent arena. The
appliance target of "<250 MiB entire appliance" is not close; it is
7× clear.

**The damage gate is platform-independent**, which is the architectural
result rather than the memory number. Idle: **70 heartbeat and 7 damage** of
77 frames (run 1: 72/10 of 82). The workstation's 60-second window gave 78/7
— the same split, on a different GPU vendor at a fifth of the memory.

**CPU is ~5× the workstation's under load** (34.29% vs 6.07% of one core for
two widgets), which is a Cortex-A76 being a Cortex-A76 and not a regression.
Busy still held 29.75 fps, every frame damage-driven, zero heartbeat.

**No throttling, either side of the load:** `throttled=0x0` before and after,
peak 60 °C. The renderer is recorded as V3D, not lavapipe, so the numbers
describe the GPU.

**The scaling slope is NOT measurable on this board, and the summary's own
figure proves it rather than reporting it.** Run 2 computes
`linear_slope_mib_per_app: -5.97` and `post_close_delta_mib: -10.4` — a
desktop that gets *smaller* as you add applications and ends below where it
started. Both are swap artefacts: the Pi OS session alone leaves the box
110–138 MiB into swap before Rill starts, so adding apps evicts pages and PSS
falls (run 1: 37.0 MiB at N=1 → 23.7 MiB at N=5, swap 217 → 287 MiB). Less
resident is not less used. **The 2.88 MiB/app slope measured on x86 remains
uncorroborated on ARM**, and a negative slope must never be quoted as a
result. `swap_used_mib` is now in the summary so the next reader can see when
PSS is lying.

### Where the busy CPU actually goes (attribution from `processes.csv`)

34.91% of a core is the headline; the split is the useful part, and it is not
where the document pipeline would predict:

```text
rill-compositor   29.89%    ← 86% of the total, and confirmed ours (below)
files-app          2.06%    server: sample, generate KDL, compile to .rill
rill-vector        1.01%    per widget: fetch, decode, resolve, layout, encode
dock               0.19%
                  ------
idle, for scale:  compositor 0.88%, vector 0.03%, files-app 0.00%
```

**The whole generate → compile → transfer → decode → resolve → layout chain
costs about 3.5% of one core between all of its processes.** The compositor
alone costs 8.5× that. Whatever "work amplification" means on this board, it
is overwhelmingly in compositing, not in the document pipeline — which is the
opposite of what the pipeline's length suggests.

Two things sharpen it further:

* **2.22 frames rendered per content update.** The workload generates 13.5
  updates/s (ascii at 12.5 Hz, meter at 1 Hz) over 77.3 s = 1044 updates, and
  the compositor drew 2312 frames, every one damage-driven, zero heartbeat.
  It is redrawing more than twice per change.
* **The nesting confound was measured, and it is small — the work is ours.**
  A third run (`2026-08-15T122601`) sampled labwc alongside, with
  `scripts/sample-host.sh` using the harness's own CPU method:

  ```text
  phase   labwc   rill-compositor   rill-vector   files-app
  idle    0.17%        1.16%           0.05%        0.00%
  busy    1.83%       27.02%           0.90%        1.90%
  ```

  **The host compositor costs 1.83% of a core under the busy workload** —
  less than the server does. Presenting into labwc is not where the CPU goes;
  our own compositor is 15× it. Milestone 15 (owning DRM directly) would
  therefore recover ~2% of a core, not the ~15% a "cost of being a guest"
  story would have predicted. That is worth knowing before anyone justifies
  bare-metal work on performance grounds: the case for it is boot time and
  appliance shape, not this.

  The run also validates the instrument. An independent sampler measured
  `rill-compositor` at 27.02% where the harness reported 29.89% in run 2 —
  different runs, and this one carries the sampler's own ~1% overhead, so two
  methods agreeing to within 3 points is the cross-check passing.

  labwc's cost does scale with our frame rate (0.17% → 1.83%, ~11×, tracking
  the frame count), so the 2.22×-per-update ratio is charged twice. But the
  second charge is under two points, so cutting the ratio is worth ~27% of
  its own saving plus ~2%, not double.

### Frame times, and what they rule out (runs 4 and 5, instrumented)

`rill-compositor` now reports a frame-duration histogram and, separately, the
time spent waiting for a swapchain image. Both phases, release, same board:

```text
          frame_ms                                    acquire_ms   work
          mean   p50    p95    p99    max     n       mean  max    mean
idle      6.07   6.50   9.75  21.00  20.92    63      0.05  0.11   6.02
busy     28.98  29.00  32.25  32.75  82.80  2304      0.07  0.28  28.91
```

Three things this settles, and one it does not.

**The distribution is tight, not bursty.** p50 29.00 against p99 32.75 under
load. The earlier 54% CPU peak in a one-second sample suggested work arriving
in lumps; the frame histogram says it does not. Whatever costs 29 ms costs it
on *every* frame, which points at structural per-frame work — the compositor
has no render cache, so every composite rebuilds each window's GPU buffers
from its DrawCommand list — and away from an occasional expensive path.

**It is not vsync back-pressure.** 29.62 fps is a 33.8 ms interval, 2.03× the
16.67 ms vsync period, which looks exactly like missing the 60 Hz deadline and
locking to every second vsync. But `acquire_ms` is **0.07 mean, 0.28 max** —
the swapchain never blocks. The frame is slow before the display is involved.

**It is not CPU-bound either, and that is the interesting part.** The
compositor spends 29.21% of one core during busy: ~9 ms of CPU per frame
against a 29 ms frame. Roughly 20 ms per frame is the thread neither
computing nor waiting on the swapchain. The blocking `pop_error_scope` calls
in rill-gpu are all in shader-compilation paths, not the composite path, so
the remaining suspect is GPU execution time — V3D finishing work the frame
span waits on.

### The 10-second desktop hitch is GNOME, and cannot be fixed from here

Recorded so nobody spends another afternoon on it. On this workstation a
shader wallpaper visibly lags about every ten seconds. It is not Rill.

Phase-splitting the frame puts the whole stall in the swapchain acquire —
`composite` stays at ~1 ms — and sampling the host at 50 ms resolution shows
why:

```text
gnome-shell CPU burst   140 ms   at t = 7.9, 17.8, 27.9 s
our acquire blocks      ~140 ms  at the same instants
```

GNOME Shell does ~140 ms of single-threaded work on a ten-second timer (its
gjs runtime collects on that cadence), and a compositor that is busy is a
compositor that is not compositing. **Every window on the screen is frozen
for that window, not only ours** — nothing a Wayland client does can put
pixels up while the host owning the display is stalled.

Ruled out as mitigations, measured rather than assumed: Mailbox still stalls
(3 events against FIFO's 4, 120 ms against 152 ms) though it does drop the
ordinary frame span from 9.25 ms to 0.75 ms by not folding the vblank wait
into it; Immediate is not offered by this surface at all (`[Mailbox, Fifo]`)
and requesting it is a wgpu validation *panic*, now guarded; and a deeper
swapchain cannot absorb 140 ms, which is 8.4 frames at 60 Hz.

It is absent under labwc on the Pi — same binary, same shader, acquire max
7.57 ms — and it will be absent on bare metal, which has no host to stall us.
Fewer GNOME extensions means a smaller JS heap and a shorter collection, if
it needs reducing on this machine today.

### An animated wallpaper defeats the damage gate (and is meant to)

Chased as a suspected bug — "an *empty* compositor drew 49 damage frames" —
and it was not one, twice over. `rill-compositor` with no client arguments
spawns `rill-vector --dashboard` by default, so the run was never empty; the
frames were that client sampling at ~2 Hz. The observation was a bad test,
reported three times before it was checked.

What the chase did establish, measured, same machine and client, 15 s each,
differing only in the theme:

```text
static wallpaper ([colors] page only)      44 frames    2.98 fps
animated background shader (ocean.wgsl)   877 frames   59.42 fps
```

**A desktop with an animated wallpaper never idles.** That is correct — the
wallpaper genuinely changes every frame, so the damage gate is right to
composite — but it means the idle numbers in this log describe a desktop with
a *static* background, which is what `bench-device.sh`'s idle-v1 writes
(`[colors] page` and nothing else). A riced desktop with a shader wallpaper
idles at the frame budget, not at the 1 Hz heartbeat, and the "quiet desktop
costs nothing" property does not apply to it.

Worth stating wherever that property is claimed: it is a property of the
*content*, not of the compositor. The gate holds; an animation is simply
content that always changed.

### Correction: the 29 ms frame was waiting for the display

Two further experiments, and the second overturns the reading above.

**Pixels do not matter.** The busy workload at two output sizes, same
content:

```text
1280x800   frame p50 29.00 ms   28.72 fps
 640x400   frame p50 29.00 ms   29.90 fps
```

Four times fewer pixels, identical to the hundredth of a millisecond. Not
fill rate, and not the fx chain's render targets either.

**It was presentation pacing.** `RILL_PRESENT_MODE=immediate` runs the same
workload with FIFO's display sync removed:

```text
AutoVsync   frame p50 29.00 ms   CPU 28.14%   28.7 fps
Immediate   frame p50  7.25 ms   CPU 22.83%   25.9 fps
```

**Our compositor's real frame cost is ~7.3 ms, not 29 ms.** The frame span
was measuring the wait for the display and reporting it as work, which is why
it was flat against pixels, flat against content, and pinned at 29.00: it was
never a measure of drawing. Arithmetic corroborates — 25.9 frames/s × 7.25 ms
predicts 18.8% of a core against 22.8% measured, where the 29 ms figure
predicts 75% and the process plainly was not using it.

So the corrected picture of the busy workload:

```text
compositor real work     ~7.3 ms/frame   ~19-23% of one core
document pipeline         (unchanged)      ~3.5% of one core
host compositor labwc     (unchanged)      ~1.8% of one core
```

**The biggest remaining lever is the frame count — and it is the clients,
not the compositor.** The "~2.1 frames per content update" first written here
was arithmetic over the widgets' *declared* refresh rates, not a measurement,
and it pointed at the wrong process. The compositor now counts
content-carrying commits for a whole run:

```text
busy   commits 1948   frames 1790   frames_per_commit 0.92
idle   commits    9   frames   25   frames_per_commit 0.78
```

**The compositor draws slightly fewer frames than it is given content for —
it coalesces, and was never double-drawing.** What is doubled is upstream:

```text
declared widget updates   13.5/s   (ascii at 12.5 Hz + meter at 1 Hz)
measured client commits   32.0/s
                          2.37x
```

`rill-vector` commits ~2.4 frames per live tick. Each of those costs the
client almost nothing — the widgets measured ~1% of a core each — but forces
the compositor into a 7.25 ms composite, so cheap client work is buying
expensive compositor work. At one commit per update the compositor would draw
12.4 fps instead of 29.4, and cost **~9% of a core instead of ~21%**.

The suspect is `AppView::poll` returning `changed || still_pending`: scroll
easing, image loads and the `stick_to_end` follow all keep the host asking
for frames after the content that caused them has already been drawn. That is
a client-side question, and it is worth answering before any compositor
optimisation, because it halves compositor cost without touching the
compositor.

### The wait was also the stutter (x86, 2026-08-20)

The correction above stopped at "the frame span was measuring the wait". The
wait turns out to cost more than a mismeasurement: it stutters. Measured on
the desktop box (RTX 5070, GNOME/Wayland host, debug build), 45 s of an idle
photograph roll, same scene both ways:

```text
             frame p50   p99      max      acquire mean   fps
AutoVsync     14.75 ms   17.25    71.44    13.09          59.4
Mailbox        0.75 ms    1.25    25.33     0.06          59.8
```

Under FIFO the frame log shows a **60-70 ms stall every ten seconds**, like
clockwork, four dropped frames at a time and all of it inside `acquire`. The
compositor's own drawing is 0.67-0.70 ms in both — the entire difference is
waiting, and the waiting is periodically much worse than a frame.

Nested inside another compositor, the host already paces us with frame
callbacks. Asking the swapchain to pace us as well is two clocks for one
rhythm, and the beat between them is the stall. **Mailbox is now the default
where a surface offers it**, falling back to AutoVsync where it does not —
which may be the Pi, whose V3D surface offers Immediate and is unmeasured for
Mailbox. Not Immediate, which would tear; Mailbox always presents whole
frames, it just does not make us queue behind them.

Worth noting what this does *not* change: 0.70 ms of drawing per frame at
1280x800 on a discrete GPU is the same 0.70 ms it was under FIFO. The
headroom was always there and the display was hiding it.

A counting bug found on the way, and it was not only mine: the commit test
recognises a `wl_buffer` or client damage, but a vector window delivers its
content through `rill_stream_v1` instead — so **the HUD's "N/s client
commits" has been reading zero on every Rill desktop**, which is every
desktop made of vector windows. Latched stream frames are now counted too.

### Fixed: 43% off the busy desktop (2026-08-15, same board)

`AppView::poll` returned `changed || still_pending` — one bool for two facts —
and every caller read it as "repaint". A fetch in flight alters nothing on
screen, so each live tick drove a redraw and a commit on every loop iteration
until the response landed. Two amplifiers fed on it: `pump` redrew on
`busy || was_busy` (a trailing redraw after the work had finished), and
`draw` itself polled and set `dirty` from the result, sustaining its own
frame-callback loop.

`poll` now returns `Polled { changed, pending }`. Repaint on `changed`;
`pending` only shortens how long the loop will sleep. Measured, busy phase:

```text
                    before    after   change
client commits/s     32.00    13.18     -59%
frames/s             29.45    13.05     -56%
compositor CPU %     28.14    15.50     -45%
whole-stack CPU %    34.43    19.69     -43%
frames_per_commit     0.92     0.99
```

The widget also runs at the rate it asks for now, which took two attempts and
is the part worth remembering. Removing the phantom redraws first dropped the
tick rate to **10.6/s for a page declaring 12.5** — the loop had been polling
often as a side effect of painting often, and without that the clock fired
late. Halving the sleep did not help, because the problem was *alignment*, not
granularity: a fixed cadence cannot hit an arbitrary interval, and the fetch
that resets the phase makes it drift. `AppView::next_tick_in` now reports the
actual deadline and the loop sleeps to it; ticks measure 13.2/s against 13.5
declared.

That correction cost CPU back (12.67% → 15.50%) and was still right: the
cheaper number was a widget running 15% slow. A performance win that quietly
changes behaviour is not a win, and it would have been easy to bank the
smaller figure and never notice.

**What it does not settle: which content costs it.** Idle draws one window
(the dock) at 6.02 ms of work; busy draws three (dock, meter, ASCII) at 28.91
ms. 4.8× the cost for 3× the windows — and the ASCII widget is dozens of rows
of text where the others are a handful of elements, so window count and
content weight are confounded. The clean experiment is one widget at a time.

Note also what the busy workload is *not*: it contains no terminal. It is one
12.5 Hz ASCII widget and one 1 Hz meter. The 20 Hz terminal refresh is a
separate, unmeasured cost.

What this run does not establish: boot-to-desktop (needs milestone 15),
power (no board sensor), the marginal-cost slope (above), frame-time
distribution (the harness records mean fps only), interaction latency, and
anything about multi-day behaviour.

## Planned: Pi/Mesa measurement protocol

Run the identical demo on the 1GB Pi (or any Mesa machine) and record, per
entry: OS, kernel, Mesa version, resolution, refresh, build profile,
renderer/adapter, boot→shell time, idle total PSS, compositor PSS, shell
PSS, per-app PSS, idle/active CPU %, frame time, power (W) if measurable.

Then launch 1 / 5 / 10 / 20 apps and record total PSS at each step. The
hypothesis under test: memory ≈ fixed platform cost + N × single-digit MB.
Plot the slope; the slope is the result. If it holds on Mesa in release
builds, the lightweight-appliance thesis is measured, not argued.

## Follow-ups (do before quoting numbers publicly)

- [x] Re-measure with `--release` builds (binary size + allocator behavior).
      Done 2026-08-13 — but note the entry's caveat: on NVIDIA the driver
      arenas move more between runs than the build profile does, so only
      the per-client and CPU figures separate the two builds honestly.
- [ ] Pin the ICD (`VK_DRIVER_FILES=<nvidia json>`) so wgpu stops loading
      Mesa's unused drivers into an NVIDIA process; measure the delta.
- [ ] Measure on an AMD iGPU / Mesa machine — that's the kiosk-class target.
- [ ] Record resolution alongside future entries (swapchain scales with it:
      ~33MB/buffer at 4K, ~8MB at 1080p, × buffer count).

## Method (reproducible)

```bash
pgrep -a rill                                  # find pids
grep -E '^(Rss|Pss)' /proc/<pid>/smaps_rollup  # totals
# attribution: walk /proc/<pid>/smaps, bucket each mapping's Rss by path:
#   target/*rill*        → Rill binary
#   *nvidia*/dev/nvidia* → NVIDIA driver
#   *vulkan*|*LLVM*|*dri*→ Mesa/compilers
#   [heap]/anonymous     → app data + driver arenas
#   *.so (rest)          → other system libs
```

RSS = pages resident for this process (shared pages counted fully in every
process). PSS = shared pages divided among sharers — sum PSS across
processes for an honest whole-stack number.
