# Rill Compositor — Design & Decisions (Group Four, milestone 14+)

Status: **in design** (Aug 2026). `rill-compositor` is Desktop Phase 2 — a
nested Wayland compositor, growing later into the standalone session
(milestone 15) and bootable image (milestone 16).

Related: [application-model.md](application-model.md),
[compute-apps.md](compute-apps.md), [theming.md](theming.md).

---

## Goal (milestone 14)

A minimal Wayland compositor that runs **nested** (as a window inside the
current desktop). Exit condition: a Rill application and an ordinary Wayland
application run together inside it.

---

## Decisions

### D1 — Architecture: everything is a Wayland client (Option A)

The compositor is the foundation. **Rill apps run as separate `rill-view`
processes** — ordinary Wayland clients the compositor composites, exactly like
a terminal. The shell/dock becomes a privileged client (wallpaper + dock),
not the window host.

* **Why:** real per-app OS process isolation — the honest "native apps without
  Electron" answer. Isolation comes from the OS + capability mediation, not a
  sandbox inside one shared runtime. Uniform architecture (everything's a
  client). Matches Rill's deny-by-default security posture.
* **Cost / consequence:** the current *in-process* `AppView` hosting in
  `rill-shell` is retired in favour of out-of-process `rill-view` clients;
  more IPC; a process per window. `AppView`/`rill-viewport` stays as the
  *client-side* render surface (what each `rill-view` process uses) — it is not
  thrown away, it just stops being multiplexed inside one shell process.
* Chosen over Option B (shell keeps in-process AppViews and also hosts foreign
  Wayland clients) — B was cheaper but keeps Rill apps un-isolated.

### D2 — Foundation: Smithay, nested backend

Build on **Smithay** (pure-Rust Wayland compositor library; its `anvil`
example is essentially this milestone), **winit/nested backend** for
Desktop Phase 2. Fits Rill's pure-Rust, no-C-deps ethos. Accepted cost: it is
a large dependency for a project that has been dependency-light. Rejected:
wlroots (C) and raw `wayland-server` (reinventing Smithay).

---

## Open questions / risks

* **gpui is a Vulkan/blade client → needs `zwp_linux_dmabuf_v1`, not just
  `wl_shm`.** Getting `rill-view` (GPU buffers) to display inside the nested
  compositor is the riskiest integration and should be de-risked first. The
  Smithay winit backend with a GLES renderer can import dmabuf.
* Shell-as-client: the wallpaper/dock should become a **wlr-layer-shell**
  client (how swaybg/Waybar work). Not required for the milestone-14 exit
  condition; a later slice.
* How the dock launches apps changes: spawn `rill-view --app X` as a
  subprocess instead of opening an in-process AppView.

## Progress

* **14a — DONE.** `rill-compositor` (Smithay/winit, single-file) binds its own
  Wayland socket, spawns a client, and composites its surfaces into a nested
  window. Verified: alacritty renders a live shell inside it, connected to the
  compositor's socket (not the host). Based on Smithay's `minimal` example.
  Renders `wl_shm` clients; keyboard forwarded, focus-follows-pointer.
* **14b — DONE.** Added `wl_output` + xdg-output + `zwp_linux_dmabuf_v1`
  (672 importable formats — real hardware EGL, the software-fallback risk did
  not materialise). gpui's earlier `PlatformNotSupported` was the missing
  output + dmabuf globals. Verified: the **Aurora Rill app renders correctly**
  inside the compositor via dmabuf. Import is optimistic in `dmabuf_imported`
  (`notifier.successful`); the render pass imports on demand.
* **14c input — DONE.** Real seat: `PointerHandle` (motion/button/axis) with
  focus-under-cursor via a `Space<Window>`, and keyboard with `SERIAL_COUNTER`
  + real event timestamps. Verified in alacritty inside the compositor:
  typing, **arrow keys (fixed)**, and **drag-select** all work. Debugging note:
  gpui/Aurora looked unresponsive but was actually receiving input (motion
  focus + 14k commits) — its buttons just reload the same page and its hover is
  subtle; and in a *nested* compositor the host draws the cursor, so gpui can't
  change it to a pointer. Not a bug.
* **14c-WM — DONE.** `MoveGrab` + `ResizeGrab` (`PointerGrab` impls) driven by
  `XdgShellHandler::move_request` / `resize_request`. Verified: titlebar drag
  moves windows; edge/corner drag resizes (min 240×160, anchored opposite
  edge). Multiple clients cascade on map; click raises + focuses.
* **14d — DONE.** Clients are `+`-separated in argv; verified `rill-view` (a
  Rill app) and alacritty running together = **milestone 14 exit condition
  met**. proj-plan.md milestone 14 ticked.

