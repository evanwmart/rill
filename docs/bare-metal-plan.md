# Closing the metal ↔ compositor gap

Status: **plan, nothing built.** Written 2026-08-14 from reading the tree.
This is milestone 15 (the DRM/libinput backend) and the OS question that
usually gets tangled up with it.

The short version, because it is the part most likely to save time:

> **"A lightweight Linux" and "the compositor can drive a screen" are
> separate problems, and only the second one is the gap.** Build the DRM
> backend on a stock distro. An OS image is a packaging decision that comes
> after, and may never need to come at all.

Building a distro before the compositor can page-flip is backwards work: you
would be assembling a system with nothing to run on it.

## Where the gap actually is

`platform/rill-compositor/Cargo.toml` takes smithay with `backend_winit` only.
So Rill today needs a host compositor to nest inside — a window to present
into and someone else's input events. Everything *above* that is already
ours and backend-independent: the scene assembly, the stream protocol, the
policy model, the widgets, the renderer.

That is the good news. The gap is a seam near the bottom, not a layer
through the middle. Concretely, bare metal needs four things the winit
backend is currently providing for free:

```text
a surface to present to   → DRM/KMS: connectors, modes, framebuffers, page flip
input events              → libinput, plus the xkb handling we already have
device access             → libseat/seatd, so it need not run as root
device discovery/hotplug  → udev
```

smithay ships all four as features we are not yet enabling
(`backend_drm`, `backend_libinput`, `backend_session_libseat`,
`backend_udev`), and its `anvil` example wires exactly this combination.
This is a well-trodden path, not research.

## The one part that is genuinely ours

Everything else is smithay glue. The interesting question is **how a
wgpu-rendered frame gets onto a KMS plane**, because Rill does not use
smithay's renderer — it renders with wgpu and imports client buffers itself.

The usual smithay path (`DrmCompositor` over its own GL/Vulkan renderer)
does not apply. Ours is:

```text
wgpu renders the frame
  → export that texture's memory as a dmabuf fd
  → wrap the fd in a DRM framebuffer (gbm / drmModeAddFB2WithModifiers)
  → atomic commit / page flip
  → repeat, with two or three buffers in rotation
```

**The crux primitive already exists and is tested.** `rill-gpu`'s
`DmabufDevice::alloc_exported` allocates a linear image with exportable
memory and hands back its dmabuf fd plus stride/offset
(`get_memory_fd`, `VK_EXT_image_drm_format_modifier`), and
`export_then_import_round_trips_pixels` proves the fd carries real pixels.
It was written to test *import* without a live client; it is the same
machinery a KMS present path needs, pointed the other way.

So the hardest-sounding part of milestone 15 — getting our own renderer's
output onto real hardware — starts from a working, tested primitive rather
than from zero. What is missing around it is buffer rotation (render to
whichever of N exported images is not on screen), and the flip bookkeeping.

## Shape: one binary, two backends

Not two binaries, and not a grand abstraction layer.

```text
rill-compositor --backend winit   (default: nested, what exists)
rill-compositor --backend drm     (new: the metal)
```

* **Keep the winit backend forever.** It is how development happens, how the
  benchmark produces comparable numbers, and how Rill runs *inside* someone
  else's desktop — which is a legitimate way to use it, not just a stepping
  stone.
* **Resist a `DisplayBackend` trait until there are two real implementations
  to factor.** Designing the abstraction first is how a seam becomes a
  layer. Let the DRM backend be written concretely and duplicative, then
  extract what genuinely repeats.
* Feature-gate the DRM backend so a nested-only build stays buildable on a
  machine without libseat/libinput headers — the development box is one.

## The ladder, demo-gated

Each rung has to show something, per risks.md #1.

### 15a — it lights up
DRM/KMS only. Open the card via libseat, pick the connector's preferred
mode, allocate exported buffers, page-flip a rendered frame. No input, one
output, no hotplug.

*Demo:* a Pi with no desktop under it, showing the Rill wallpaper.
*Proves:* the export → framebuffer → flip path works on real hardware.

