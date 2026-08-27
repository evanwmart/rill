# Resource envelope — what Rill costs, and what it needs

Status: **derived document**, 2026-08-13. Every figure traces to a dated
entry in [memory-footprint.md](memory-footprint.md) or is marked
PROJECTED/TARGET. Nothing here is a new measurement; this is the log turned
into requirements.

Read the labels literally:

* **MEASURED** — observed on a named machine, reproducible via
  `scripts/bench-stack.sh`.
* **PROJECTED** — extrapolation with stated reasoning. Not evidence.
* **TARGET** — an engineering goal. Hitting it is only credible if it was
  never quietly reclassified.

The reference machine for every MEASURED figure below is one box: Ryzen 9
9950X (powersave), 31 GiB RAM, **RTX 5070 on the NVIDIA proprietary driver**,
nested in a host Wayland compositor. That driver is the single largest
consumer in every measurement and is *not* representative of the hardware
Rill is aimed at — see "The driver is the bill" below.

## Scenario consumption (MEASURED, 2026-08-13, release builds)

| Scenario | What is running | Stack PSS | Rill-attributable | CPU (of 1 core) | Frames |
|---|---|---|---|---|---|
| **Idle desktop** | compositor + dock + wallpaper, nothing animating | 258.2 MiB | 38.7 MiB | **0.97%** | 1.13 fps (41 heartbeat / 7 damage) |
| **Busy desktop** | + 2 live widgets (meter; ASCII at ~12 Hz) | 310.2 MiB | 61.2 MiB | **6.07%** | 28.7 fps (0 heartbeat / 1213 damage) |
| **App server** | `files-app` (rill-server + a handler) | 7.4 MiB | 7.3 MiB | 0.83% | — |
| **Per extra app window** | vector client | 3.6–4.6 MiB | — | 0.07–0.77% | — |

Two things this table is really saying:

**Idle is genuinely idle.** 41 of 48 frames over 42 seconds were the 1 Hz
self-heal heartbeat. The desktop is not spinning a render loop at 60 Hz and
throwing frames away; it composites when something changes. This is the
property that makes battery and fanless deployment plausible at all, and it
is now measured rather than asserted.

**The load is the widgets, not the desktop.** Idle → busy is 0.97% → 6.07%
CPU, and the whole difference is two widgets, one of which re-fetches ~12
times a second because that rate was picked to look good on video. Widget
cadence is the dominant power knob on any battery device — `seconds = 0.2`
would look nearly identical and cost roughly a third as much.

### Scaling shape (MEASURED, 2026-08-11, debug builds)

Five app windows opened one at a time onto a running desktop:

```text
marginal PSS per app:  +4.8  +5.1  +2.3  +3.0  +2.2  MiB   (mean +3.5)
compositor growth across all five windows:  +3.8 MiB total
closing all five returned to within 4 MiB of baseline (no leak signal)
```

Marginals *shrink* as clients multiply (shared pages divide across more
sharers). The result to defend is the shape — **memory ≈ fixed platform cost
+ N × single-digit MiB** — not any one intercept.

### Disk (MEASURED, release, stripped)

```text
rill-compositor  14.9 MiB       rill-server  3.1 MiB
rill-vector       8.5 MiB       rill         4.6 MiB
core desktop (compositor + vector + server): 26.5 MiB
```

Content cache: **bounded by design** as of this week — 64 MiB default budget,
swept on connect *and* on store. Measured growth in all four bench runs:
**0.00 MiB/min** (it was 0.38 MiB/min ≈ 547 MiB/day before the live-page
cache bypass landed).

### Network (instrumented 2026-08-19; first sample, not yet a bench entry)

Request *rate* was already measured (9.7 requests/s with two widgets).
As of 2026-08-19 the server counts **protocol bytes** (post-TLS plaintext,
compressed responses at compressed size) via `rill_server::WireStats`;
`RILL_STATS=<path>` writes a JSON snapshot every 5 s, and bench-device.sh
embeds it in `summary.json` as `network.server_wire` — distinct from the
coarse loopback-interface totals.

First smoke sample (workstation, debug, ~45 s desktop with dock + meter
widget + three app launches): **rx 24.7 KB, tx 235.6 KB (~5 KB/s), 48
connections.** Quote nothing from this beyond "instrumented and plausible"
— the citable figure comes from the next full bench-device run. The
"~2KB/frame" claim still needs *per-frame* attribution; this is aggregate.