## Milestone 14 — DONE. Known polish gaps (not blocking)

* **Cursor shape — FIXED.** Advertise `wp_cursor_shape_v1`; store the client's
  requested `CursorImageStatus` and apply it to the host winit window each
  frame (`window.set_cursor`). Smithay's `CursorImageStatus::Named(CursorIcon)`
  and winit's cursor are the same `cursor_icon::CursorIcon`, so it's a direct
  hand-off — no bitmap drawing needed. Verified: resize arrows on edges,
  pointer-hand on links, I-beam over terminal text. (Client cursor *surfaces*
  still fall back to the default arrow; gpui/alacritty use the shape protocol.)
* **Overlap / focus / z-order — FIXED.** Root cause was an inverted render
  order (Smithay render elements are front-to-back; the loop collected
  bottom-to-top, drawing the raised window *behind* — so visual top ≠ input
  top). Now iterate `space.elements().rev()`. Also: hit-test by visible
  **geometry** top-to-bottom (`window_under`) instead of the client input
  region (CSD shadows were letting clicks fall through), and draw an **accent
  focus border** around the top window. Overlaps now behave correctly.
* **Resize drift — FIXED.** Was repositioning mid-drag from the *requested*
  size (raced the client). Now the grab only requests the size; repositioning
  happens on **commit** using the applied size, anchoring the opposite edge
  (`ResizeState` in `Rill`).
* **Cursor — FIXED** (above). **Decorations:** advertise `xdg-decoration` as
  **client-side** — Rill apps (rill-view) draw their own consistent titlebar,
  themed by precedence (user-enforce > manifest `[window] titlebar` >
  `surface-raised` default). **Foreign apps that don't self-decorate (e.g.
  alacritty) are left bare** by choice — no titlebar, not draggable. A future
  targeted server-side fallback (compositor draws a minimal bar *only* for
  undecorated foreign windows) was discussed and deferred.
* Still open: output mode fixed 1280×800 (doesn't track winit resizes → dock
  and wallpaper don't follow when the compositor window is resized); no
  popups; optimistic dmabuf import; no dim on inactive windows.

## Live cross-process theming — DONE

The process-per-app model broke live re-skinning (apps load their theme at
launch). Restored **declaratively, no IPC**: the dock writes a `theme.runtime`
sidecar (sibling of `theme.toml`) holding the active palette name + enforce
flag; every process (dock + each `rill-view`) runs a background gpui timer
(`cx.background_executor().timer`, ~300ms) polling that file's mtime and
reloading its theme on change. Verified: cycling the palette / toggling
override re-skins all already-running apps + the dock within ~300ms. ~300ms lag
is the poll interval (tunable). Newly launched apps read `theme.runtime` at
startup so they match immediately.

## Fetcher / offline — DONE (earlier note was a misdiagnosis)

Static apps were **already offline-first**: `Fetcher::fetch` on `Source::App`
is pack-first — `InstallStore::read_resource` serves resources from the local
`.rillpack` and only falls to the origin server for resources not in the pack.
The earlier "apps need the server to launch" symptom was actually the
**frame-scheduling bug** (fixed via `request_animation_frame`), confirmed by the
server log showing zero connections during the "hang". The real gap was
robustness: `Client::connect` had **no timeout**, so a genuinely unreachable
server hung the surface forever. Fixed: a 4s connect timeout (`CONNECT_TIMEOUT`
in fetcher.rs) for both page fetches and actions → offline/unreachable degrades
to a clean error in seconds. Verified: static apps (Aurora/Console/Glass/
Brandbook) launch and render with the server down; dynamic Notes shows a clean
"server unreachable (timed out)" instead of an eternal spinner.

Next: the shell-as-layer-shell-client integration (Option A arc), then
milestone 15 (standalone DRM session — needs gbm/libinput, absent here).

## Environment notes (this build box)

* Smithay 0.7 (default-features off; `backend_winit backend_egl renderer_gl
  wayland_frontend desktop`) **compiles and runs** here. Gate binary (slice
  14a) creates a `wayland_server::Display` successfully.
* 64-bit libs are in **/usr/lib64**, not /usr/lib (which is 32-bit). The
  linker needs unversioned dev symlinks, so the scratchpad `libshim` dir has
  `libxkbcommon.so`, `libwayland-server.so`, `libwayland-client.so`,
  `libwayland-egl.so`, `libEGL.so`, `libgbm.so`, `libGL/libGLESv2/…` →
  `/usr/lib64/*.so.N`, used via `RUSTFLAGS="-L $libshim"`. Same pattern as the
  existing xkbcommon-x11 shim.
* Run with `LD_LIBRARY_PATH=/usr/lib64` (or rely on the loader finding the
  versioned sonames).