### 15b — it is usable
libinput + udev: keyboard, pointer, the seat plumbing. Session pause/resume
so VT switching does not kill it — this is the classic place bare-metal
compositors die, because DRM master is dropped and device fds are revoked
when you switch away, and everything must be reacquired on resume.

*Demo:* use the desktop on the metal — open apps, drag windows, type.
*Proves:* it is a compositor, not a slideshow.

### 15c — it is a session
An autologin unit that starts Rill instead of a desktop. Still a stock
distro underneath.

*Demo:* power on → Rill, nothing else on the screen ever.
*Proves:* the appliance experience, without an appliance.
*Unlocks:* `boot_to_shell_ms`, which `bench-device.sh` currently reports as
null because it is genuinely not measurable while nested.

### 15d — it is an image
Only if something needs it: an OEM, a product, a fleet. See below.

Rungs 15a–15c are the whole user-visible payoff. 15d is packaging.

## On "lightweight Linux"

The honest ranking, given one developer and risks.md #5 (support named
reference devices extremely well, expand deliberately):

**1. Pi OS Lite / Debian minimal + a systemd unit.** No desktop, no browser,
Rill as the session. Perhaps a day's work once 15b exists. Gets you every
user-visible property of the appliance — boots into Rill, nothing else
running, no web content parseable anywhere on the machine — for almost no
engineering. **This is what I would do, and possibly all I would ever do.**

**2. Alpine + musl.** Meaningfully smaller, still a real package manager,
openrc instead of systemd. Rust on musl is fine; Mesa is packaged. A
sensible middle if image size starts to matter.

**3. Buildroot.** A genuine appliance image, tens of MB, reproducible,
nothing you did not choose. Also a real project: kernel config, Mesa, the
wayland libs, seatd, fonts, an update story. Worth it when there is a
*reason* — hardware to ship, a fleet to manage, a size budget someone is
paying for.

**Not Yocto, not NixOS.** Yocto is heavier process than a solo project can
carry; NixOS is reproducible but not embedded-shaped and would not shrink
the result.

The thing to avoid is starting at 3. `specs/appliance.md` already puts "a
real image" at ladder rung 3 with the note that only then do projected
numbers become measured ones — but rung 15c gets most of that benefit, and
the measurement that matters (idle footprint, slope, boot time) is
available there.

## Reference distro (decided 2026-08-19)

**Debian (stable / minimal) is the named reference for both doors** — the
`rill-session` package and the appliance base. The author's call, over an
Arch-as-desktop-reference proposal, on three grounds:

* **Trust alignment.** The project's brand is inspectability; the
  reference install path must be reproducible from vetted sources. The
  AUR is user-submitted and unvetted, with recurring malware incidents —
  fine as a community channel, off-brand as the recommended path for a
  security-positioned project.
* **Already proven here.** The Pi reference device runs Debian 13; the
  dependency list in pi-bring-up.md is Debian's; the appliance rank-1
  pick above was Debian-family already. One base for both doors halves
  the support surface — risks.md #5 applied to distros.
* **Operational experience.** Prior daily-driver friction on Arch
  (barriers in routine functions) argues against making it the surface
  this one-person project promises to keep working.

The supporting cast, so nobody re-litigates it piecemeal:

* **Ubuntu LTS** — build-verification bracket, not a reference. Same
  family as Debian (cheap to cover), *oldest* packaged deps, so it
  catches version-floor surprises before users do.
* **Arch** — community channel. An AUR package is welcomed and probably
  inevitable (it is where the r/LinuxPorn audience lives), maintained by
  the community, never the documented reference path.
* **Fedora** — deferred stress test. The most aggressively
  Wayland-forward distro (portals, PipeWire, no-X11 defaults); the
  harshest coexistence test for rill-session *when the foreign-app
  desktop arc gets serious*, not a launch target.

### openSUSE — the first-party platform (added 2026-08-19)

Distinct from the published reference: **Debian is what strangers are
told; openSUSE is what the project itself runs on.** Terminology, since
it decides which product fits which role: Tumbleweed is *rolling*;
Leap is the stable/LTS-shaped one; **MicroOS** is the minimal immutable
one (transactional updates, btrfs rollback — no spam apps by
construction).