The sample also surfaced **48 connections in 45 s** — and the first
reading of it here ("clients connect per fetch") was wrong: the fetcher
already reuses one keepalive-pinged connection per origin for GETs. The
actual per-request diallers were **ACTIONs** (`spawn_action` connected,
acted, and closed every time — fixed 2026-08-19: actions now ride the
shared connection via `Fetcher::with_client`, with no stale-socket retry
because a transport error mid-write is ambiguous) and the **CLI
one-shots** during demo setup (trust + four installs). Remaining
attribution comes free from the per-connection close logs
(`closed rx_bytes= tx_bytes=`) under `RILL_LOG=info`; re-count after the
next bench run rather than theorizing further.

## The server envelope (stated 2026-08-19)

The one-line version, labels inline:

> **rill-server: 3.1 MiB on disk, ~7 MiB resident, under 1% of one core
> serving a desktop's whole traffic (MEASURED). Floor: any Linux box with
> ~32 MiB of RAM to spare — no GPU, no display (PROJECTED). Ceiling
> committed: under 64 MiB with every planned capability on (TARGET).**

The same, unpacked:

* **MEASURED** — 3.1 MiB stripped release binary; 7.4 MiB PSS and 0.83%
  of one core serving the demo suite on x86 (2026-08-13); ~2% of a
  Cortex-A76 core under the busy Pi workload (2026-08-15). Serving is
  I/O-bound; the server has never been the bottleneck in any run.
* **Minimum acceptable host (PROJECTED)** — any Linux (x86-64/aarch64)
  with ~32 MiB free RAM and ~10 MiB of disk beyond content. No GPU, no
  display, no session. Router-class and NAS-class boxes qualify; the
  binding constraints on tiny hosts are TLS handshake CPU (per
  connection — mitigated by connection reuse, added 2026-08-19) and
  content-cache disk, not steady-state serving.
* **TARGET (the capability-creep guard)** — the *full-capability* server
  (bridge + history + broker + push) idles under **64 MiB PSS and 0%
  CPU**; capabilities are lazily initialized (a capability named in no
  installed manifest costs nothing resident); marginal cost per
  additional app handler stays **single-digit MiB**. Bench follow-up
  that makes drift visible: record server PSS at 1 vs 8 handlers, and
  with each capability toggled, alongside the existing app-count slope.

## The driver is the bill

In every run, the largest single line item is the GPU driver's userspace,
not Rill:

```text
idle, release:  compositor 244.4 MiB PSS = 186.9 NVIDIA + 25.5 Rill + rest
busy, release:  compositor 290.1 MiB PSS = 216.2 NVIDIA + 42.1 Rill + rest
```

NVIDIA arenas moved ~30 MiB between two runs of the same binary — more than
the entire debug-vs-release difference. Consequences for how these numbers
are used:

* Rill's own working set for a whole idle desktop is **~39 MiB MEASURED**.
* The 258 MiB total is an *NVIDIA workstation* number and is the worst case,
  not the target case. Mesa (RADV/ANV/V3D) userspace is far smaller and ACO
  removes the LLVM dependency that costs ~19 MiB here in loaded-but-unused
  ICDs.
* ~~PROJECTED: idle total of 120–200 MiB on Mesa hardware at 1080p~~ —
  **superseded 2026-08-15 by MEASURED 27.8 MiB** on a Pi 5 (1 GB, V3DV Mesa
  25.0.7, nested in labwc, release). The projection was built as "Rill ~39
  MiB + Mesa userspace, tens of MiB + swapchain" and was too pessimistic by
  4–7×: the entire stack on V3DV costs less than Rill's attributable share
  did on NVIDIA. Two runs agree (27.8 and 33.6 MiB idle). See
  memory-footprint.md 2026-08-15.

## Recommended systems

### Will thrive (PROJECTED unless noted)

| Class | Example | Basis |
|---|---|---|
| **x86 mini-PC / workstation** | N100 box, any modern desktop | MEASURED on a 9950X/RTX 5070: 0.97% CPU idle. Headroom is enormous; this class is over-provisioned for Rill. |
| **SBC kiosk / signage, 2 GB+** | Pi 5 (4 GB), CM5 carrier | **MEASURED 27.8 MiB idle on a 1 GB Pi 5**; a 2 GB+ board is not remotely constrained. Static content + damage gate ⇒ near-zero steady-state cost. Best fit for the architecture. |
| **Self-hosted app server** | router, NAS, any always-on box | MEASURED 7.4 MiB PSS, 3.1 MiB binary for a serving process. "Your computer could run on a router" is the best-supported claim in the stack. |
| **Battery/fanless terminal** | Pi 500 + portable monitor | PROJECTED. Rests entirely on the idle measurement above; widget cadence is the deciding variable, not the platform. |

### Marginal — needs measurement before promising

