# The appliance — Rill as the whole machine

Status: **direction, nothing built.** Written 2026-08-09, while the file
explorer arc was proving the app model. Every number below is labeled
measured, projected, or target — defend the slope, not the intercept.

## The idea

A machine that runs the Rill stack and nothing else: a trimmed Linux
kernel, musl userspace, Mesa for the one GPU shipped, libinput + xkbcommon,
a DHCP client and Wi-Fi supplicant, bundled fonts, and the Rill binaries.

The deletions are the point. No browser engine, no JS runtime, no GTK/Qt,
no dbus, no X11. The compositor *is* the display server; the protocol is
the only network surface; the policy model — identity-gated, denials
hidden as NOT_FOUND — is already built and tested.

## Why the numbers already lean this way

Measured (2026-08-09, debug builds, NVIDIA worst case — see
`docs/memory-footprint.md`, remeasure with `scripts/measure-usage.sh`):

* Each app: **4–8MB PSS**. This is the slope, and it is measured.
* Compositor's own working data: **~33MB**.
* The dominant cost is the *graphics driver* — 161MB of NVIDIA userspace —
  and the driver is the one component an image gets to choose. On Mesa
  hardware (AMD/Intel iGPU, Pi) RADV/V3D userspace is tens of MB and ACO
  removes the LLVM dependency. (Projection.)

Projected image: 200–400MB disk, boot-to-desktop in the low hundreds of MB
RAM on Mesa hardware. No tighter number until one is built.

## The payoff: the computer stops being a place

The thesis, in one sentence: **personal computing as an identity plus a
set of semantic streams, with devices reduced to glass.**

Two inversions carry it. Every current system makes each device a full
computer and then fights to synchronize them; here there is exactly one
locus of state, so seamlessness is not a feature — it is what remains when
the thing that made devices diverge is removed. And every current system
splits the screen across five industries — display protocols, screen
recording, accessibility APIs, automation tooling, agent frameworks — that
here are one format, because one semantic stream feeds every consumer:
GPU, disk, diff, agent.

The appliance holds no state. Apps are servers; per-identity view state
already lives server-side (the explorer's selection, sort and filter work
that way today); every window is a command stream measured in kilobytes.
"Your computer" reduces to **an identity plus a set of streams**, and the
box on the desk is glass.

What that buys, concretely:

* **Session portability pixels cannot have.** Open another Rill machine
  and the same session is there — not a video of it, the thing itself,
  re-rendered crisp for that screen's size and DPI, because reflow is free
  when frames are semantic. VNC ships a photograph of a desktop; this
  ships the desktop, at ~2KB a frame, over a phone tether. The relay
  subscription is not an add-on to this — it *is* this: what carries your
  streams to whatever glass you are in front of.
* **Yanking the power cord is a non-event.** Nothing local was the truth.
  Backup, sync, migration and new-machine setup collapse into: sign in.
  A $30 board and a $3000 workstation are the same computer at different
  frame rates.
* **Your day is a file.** A full workday of everything on screen is
  megabytes of `.rillrec` — scrubbable, perfectly replayable, auditable.
* **The machine is agent-legible.** An LLM reads or drives the whole
  desktop through the same semantic streams the compositor renders — no
  screenshots, no OCR. Recording plus agency on a high-security terminal
  is not a feature combination; it falls out of the wire format.

Nobody else can build this cheaply, because every other desktop is pixels
at the boundary. This one is meaning at the boundary — the
`architecture-advantages.md` argument taken to its conclusion.

## The blocker that is ours

rill-shell is gpui: the one heavyweight pixel process left (**~101MB heap
measured**, and the whole gpui dependency tree in the image). For the
appliance, the shell must become what everything else already is — a
rill-vector client drawing the dock as a command stream. Then the
compositor is the only process that touches pixels, gpui leaves the base
image entirely, and the stack is uniform top to bottom. Worth doing on its
own merits before any image work starts.

## Honest limits

* **Media is where "just enough libs" erodes.** Images now reach vector
  windows (`attach_image`, specs/wgpu-renderer.md W4) — decoded by the client,
  which already had to, so the compositor gains no image codec and no
  credentials. Video and audio still have no story, and codecs remain the
  first large library the image cannot avoid. The appliance is honest as a
  secure terminal before it is honest as a media machine.
* **No browser is the pitch and the constraint.** The web is the escape
  hatch for everything an ecosystem lacks; an appliance without one must
  be sold as what it is — kiosk, work terminal, relay/backup box,
  distraction-free machine — not as a daily driver, until the app
  catalogue earns that.
* **The security claim is structural, not hardening:** one wire protocol
  with mutual identity, on a machine that cannot execute web content
  because nothing on it can parse it. Say it exactly that way.

## Ladder

1. **Shell as a vector client.** Dock becomes a command stream; gpui exits
   the base stack. (Also deletes the theme-IPC gap: one process fewer to
   re-skin.) — **DONE 2026-08-10**: the dock is `rill-vector --dock`
   (chromeless, app_id-pinned); wallpaper is compositor-painted
   (`[desktop] wallpaper`); rill-shell, rill-view, and rill-ui-gpui are
   deleted and gpui is out of Cargo.lock entirely.