* **Tumbleweed = dev reference and rolling canary.** Every line of Rill
  is developed and demoed on it daily, making it the most-tested
  platform in the project and a better fresh-deps early-warning than any
  CI container of a distro nobody here uses — breakage against new
  Mesa/wayland surfaces on the actual dev box. This replaces the earlier
  idea of an Arch canary container. Known papercut: the xkbcommon
  unversioned-.so link shim (build-time only).
* **MicroOS = appliance-base candidate, gated on one Pi trial.** It is
  the most on-pattern base available: immutable root + transactional
  updates + automatic rollback = the SteamOS/HAOS A/B pattern provided
  by the OS for free. The gate is Pi support — Debian's Pi path
  (firmware, kernel, V3DV packaging) is battle-worn and proven in this
  repo; MicroOS-on-Pi is real but less trodden. If a trial boots the
  reference Pi cleanly with working V3DV, rung 15c gets attempted on
  both bases and rollback-for-free becomes a serious argument.
* **OBS (Open Build Service) = packaging-infra candidate, independent of
  the reference choice.** One source spec building packages for Debian,
  Ubuntu, Fedora, and openSUSE, hosted free; KIWI for appliance images.
  Whatever distro users are told to run, the *pipeline* that builds
  their packages can be OBS and cover every named target at once.