* **1 GB boards.** Rill itself **fits easily — MEASURED 27.8 MiB idle on a
  1 GB Pi 5.** What does *not* fit is the rest of a Pi OS desktop session
  around it: that board is already ~110–138 MiB into swap before Rill starts,
  and the benchmark's own scaling sweep OOM-killed at ten apps. So: Rill is
  not the constraint on a 1 GB board, and a 1 GB *appliance* — Rill on a
  minimal session rather than nested in a full desktop — is now the
  interesting question rather than a doubtful one. Note also that a 1 GB
  Pi 5 exists; do not infer the board from the RAM.
* **4K multi-monitor.** Swapchain scales with resolution (~33 MiB/buffer at
  4K vs ~8 MiB at 1080p, × buffer count, × outputs). Memory-bound before
  CPU-bound.
* **Many live widgets.** Cost is linear in widget refresh rate, and the
  measured busy case is only two. Ten widgets at 12 Hz is a different machine.
* **Software rendering (llvmpipe / no GPU).** Untested. The damage gate helps
  more here than anywhere, but nothing has been measured.

### Will not thrive (structural, not tuning)

* **Media playback.** Codecs are the first large dependency the appliance
  cannot avoid, and image transport in vector windows is still incomplete.
  Honest as a secure terminal before it is honest as a media machine.
* **Anything needing a browser.** Not a limitation to tune around — it is the
  security claim. The machine cannot parse web content by construction.
* **High-frame-rate interactive content.** The architecture optimizes for a
  desktop that is usually still. It is not a game or video pipeline.

## Targets (goals, not predictions)

Carried forward unchanged from appliance.md so they stay distinguishable:
**<250 MiB entire appliance**, fast boot, 60 fps composite, **<5 W**. None of
these are measured. The <250 MiB target is now *bracketed* by measurement —
258.2 MiB MEASURED on the worst-case driver, 120–200 MiB PROJECTED on the
intended one — but bracketing is not achieving.

## What would most improve confidence

1. **Run the Pi/Mesa protocol.** Every appliance number is projected off one
   NVIDIA box. This is the single highest-value measurement outstanding.
2. **Instrument wire bytes** in `bench-stack.sh`. The remoting economics
   claim is currently unmeasured.
3. **Pin `VK_DRIVER_FILES`** so wgpu stops loading unused Mesa ICDs into an
   NVIDIA process (~19 MiB here), then re-measure.
4. **Run a long soak.** All figures are 30-second samples. risks.md #4 asks
   for 72h/7-day soaks; the cache-sweep gap fixed on 2026-08-13 was exactly
   the class of bug only a soak finds.

## The headroom rationale (2026-08-19, derived — savings are reallocatable)

The figures above are usually read defensively ("Rill fits on cheap
hardware"). The stronger reading: on a fixed machine, resources are
zero-sum between presentation and payload, so **every MiB the display
layer doesn't take is returned to what the machine is for.** The savings
are MEASURED; each reallocation below is PROJECTED until something
actually occupies the headroom.

* **Co-residency.** A 1 GB board under a browser kiosk spends most of
  itself before content exists; under Rill, ~900 MB remain after the
  entire desktop (18 apps ≈ 188 MB — less than one alacritty, MEASURED).
  "Too small for a desktop" becomes "a desktop plus the household's
  servers" on one board — a device-class jump per BOM. For an OEM: UI at
  5% of the SoC instead of 60% means the product's function gets the
  silicon, or the BOM drops a tier.
* **The agent lives in the headroom.** Local models need exactly the
  resource browser-desktops hoard: an 8 GB box holds a quantized 7B
  model *or* a modern desktop's working set, rarely both. A 30 MiB
  desktop is a desktop that can afford a resident AI — the private-agent
  north star is funded out of the presentation budget, on the same
  machine. (PROJECTED; also the obvious demo bundle.)
* **Bandwidth: the interactive class is protected, not just cheaper.**
  Kilobyte frames occupy queues briefly, so interactivity survives
  *congested* links where video-based remoting dies of bufferbloat — the
  session stays live while a backup saturates the uplink; a classroom of
  screens shares an AP that the same count of video streams would
  flatten. UI traffic coincides with bulk traffic on one wire without
  contending. (PROJECTED — wire bytes are still uninstrumented; see
  "What would most improve confidence" #2.)

Scope honestly: the argument holds where glass and payload share a
machine or a link. A thin client to a distant server saves the client's
memory and donates nothing to the server — but "glass and brain share
the box or the LAN" is precisely the self-hosted shape the project aims
at.

Demo candidates that would convert PROJECTED to MEASURED: one 8 GB box
running the full desktop plus a local model; one 1 GB Pi running the
desktop plus the household's app servers; a session remoted over a
deliberately saturated link.
