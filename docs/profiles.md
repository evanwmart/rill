# Profiles — one stack, three shapes

Decided 2026-08-31. Rill ships as three *profiles* of the same binaries,
not three editions. The difference between profiles is what launches at
boot and what the package pulls in — never a feature gate, never a
separate build. One test surface, one release, three configurations.

Each profile contains the one below it.

## server

The minimal collector. `files-app` (or another rill server) plus the
history recorder, headless, bound to the trust boundary's interface.
Collects, records, serves documents; draws nothing.

Display is *attachable*, not resident: any `rill-vector --widget` or
viewer pointed at a `rill://` URL is the display, on demand, from any
enrolled device, and goes away without the server noticing. "Local
display of live data" costs a client process, not a permanent stack.

Footprint: 16 MiB idle (MEASURED, 2026-08 arena fixes; see
memory-footprint.md). Runs anywhere the binary does.

## glass

A screen that boots into a document. server + `rill-compositor` +
pinned widget(s) — no dock, no launcher, no session UI. The wall
panel, the shop dashboard, the kiosk: power on, glass shows the
document, nothing else exists to misconfigure or exploit.

This is the shipped soak workload minus the nested session: the
2026-08 Pi run (compositor + one 1 Hz meter, 7 days, docs/pi-soak.md)
is a glass profile in rehearsal.

**Bare-metal targets glass first.** DRM + compositor + one pinned
document is the smallest thing that can boot to glass — no input
complexity, no session management — and the desktop session rides the
same plumbing afterward. The appliance image ships server + glass.

## desktop

The full session: compositor as the Wayland session, dock, app suite,
enrollment screens. Bare-metal on appliance hardware, or a session
entry beside the others at a distro's login screen (a greetd/SDDM
session file, not a display-manager replacement). Debian first —
the appliance world is already Debian 13; other distros follow use,
not ambition.

Footprint: 28–34 MiB PSS idle, whole desktop, on a 1 GB Pi 5
(MEASURED 2026-08-15).

## Why profiles and not editions

The trap this document exists to avoid: "editions" grow feature flags,
then divergent builds, then a test matrix. A profile is a launch
config and a package manifest over the one release. If a capability
ever seems to belong to only one profile, the question is not "which
edition gets it" but "why is this not attachable" — the server/glass
split above shows the pattern: glass is server plus processes, not
server plus code.