Category note, recorded because the comparison recurs: Rill is
**DE-shaped, not WM-shaped** — the right category comparison is GNOME
(compositor + shell + session services + toolkit + apps), not sway/niri
(compositor only). In session-integration *maturity* it is currently a
young sway: the finite gap list for "session on a distro" beyond
milestone 15 is a portal backend for foreign apps
(`xdg-desktop-portal-rill`, cribbed from wlroots' portal), notifications,
and settings surfaces (which Rill does as native apps anyway). Ambition
GNOME, plumbing sway-stage, packaging playbook borrowed from the
compositor community.

## On "isolated"

Worth separating two meanings, because Rill's answers differ sharply.

**A minimal system** — few packages, small attack surface. Mostly a
by-product of the above. The genuinely load-bearing property is already
structural and true today: **the machine cannot parse web content**, because
nothing on it can. That is worth far more than hardening, and it is not
something a distro choice gives or takes away.

**Sandboxing what runs** — and here the model inverts the usual advice.
On a normal desktop you isolate *applications*, because applications are
code. In Rill, a client is a document renderer that executes nothing an app
sent it; the thing that runs code is the **server**. So isolation effort
belongs on the server side, not the client side:

* app handlers are in-process today (`AppHandler` runs on the connection
  task) — process-per-app with a small IPC seam is the obvious hardening,
  and it is a *server* change; **when** to spend it is now recorded as
  "isolation follows exposure" (specs/security.md §11): sharing an app to
  a less-trusted identity is the trigger that promotes its handler out of
  the shared process;
* systemd unit hardening for the server (`ProtectSystem`, `PrivateTmp`,
  `NoNewPrivileges`, a dedicated user) is cheap and immediate;
* the terminal app is the honest exception and should be labelled as such —
  `/term/**` is a shell, so granting it is granting the machine, which
  `demo-desktop.sh` already says in a comment.

Client-side isolation is close to wasted effort: there is no foreign code
there to contain. Saying that clearly is better than performing security
theatre on the half that is already inert.

## Risks, in the order I would worry about them

**Session/VT handling.** More compositors are broken by device revocation on
VT switch than by anything in the render path. Budget real time for pause →
resume, and test it deliberately rather than discovering it.

**Buffer rotation and tearing.** A single exported buffer will tear or
stall. Two or three, with flip completion tracked, is the minimum. This
interacts with the damage gate — which is an asset here: a compositor that
already knows when nothing changed can simply not flip.

**V3D's Vulkan on a Pi.** Same five extensions
[pi-bring-up.md](pi-bring-up.md) opens with. If the export side is missing
on that driver, 15a is blocked on that Pi specifically — worth checking
before choosing reference hardware.

**Scope.** "Bare metal" invites GPU support, hotplug, multi-monitor,
rotation, touch, audio, Wi-Fi setup, an updater. Rung 15a is *one output, no
hotplug, no input*. Keep it there until it flips a frame.

## What I would actually do next

Not now — this is after the video, and probably after the Pi measurement,
because measuring the nested desktop on real Mesa hardware is cheaper and
answers a question that is currently blocking claims.

```text
1. Check the five Vulkan extensions on the chosen reference device.   ← gate
2. 15a on that device: one output, one frame, no input.
3. 15b: libinput + seat + VT switching that survives.
4. 15c: autologin unit on a stock lite distro. Measure boot-to-Rill.
5. Stop. Re-read specs/appliance.md and decide whether 15d has a reason yet.
```

Step 5 is deliberate. By then the appliance's user-visible promises are all
delivered, and an image becomes a business decision — an OEM, a fleet, a
size budget — rather than an engineering itch.

## Progress

### Phase 0 — workstation-buildable groundwork (2026-09-02)

Everything 15a needs that does *not* require the reference Pi's screen, so
the hands-on Pi work is pure hardware bring-up. All on the dev box, both
feature configs green (clippy + tests).

* **`rill_gpu::dmabuf::alloc_scanout`** (commit 70f9e4b) — the crux
  primitive, the other direction of `alloc_exported`: one linear image
  kept alive as a wgpu render target *and* handed out as a dmabuf fd for
  `drmModeAddFB2WithModifiers`. Render-to-linear capability is queried up
  front and refused cleanly rather than exploding at create time.
  * **Found by building it:** present has two modes, and drivers disagree.
    *Copy-present* (render/write offscreen, copy into the scanout image)
    works everywhere the import path works and is proven byte-for-byte
    through a raw dmabuf `mmap` — the display controller's own view of the
    memory. *Render-present* (a render pass straight into the scanout
    image) is faster but NVIDIA's proprietary driver claims linear render
    support and then **wedges its queue** on it (the tests poll with a
    deadline and record a skip instead of hanging). The reference V3DV is
    where render-present must genuinely pass; until then the DRM backend
    fills through the copy path, which is measured-good.

* **`--backend winit|drm` + `drm` feature** (this commit) — one binary,
  two backends, per the shape above. The flag is stripped before client
  parsing; `--backend drm` without the feature is a clean error, not a
  missing symbol. Nested builds carry none of the DRM crate.

* **`drm_backend.rs`, the 15a light-up diagnostic** — opens
  `/dev/dri/card*` (preferring the card with a *connected* connector —
  this multi-GPU box has four dark connectors on the discrete card),
  reports every connector and mode, then two-buffer color-flips the
  chosen output with legacy modeset + `page_flip`, waiting on each
  flip-done event. Uses the drm crate's pure-Rust ioctls — no C headers,
  so this is not the thing gating the nested build; the feature gate is
  about keeping nested *lean*, not about compilability.
  * **Degrade-and-wait is in from line one** (the appliance requirement
    filed from the 2026-08-24 soak launch, where an absent display was a
    wgpu panic three layers down): no connected connector is a backoff
    loop, not a death — and the loop honors SIGTERM, because the first
    smoke run produced an unkillable spin-waiting service, which is its
    own class of appliance bug. Verified: clean exit 0 on `kill -TERM`.

**What Phase 0 could NOT prove here, by construction:** the actual flip.
This workstation's running GNOME session owns the display through the
NVIDIA proprietary path, so every generic-DRM connector reads
disconnected — the diagnostic and the wait loop exercised, the modeset
and flip cycle did not. That half is 15a proper, and it belongs to the
Pi where the process owns its VT. Everything above it is done and green.

### Phase 0.1 — the real renderer reaches the scanout buffer (2026-09-02)

The unknown between "flip solid colors" (above) and "flip a composited
desktop" (15a's actual goal): can the production `GpuRenderer` — not a
`write_texture` fill — compose a scene *into* an `alloc_scanout` image?
Answered yes, on the workstation, no screen needed:

* **`scanout_composites_a_real_scene`** (rill-gpu test) — a Renderer built
  at the scanout format (BGRA, non-sRGB, the winit path's own choice) on
  the dmabuf device composites a `DrawCommand` scene into that device's
  exported image via the copy path, and a readback proves the rect and
  clear landed in the buffer KMS scans out. This is the whole compose→
  present pipeline minus the flip, byte-verified.
  * Its second value: it made the suite's device-creation serialization
    (the NVIDIA concurrent-create deadlock guard, 2026-08-30) a shared
    `gpu_serial()` the dmabuf tests hold too, rather than a mutex hidden
    inside the headless `renderer()` helper.

* **`drm_backend` now renders a real frame**, not a solid fill: one
  `GpuRenderer` on the export device, a `composite_scene` of a centered
  card + accent bar into an offscreen target, copied into whichever
  scanout buffer is off-screen — the exact path the test pins. The two
  rotation buffers place the accent bar differently, so the flip on the
  Pi will read as live motion, not a frozen frame. 15a's scope holds:
  one output, no clients; the scene is a rendered stand-in for the
  wallpaper, and hosting Wayland clients behind this is the next rung.

Still Pi-only: the framebuffer import and the flip. But the pixels that
will land on the glass are now produced by the real renderer and proven
correct in memory here.

### Phase 0.2 — on the Pi: the import holds, the flip waits for master (2026-09-08)

The plan's step 1 gate is passed, MEASURED with `vulkaninfo` on the
reference Pi 5 (V3DV Mesa 25.0.7, API 1.3.305): `VK_EXT_external_memory_dma_buf`,
`VK_EXT_image_drm_format_modifier` (rev 2), `VK_KHR_external_memory_fd`,
`VK_EXT_physical_device_drm`, `VK_KHR_external_semaphore_fd` — all present.

`rill-compositor --backend drm` (cross-built with `--features drm`, run
as the ordinary user over SSH while the Pi OS desktop session was still
up) got this far on real hardware:

```text
wgpu on V3D 7.1.10.2
/dev/dri/card1                       ← the vc4 KMS card, not v3d's render-only card0
  HDMI-A-1 connected, 6 modes        ← the forced video= mode, no panel attached
  HDMI-A-2 disconnected, 0 modes
lighting HDMI-A-1 at 1280x800@60
  scanout buffer 0 imported as fb framebuffer::Handle(685) (modifier 0x0)
  scanout buffer 1 imported as fb framebuffer::Handle(686) (modifier 0x0)
Error: modeset refused: this process is not DRM master — another compositor
       (a desktop session) owns the card; stop it, or run from a VT this process owns
```

So the two things Phase 0 could not prove on the workstation split: the
**framebuffer import is proven** — `alloc_scanout` on V3DV → dmabuf fd →
`PRIME_FD_TO_HANDLE` → `ADDFB2` with the linear modifier, accepted by
vc4 — and the **flip is not yet**, only because labwc held master. The
non-master run was worth doing on purpose: `ADDFB2` and the PRIME import
are unprivileged ioctls, so everything short of the modeset can be
exercised without touching the session, and the backend now names each
stage on its own line and turns the modeset's bare `EACCES` into a
sentence (both added after the first run stopped at an unlabeled
permission error).

Remaining for 15a: stop lightdm (`sudo systemctl stop lightdm`, which
takes labwc and wayvnc with it), rerun, and count flip-done events. With
no panel on the forced output the demo is the log line, not a picture —
the wallpaper on glass wants the TV plugged back in for the photo.

### 15a — it lights up: HOLDS on the Pi (2026-09-08, later the same day)

With lightdm stopped (`sudo systemctl stop lightdm`, labwc and wayvnc
gone with it), the same binary still refused the modeset. Nothing else
held the card; **the process itself did.** DRM master goes to the first
file that opens the primary node while no master exists, and V3DV opens
the display card when the Vulkan device comes up — so the Vulkan fd was
master and the backend's own fd was refused. Fix: open the card *before*
creating the GPU device, then `SET_MASTER` explicitly and log the answer
(a refusal is a line, not a death, since the first-open grant may already
have happened). Two lines of reordering; a whole rung of difference.

```text
/dev/dri/card1
  HDMI-A-1 connected, 6 modes
DRM master acquired
wgpu on V3D 7.1.10.2
lighting HDMI-A-1 at 1280x800@60
  scanout buffer 0 imported as fb framebuffer::Handle(680) (modifier 0x0)
  scanout buffer 1 imported as fb framebuffer::Handle(681) (modifier 0x0)
modeset up — flipping
80 flips clean — 15a light-up holds          (40 s run; a 10 s run: 20 flips)
```

MEASURED alongside, 20 s into the 40 s run, Pi 5 1 GB, no clients, no
desktop session underneath:

```text
PSS 58.3 MiB   VmRSS 60.5   VmHWM 61.0   threads 3   47.7 °C
connector: connected + enabled (sysfs)   kernel log: only the HDMI-audio
"Unknown ELD version 0" chatter a forced mode with no EDID always produces
```

That 58 MiB is the bare-metal floor for *this* build — V3DV + wgpu + the
renderer + two 1280x800 scanout images + one offscreen target — and it is
not comparable to the nested figures (28–66 MiB) in either direction: no
Wayland frontend, no clients, no history recorder, but also a Vulkan
instance the nested path shares with the host. The number that matters
arrives with 15c, when the same process hosts the desktop.

One observation, recorded rather than explained: `CmaFree` was 64 KiB
at idle (of 65,536) and 160 KiB during the run, and the run did not
care — the scanout images come from V3DV's own allocator, not CMA, and
vc4 imported and scanned them out with the pool empty. The 2026-08-24
soak launch found CMA starvation fatal *through the nested path*
(`Surface::configure` → V3DV swapchain → CmaFree 0); the DRM path did
not hit that, and the appliance-profile `cma=128M` item stays filed as
headroom, not as a 15a blocker.

*15a's demo, per the ladder: "a Pi with no desktop under it, showing the
Rill wallpaper."* The scene flipped is the renderer's stand-in card with
its accent bar alternating between two buffers; with no panel on the
forced output the evidence is the flip-done count and the connector's
`enabled`, and the photograph waits for the TV. 15b (libinput, seat, VT
switching that survives) is next on the ladder.

### 15b — it is usable: the hosted desktop composites and presents on the metal (2026-09-08)

The DRM backend grew from a light-up diagnostic into the real thing: the
same `main` loop drives both backends, matched at five seams (pump, size,
acquire, present, budget) and nowhere else. `--backend drm` now hosts
Wayland clients; `--backend drm-lightup` keeps the standalone diagnostic.

**MEASURED on the reference Pi 5, over SSH, lightdm stopped:** the dock
and one meter widget ran as clients of the compositor on bare vc4/KMS —
no labwc, no host compositor. Screenshot captured by a new `SIGUSR2`
readback path (the forced HDMI output has no panel; a kiosk in the field
has no one at it), pulled to the workstation:

* Dock strip with the Rill logo and live clock, the meter widget window
  drawn with its focus glow, and — with the demo server up — the gauges
  streaming real values (mem 327M/991M, disk 9.3G/28.7G, load 0.00). The
  whole path proven: wgpu composite → dmabuf export → KMS framebuffer
  import → page flip, hosting a real vector client.
* Compositor PSS ~61 MiB (up from 15a's 58 with no clients; the two
  vector clients add ~5 MiB each). frames tracked the workload.
* **libinput came up on the seat:** `udev_assign_seat(seat0)` succeeded
  and the device-added events enumerated the board's input nodes
  (`pwr_button`, the two HDMI audio jacks). No HID was attached to this
  bench Pi, so no keypress or pointer motion was driven through — the
  translation compiles and the pipeline is live, the events had nothing
  to carry.

**How device access resolves, MEASURED:** libinput and DRM nodes open
through libseat when a seat is active, and fall back to direct opens when
it is not. An SSH login is a session that never becomes the active one on
seat0, so this run took the direct path and said so
(`seat seat0 never became active … opening devices directly; no VT
switching on this run`). That is the honest state: **the seat
pause/resume and VT-switch survival code paths exist but are UNTESTED**,
because testing them needs a console session (the active seat) that an
SSH run is not. The card-before-Vulkan master ordering from 15a still
holds and is now inside `Metal::open`.

**What 15b still owes, and why it is a hands-on/console task:**

* Input actually driving the desktop — needs either a physical keyboard
  and mouse on the Pi, or root access to `/dev/uinput` to inject a
  synthetic device (the node is `root:root`, so either needs privilege
  this SSH user does not have).
* VT-switch pause/resume survival — needs a real logind seat, i.e. a
  launch from the console (tty1 autologin is already configured), not
  SSH. This is the classic place bare-metal compositors die (risks.md),
  and it is written but unproven.

Both fold naturally into 15c (the autologin session *is* the console
seat), so the honest ladder position is: 15b's rendering half is done and
photographed; 15b's input/seat half is written and waits for a console
run, which 15c sets up anyway.

**On a real panel, 2026-09-08:** the forced 1280x800 output was plugged
into a physical TV and the desktop came up on the glass — clock ticking,
the meter widget's gauges updating live (photo:
`bench-results/2026-09-08_metal/on-panel-tv.jpg`). This retires the "a
screen nobody can see" caveat for the rendering half: 15a's demo goal
(*"a Pi with no desktop under it, showing the Rill desktop"*) is now
literally photographed, and the readback path (`SIGUSR2`) and the panel
agree. The TV accepted the forced mode directly, so the mode-mismatch
worry did not bite this display. Still unproven, unchanged: input driving
it and VT-switch survival.

### 15b/15c prep — the session service and the input harness (2026-09-08)

Everything the remaining 15b/15c work needs that does *not* require a
privileged action is built and staged; what remains is one reviewed `sudo`
run, after which input and VT-switch survival are testable over SSH.

* **Zombie reaped.** The V3DV driver forks a helper at startup it never
  waits on — one stable zombie, measured across four hours on the Pi, not
  accumulating. The compositor loop now calls a `WNOHANG` reaper each
  frame (`reap_children`), correct hardening for a session that runs for
  weeks whatever the fork's origin. Safe because the compositor never
  `wait`s on a child it tracks (parec and the clients are both
  fire-and-forget).
* **`deploy/pi/rill-session.{sh,service}`** — the 15c kiosk session.
  The service is modeled on cage/greetd: `PAMName=login` + `TTYPath=/dev/tty1`
  is what makes logind hand it a *real* session on seat0, which is the
  whole point — the DRM/libseat path only becomes active, and only
  survives VT switches, inside such a session. It `Conflicts` lightdm
  (one owner of the card) and restarts on failure.
* **`deploy/pi/99-rill-uinput.rules`** opens `/dev/uinput` to the `input`
  group, and **`deploy/pi/uinput-inject.py`** (stdlib only) creates a
  virtual keyboard + pointer and plays a scripted sequence — so 15b's
  "input drives the desktop" can be proven over SSH with a screenshot,
  no physical HID and no one at the Pi. `demo` walks to the dock logo,
  clicks it (launcher menu), then taps Ctrl+Shift+R (rice cycle) — both
  visible in a readback.
* **`deploy/pi/install-15c.sh`** batches the privileged half into one
  reviewed script: install the service, open uinput, disable lightdm,
  enable rill-session. It deliberately does not auto-start (a second
  compositor would fight for the card); the operator starts it or reboots.

**The one gate, and the test plan behind it.** Run `sudo bash
~/deploy/install-15c.sh` on the Pi, then `sudo systemctl start
rill-session`. That gives Rill the active seat0 session on tty1 — no
reboot needed. Then, over SSH:

1. `chvt 2 && sleep 2 && chvt 1` — the compositor should log `seat
   disabled — pausing` then `seat is ours again — resuming`, re-modeset,
   and a screenshot after should match before. **This is the risk the
   whole milestone was written around** (risks.md: device revocation on
   VT switch), and it is the first thing the real seat lets us test.
2. `~/deploy/uinput-inject.py demo`, screenshot before/after — the
   launcher menu open and the rice changed is input driving the desktop.

Only after those two pass is 15b's input/seat half MEASURED rather than
merely written; 15c's boot-to-Rill (`boot_to_shell_ms`) follows from the
same service via a reboot.

### 15b on the seat — input PROVEN, VT-switch not yet exercised (2026-09-08→09)

`rill-session.service` started and logind put it on a real session:
session 520, seat0, tty1, compositor as leader. It came up at the TV's
native **3840x2160** (the service re-probed the connector and took the
EDID's preferred mode over the forced 1280x800). RSS ~62 MiB at 4K.

**Input is proven.** `sudo test-15b.sh` loaded uinput, and the injector's
pointer move put the cursor mid-frame in the readback (`15b-2-pointer.png`)
— a virtual device over `/dev/uinput` → libinput → the compositor → the
drawn cursor. The libinput translation works on real hardware.

**The seat finding.** The session shows **Active=no**: the foreground
console is VT7 (a leftover from the two weeks lightdm ran there), while
the compositor's session is on VT1, in the background. So libseat's
`Enable` never fired, `Session::open` hit its 5 s timeout and fell back to
**Direct opens + a forced `SET_MASTER`**, and the compositor is scanning
out by brute force rather than cooperating with the seat. That is why the
`chvt 2 → back` test logged no pause/resume: it toggled VT7↔VT2 and never
touched the compositor's VT1, and a force-master compositor ignores VT
state anyway. It *survived* (still painting after), but the pause/resume
path was not run.

Two consequences, recorded:
* **VT-switch survival is still unproven**, and the environment to prove
  it is a clean boot: with lightdm disabled, VT1 is the foreground console
  at boot, the session goes Active, libseat `Enable` fires, and the
  compositor runs in seat mode where `chvt` actually pauses and resumes
  it. That boot *is* 15c, so 15b's last piece and 15c's deliverable are
  the same reboot.
* **A robustness nit for the kiosk** (filed, not yet fixed): starting the
  compositor while its VT is in the background makes it force-grab master
  in Direct mode instead of waiting for the seat. Right on a boot (VT1 is
  foreground); wrong on a mid-session start. Worth making the seat path
  prefer waiting for `Enable` over the Direct fallback when a seat exists.


### 15b/15c status, honestly (2026-09-09)

**15c — DONE, MEASURED.** The Pi boots straight into Rill: `rill-session.service`
on `graphical.target`, lightdm disabled, no display manager. The booted
compositor is the leader of an `Active=yes` session on seat0/VT1, at the TV's
native 3840x2160 (EDID preferred over the forced mode), ~62 MiB RSS, running a
live animated background rice. Auto-starts on power-on.

**VT-switch — SURVIVES, MEASURED (mechanism not logged).** `chvt 2 → chvt 1`
against the booted compositor: it stayed alive and kept rendering — the clock
advanced 11:19→11:23 and the animation progressed across the switch
(`bench-results/2026-09-08_metal/15b/vt-{0-before,1-after}.png`). It did not
die on device revocation, which is the milestone's bar. Whether it survived by
the seat's pause/resume path or by force-holding DRM master was NOT captured,
because the compositor's stdout goes to the tty (not the journal) and this
run's Active check read the wrong (stale) session.

**Input — PROVEN in Direct mode, UNCONFIRMED in the booted seat config.**
The earlier Direct-mode run (pre-boot) moved the cursor to mid-screen from an
injected uinput pointer — the libinput translation works
(`15b-2-pointer.png`). But in the *booted* seat-mode config, injected pointer
motion did NOT visibly move the cursor in two tries (`vt-2-pointer.png`,
recheck). Either input is not reaching the compositor in seat mode (a real
bug: hotplugged uinput device opened via libseat), or the cursor is not being
drawn over this fullscreen-shader rice. Cannot tell without the compositor's
log.

**Two harness lessons banked:** a `>> logfile` redirect on the exec'd
compositor crash-looped the service 455× (reverted, self-heals since the
service re-reads the script); a mid-life `systemctl restart` does not reliably
re-enter the active seat, only a boot does.

**The clean way to finish (needs one privileged pass):** make the compositor
log to the journal — set `StandardOutput=journal`/`StandardError=journal` to
actually capture stdout (or a robust logfile that cannot fail the exec) — then
one boot, and a corrected test that (a) selects the session whose Leader is the
compositor's MainPID, and (b) reads the compositor's own `seat enabled/disabled`
and `input device added` lines. That resolves both open questions —
seat-mode input and the VT pause/resume mechanism — in a single run.