2. **Session handoff on a LAN.** Two machines, one identity: close the lid
   on one, the session is on the other. Mostly plumbing that exists —
   streams reconnect, state is server-side. This is the demo that makes
   the whole idea legible in ten seconds.
3. **A real image.** Buildroot or Alpine-derived, one target (a Pi or a
   mini PC), measured boot RAM and cold-boot-to-desktop time. Only now do
   the projected numbers become measured ones.
4. **The relay in the loop.** Same handoff, but across networks, through
   the relay — the subscription made tangible.

## Form factors (idea log, 2026-08-11 — after the ladder, not on it)

A device whose only job is being Rill glass needs far less than a laptop
needs (no x86 thermals, no big storage, watts not tens of watts) — which
makes flat, custom "Rill terminal" hardware plausible in tiers:

* **Tier 0, buy it:** Pi 500 (Pi-5-in-a-keyboard, ~$90) + USB-C portable
  monitor + PD battery bank ≈ the whole portable-terminal experience with
  zero engineering. Kit devices (uConsole-class) are pre-made chassis.
* **Tier 1, solo-feasible:** CM5 on a custom carrier board — the module
  carries the hard silicon; the carrier is a KiCad project (PD sink,
  BQ-series battery management, DSI/eDP bridge) in cyberdeck territory.
