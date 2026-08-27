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