* **Tier 2, a company:** hinged, polished laptop hardware. Mechanical is
  the hard 80%; explicitly out (risks.md #5) until revenue exists.

Sequencing: prove the software on stock hardware first; Tier 0 is the
rung-2 portable demo; Tier 1 is a justified adventure only after that;
first-party hardware is the post-revenue Home-Assistant-Green move
(business-direction note).

### Glass classes (idea log, 2026-08-11)

One semantic stream, rasterized per device class — each class is a
*presentation tier*, not a port:

* **Interactive desktop glass** — monitors, Pi-500-class terminals,
  classroom panels. Full DrawCommand desktop. (Education note: the wedge
  is student-data sovereignty + stateless devices + semantic session
  replay as pedagogy; the incumbent is the Chromebook ecosystem and the
  channel is slow — enter via self-hosting teachers, not districts.)
* **Handheld** — uConsole-class ultrawides; reflow makes hostile aspect
  ratios native; compact Metrics preset (14/6) is the whole port.
* **E-paper** — damage regions map to partial refresh; static = zero
  cost. Weeks on battery.
* **HUD / waveguide glasses** — text-first, tiny FOV: NOT a small
  desktop; a re-presentation of typed snapshots (bridge.md) as a few
  lines of state. Only cheap because the boundary carries meaning.
* **USB-C display glasses** (XREAL-class) are just monitors — pocket
  Pi + glasses + keyboard + LTE works today. Gotcha: Pi 5's USB-C has
  no DP alt-mode; video is micro-HDMI, so glasses need an HDMI adapter
  (or a CM5 carrier wiring DP). Privacy line worth keeping: glasses are
  the one display nobody else can see — private computing in public,
  over kilobyte frames, to a server you own.

### Small glass — MCUs are glass too (decided direction, 2026-08-16)

Decision (author decision): microcontrollers CAN be glass, and the possibility stays
alive. The earlier framing ("glass needs Vulkan, so MCUs are endpoints
only") drew the line in the wrong place — only *full-fidelity* glass needs
a GPU. Glass is a presentation tier (this section's premise), and the
ladder extends below e-paper/HUD all the way to a seven-segment display.

Why it holds structurally, not aspirationally: the stream codec, wire, and
protocol crates (rill-ui stream, rill-wire, rill-protocol) carry ZERO GPU
dependencies — the graphics stack is the measured bulk of the system
(compositor binary 14.9 MiB vs server 3.1; driver-dominated memory on every
measured box) and it is already severed from the semantics. The meaning is
kilobytes; meaning is what a humble display needs. A pixel protocol on a
seven-segment display is a category error; a semantic stream degrades
gracefully because it carries "meter reads 47", not a framebuffer.

Three tiers (A/B TARGET, unbuilt; C measured):

* **Tier A — semantic re-presentation.** Seven-segment / character LCD /
  LED ring + buttons. No rasterizer: subscribe to a few named values (or a
  typed snapshot per bridge.md), buttons map to declared actions. Needs
  only codec + embedded TLS; tens of KB RAM. A $3 part becomes an
  identity-pinned window into the session.
* **Tier B — DrawCommand-subset rasterizer.** Small SPI TFT / e-paper:
  rects, fills, bitmap-font text; no shaders/Backdrop/blur. Bounded no_std
  software renderer — small because the command vocabulary is. Damage
  regions map onto e-paper partial refresh: static content costs nothing.
  Shaping answered by server-side shaping or fixed glyphs, declared as a
  glass-class capability.
* **Tier C — full fidelity.** The measured desktop; Pi 4/5-class Vulkan
  floor per reference-device discipline (risks.md #5). Unchanged.

Two design implications, noted not decided: (1) **capability negotiation
per glass class** — a sink declares primitives/color depth/fonts/refresh
model and the stream side sends the right presentation (the compact
Metrics preset generalized); (2) Tiers A/B share **one no_std substrate**
(codec + embedded TLS) with the serve-only OEM endpoint — one audited
crate, two markets (a device that serves its own status page AND shows a
live desktop value).

Demo gate (risks.md #1): this leaves the idea log the day a bench MCU
shows one live value from the desktop session and one button fires a
declared action. Until then it is this note.

**Fused endpoint+glass device — theoretical part list (2026-08-19; all
figures THEORETICAL, none measured).** The device: an ESP32-S3-class
module reading sensors, serving GET/GET_IF/ACTION over mutual TLS to
enrolled devices, and feeding its own small display by rendering the
same document it serves — one UI definition at three fidelities
(on-device panel, desktop window, anyone's Tier-A gadget).

* Budget: TLS 1.3 session ~40–60 KB RAM (mbedTLS ~300 KB flash; ESP32
  has AES/SHA hardware), codec + buffers ~16 KB, Tier-B framebuffer
  150 KB in PSRAM (or ~2 KB line-buffered), documents pre-compiled at
  firmware build time (build.rs runs the KDL compiler; runtime splices
  live values and re-hashes — blake3 over 2 KB is microseconds at
  240 MHz). Total ≈ 150–250 KB RAM, <1 MB flash + fonts. 2–4 concurrent
  TLS clients (RAM-bound) — enough for a desktop plus a wall panel.
  BOM for module + 2.8" SPI TFT + sensor: **under $10**.
* Three properties fall out of the existing design: (1) **GET_IF is the
  perfect MCU verb** — unchanged sensor answers with a hash comparison,
  no body; the conditional-fetch machinery is the low-power serving
  story. (2) **The security model needs zero adaptation** — fingerprint
  enrollment, no CA bundle (embedded TLS's usual nightmare deleted), no
  vendor cloud; a stranger on the LAN sees nothing. (3) **The port may
  be cheaper than "no_std" suggests**: ESP32 under esp-idf gives Rust
  *std*, so the zero-GPU-dep codec crates may compile nearly unchanged —
  the surface is swapping tokio's accept loop for thread-per-connection
  and rustls for mbedTLS bindings.
* Honest gaps: server-side TLS 1.3 in pure embedded Rust is the thin
  part of the ecosystem (client-side embedded-tls is solid; the server
  role likely means mbedTLS C bindings); the protocol needs a
  **small-device profile** (negotiated-down chunk/payload caps — the
  same capability-negotiation conversation the glass classes need);
  enrollment on a buttonless device (fingerprint as a QR on the label +
  a physical pair button). Power: an always-listening Wi-Fi server
  can't deep-sleep — ~0.5 W typical; fine wall-powered (the OEM class),
  battery nodes want a push/wake model (another argument for the
  eventual subscribe verb, not a blocker).
* Note the gating split: the Tier-A *subscribe* side waits on semantic
  identity, but the *serving* half is gated on nothing but the port —
  "ESP32 serves a live sensor page to the desktop over mutual TLS" is
  the smallest slice that lights up the OEM story and touches none of
  the compositor.

### Education / closed-system deployment (idea log, 2026-08-11)

Projected seat economics (estimates, not quotes): Pi 500 $90 + reused or
refurb display vs Chromebook-1:1 true cost (~$400–550/seat over 4yr once
MDM licenses, GoGuardian-class monitoring SaaS ($5–12/student/yr),
charging carts ($50–100/seat), and battery/hinge breakage are counted).
Rill deletes those line items architecturally: no batteries → no carts;
stateless glass → repair = hand over another unit; fleet = content
hashes → no MDM; monitoring = the wire format → no SaaS. Rough TCO:
2–3× cheaper. BUT the incumbent wins on the web: curriculum tools and
*certified state testing* live there — so entry is not districts; it is
closed rooms and self-hosting teachers (labs, libraries, writing rooms,
homeschool co-ops, offline/developing-region schools).

**The closed system is the real pitch, not price:**
* Air-gapped by construction: devices speak only rill:// to enrolled
  servers; there is no browser to lock down — the image cannot parse web
  content. Internet on the LAN is irrelevant to the devices.
* Observability that is cheap AND proportionate: a teacher view tiling
  30 live student screens costs 30 × ~2KB frames, on-prem; .rillrec
  replay is text-searchable and megabytes/day. Semantic recording can be
  scoped (coursework apps, not private notes) with the capability log
  auditing the auditing — the anti-GoGuardian.
* Exam mode is the default state of the machine: sealed, deterministic,
  replayable — lockdown-browser vendors approximate this; here it is
  structural.

E-ink in classrooms, honest split: per-desk e-ink monitors fail on price
today ($650+ panels); classroom/hallway signage on 7–13" panels ($50–150)
works as soon as the bridge exists — same fleet story as kiosks. Desks
follow panel prices, not our roadmap.
