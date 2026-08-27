# Rill wgpu Renderer — Design & Plan (the endgame backend)

Status: **in design** (Aug 2026). This is the "Rill-owned wgpu backend" that
[theming.md §4](theming.md) and the internal TODO ("North star") name as the
endgame. It replaces gpui as Rill's render layer and is the shared prerequisite
for vector-native windows, command-stream remoting, GPU-uniform theming, and the
post-process shader stage.

Related: [compositor.md](compositor.md), [theming.md](theming.md),
[document-format.md](document-format.md), [compute-apps.md](compute-apps.md).

---

## Why now

gpui got us a real desktop (milestones 1–14), but it is a **closed render
pipeline**: no hook for our own WGSL, no post-process pass, no render target we
control, and it owns the window/input/text stack too. Every remaining north-star
item — vector-native windows, remoting, session recording, the agent surface,
ricing/shaders, GPU-uniform palette swaps, honest HiDPI — is gated on Rill owning
the pixels. The `DrawCommand` seam (`rill-ui` emits `Vec<DrawCommand>` with zero
render deps; `rill-ui-gpui` is the *only* consumer) was built for exactly this
swap.

## The seam we're replacing

`rill-ui` → `Vec<DrawCommand>` is backend-agnostic. gpui is load-bearing in four
crates, and "replace gpui" means owning **all** of it, not just paint:

| gpui gives us | where | replacement |
| --- | --- | --- |
| paint (quad/shadow/text/image/clip) | `rill-ui-gpui` | `rill-gpu` wgpu pipelines |
| text shaping + font enumeration (cosmic-text) | `rill-ui-gpui` | cosmic-text + swash/atlas |
| window / input / clipboard / native chrome | `rill-view`, `rill-shell` | winit + our own |
| executor / frame scheduling / timers | `rill-viewport` | winit event loop + async |
| compositor GPU stack (Smithay `GlesRenderer`) | `rill-compositor` | wgpu renderer (D2) |

The `DrawCommand` vocabulary is small and GPU-friendly — six painted primitives
(`Rect` incl. rounded, `Shadow` blur/spread, `Text`, `Image`, `PushClip`/
`PopClip`) plus three non-painting hit regions. A tractable renderer surface.

---

## Decisions

### D1 — Orientation: compositor-first, no in-app-rendering detour

The renderer is built **toward the compositor rendering the command stream**
(vector-native windows), not toward an in-process gpui replacement. We
explicitly reject the "blit a `rill-gpu` texture into the existing gpui window"
stepping-stone: it would be throwaway scaffolding for a path we're not taking.

* **Why:** the compositor rendering `DrawCommand`s directly *is* the north star
  (kilobyte windows, restyle-in-flight, remoting, agent surface). Every other
  payoff reuses the same core. Aiming at it keeps all work on the critical path.
* **Consequence / honesty:** "compositor-first" is an *orientation*, not a claim
  that stage 1 is compositor code. The stream and its renderer must exist before
  the compositor can render them — so the ordering is still core → wire →
  compositor. The difference from the rejected option is that **none of it is
  throwaway.**
* **Standalone `rill-view` keeps gpui** for now: an app run outside the
  compositor still needs a local painter. `AppView` already produces
  `Vec<DrawCommand>` regardless of sink, so it gains a *second* sink (serialize
  to compositor) rather than losing the first.

### D2 — Single modern GPU stack: move the compositor onto wgpu

`rill-compositor` today renders with Smithay's `GlesRenderer` (GLES2/EGL),
compositing client **dmabufs**. We **replace that render path with a wgpu-based
renderer** on the same device that runs `rill-gpu`. One GPU API end-to-end.

* **Why:** rejected the two hybrids — (a) a GLES-native core plugged into
  Smithay (fast, but the "wgpu backend" becomes aspirational and we never get
  the modern stack), and (b) a wgpu core sharing buffers with GLES via
  dmabuf/EGLImage interop (two APIs in one process, fiddly and driver-sensitive
  — the worst of both). A single wgpu stack is the clean long-term answer and
  the only one that fully delivers the shader endgame.
* **Cost:** highest lift. Smithay ships **no** wgpu renderer, so we build one:
  dmabuf import into wgpu (for foreign clients like alacritty), shm import,
  damage, and compositing — on wgpu. We keep Smithay's Wayland *frontend*
  (protocol handling, surface trees, xdg-shell, seat, dmabuf global) and swap
  only the *renderer*.
* **Pivotal risk:** **dmabuf import into wgpu.** Foreign Wayland clients still
  hand us dmabufs; if wgpu can't import them on this box's drivers, D2 is
  jeopardized. This is front-loaded as a spike (see Milestone W0) *before* we
  commit renderer code. **W0 ran and retired this risk — D2 confirmed viable.**

### D3 — Text stack: hand-roll raster/atlas/pipeline over cosmic-text shaping

`rill-gpu` uses **cosmic-text for shaping only** (job 1 — which glyphs, where;
the hard part nobody hand-writes, and what gpui uses today) and **hand-rolls the
GPU side** — `swash` rasterization, our own glyph atlas, our own wgpu text
pipeline. Rejected **glyphon** (off-the-shelf cosmic-text→wgpu text layer).

* **Why:** the four jobs are shaping / raster / atlas / draw; only raster+atlas+
  draw are up for grabs (shaping is cosmic-text either way). Two Rill-specific
  factors decide it:
  1. **Parity by construction.** `rill-ui::layout` already owns line-wrapping
     (`wrap_segments`) and only asks the backend to *shape+measure*. Reusing the
     *same* cosmic-text shaping + the *same* wrap logic makes the new measurer's
     numbers **identical to today's** — killing W1's top risk for free. glyphon
     does its own wrapping at the `Buffer` level, which *reopens* that risk.
  2. **Ordered, single-pass rendering.** Our renderer paints `DrawCommand`s in
     strict order with clips/overlaps; a glyph is then *just another textured
     quad* in the same ordered pipeline as rects/images. glyphon prefers to
     batch all text into its own pass, which fights ordering and carries its
     model into the compositor.
* **Cost:** we own the atlas + text pipeline (the "glyph atlas engineering" the
  theming.md caveat named). Bounded, one-time, and core competency we want
  anyway. glyphon's shortcut saves work on jobs we're fine owning while adding
  cost to the two things we care about most.
* **Determinism bonus:** owning the atlas + pixel-grid policy is what the render
  cache (north star) and the pixels-vs-vectors invariant want.

### D5 — Shader slot scope: whole-output post-process *and* blur-behind

(Decided Aug 2026.) The W5 shader slot ships both halves at once:

* **Whole-output user shader:** one fullscreen WGSL pass over the finished
  frame (CRT, color grading, vignette, night light). User supplies a fragment
  stage; the renderer supplies the preamble (scene texture + sampler +
  uniforms + fullscreen vertex stage). Configured from
  `theme.toml [desktop] shader`, hot-reloaded by mtime. naga-validated at
  load; a bad shader logs and drops to identity — never crashes the desktop.
* **Blur-behind:** windows/panels can sample a blurred copy of what's behind
  them (dual-Kawase, the KDE/Hyprland technique). This forces the composite
  architecture change: accumulate into an offscreen texture, break the pass
  at each backdrop, blur, resume — then the whole-output pass runs over the
  accumulation. When neither feature is active the old direct-to-swapchain
  single pass is kept (zero cost unused).
* **Damage-gate interplay:** naga's post-validation `GlobalUse` tells us
  whether the user shader actually reads the `time` uniform. Static shaders
  (grading/vignette) keep the damage-gated idle win untouched; only genuinely
  animated shaders get a paced redraw loop — a cost the user opted into by
  installing one.

### D6 — Blur-behind is a DrawCommand, not a window property

(Decided Aug 2026.) `DrawCommand::Backdrop { rect, blur, corner_radius }` —
frosted glass is *paintable content*, not window metadata.

* **Why:** any rect in any document can frost — a titlebar alone, a sidebar,
  the dock (whose theme already carries a glass alpha). It rides the existing
  `rill_stream_v1` stream with no protocol bump, and it is the vector-native
  answer: effects are part of the command vocabulary. Rejected: a per-surface
  protocol flag (coarser, protocol version bump) and a theme-global toggle
  (all-or-nothing, nothing novel).
* **Cost/containment:** it enters the wire format — codec tag, fuzzer corpus,
  and a hard cap (`MAX_BACKDROPS` per frame) so a hostile client can't turn
  65k backdrop commands into 260k fullscreen blur passes. Hosts without a
  backdrop (standalone `rill-view`, gpui) no-op the command; the panel's own
  translucent fill keeps content legible. For the render cache, `Backdrop`
  is the one command that samples the scene — an explicit determinism escape
  to be tracked when the cache lands.

---

## Milestone plan

Sizing is relative; each milestone is independently reviewable and leaves the
tree green.

### W0 — De-risk: dmabuf-import-on-wgpu spike — ✅ **DONE, D2 confirmed viable**
The one experiment that could invalidate D2. Ran a headless wgpu probe on this
box (NVIDIA RTX 5070 + AMD RADV + llvmpipe). Findings:

* **All three Vulkan drivers advertise the full dmabuf-import extension set** —
  `VK_EXT_external_memory_dma_buf`, `VK_KHR_external_memory_fd`,
  `VK_EXT_image_drm_format_modifier`, `VK_EXT_queue_family_foreign` — including
  the historically-weakest NVIDIA proprietary driver.
* **wgpu 26 initializes on this Vulkan stack** and exposes the raw `ash`
  device/instance/physical-device via `Adapter/Device::as_hal::<Vulkan>()`.
* wgpu's **default** device already enables 2 of the 4 (`external_memory_dma_buf`
  + `external_memory_fd`); the other two are added via **`wgpu-hal
  device_from_raw`** — a probe **built a `wgpu::Device` with all four enabled and
  allocated on it successfully** (`create_device_from_hal` round-trip).
* Cross-checked against reality: **milestone 14b already imports these same
  foreign-client dmabufs via EGL/GLES** (672 hw formats), so the buffers are
  proven importable on this hardware.

**Verdict:** D2 is viable. The device-creation path is settled (build via
`device_from_raw` with the four extensions, wrap with `create_device_from_hal`).
The only unexecuted step is binding an actual imported fd to a `vk::Image` and
sampling it — that's `texture_from_raw` (API confirmed present) and becomes the
first concrete task of **W3**, not a capability unknown.

**Constraint surfaced for W3 (multi-GPU):** this box has two GPUs. Cross-GPU
dmabuf import generally fails, so the compositor's wgpu device **must be the same
physical GPU the clients allocate on.** Enable `VK_EXT_physical_device_drm`
(missing by default in the probe) to match the Vulkan device to the DRM node.

Spike code (throwaway): `scratchpad/wgpu-dmabuf-spike/` — stages 1 (init+hal),
2 (default-device extensions), 3 (build dmabuf-capable device).

### W1 — `rill-gpu` renderer core — ✅ **DONE** (Aug 2026)

Landed as `crates/rill-gpu` (headless: commands → offscreen texture → RGBA
readback, pixel-asserted on the real GPU; 16 crate tests):

* **One SDF quad pipeline** for sharp rects, AA rounded rects, and shadows
  (blur falloff; smoothstep approximation, not true gaussian — noted).
* **Ordered executor**: commands paint in list order; `PushClip`/`PopClip`
  become scissored spans; quads/images/glyphs interleave correctly.
* **Images**: `ImageSource` trait (mirrors gpui's `ImageProvider`), textured
  quads, byte-exact gpui placeholder parity. Per-frame upload, no cache (P3).
* **Text (D3 executed)**: wrap arithmetic **moved into `rill_ui::text`** —
  `wrap_segments` + `split_runs` + `LINE_HEIGHT_FACTOR` + font candidates are
  now shared code both backends run, so measure/paint parity with gpui is
  structural, not tested-in. `rill-gpu::text::TextEngine` shapes with
  cosmic-text 0.14 (the same version gpui locks), `EngineMeasurer` implements
  `TextMeasurer` with identical arithmetic; swash rasterizes into a coverage
  (R8) shelf-packed atlas; a glyph pipeline tints coverage by text color.
  Glyph snapping happens in the renderer (`place_line`) — the
  pixels-vs-vectors split, honored.
* Dependency note: `rill-gpu` enables `naga/termcolor` to keep naga 26
  compiling next to gpui's blade (codespan-reporting feature unification);
  drop when gpui leaves.

Follow-ups (not W1-blocking): paint-side shape cache, atlas eviction (reset-
on-full today), color-emoji atlas page (alpha-collapsed today), true gaussian
shadows, and the `rill-ui` caret `.round()` removal at cutover.

*(original scope below)*
New crate. `&[DrawCommand]` + glyph source + render target → a frame. Pipelines:
solid/rounded quad (SDF), glyph-atlas text, image, blurred shadow, scissor/clip
stack. **Text stack lifted out of gpui (per D3):** cosmic-text for *shaping only*
+ swash rasterization + our own atlas + our own wgpu text pipeline. Parity is
preserved by **porting the existing `wrap_segments` two-phase engine verbatim** —
the new `TextMeasurer` reuses the same shaping + wrap, so `rill-ui::layout`
numbers don't move (the measurer touches shaping/wrapping only; raster+atlas are
paint-side and don't affect measurement). Headless-testable: render to texture,
hash pixels. The pixels-vs-vectors invariant is *enforced here* — DPR, snapping,
the caret `.round()` all move into the renderer and out of `rill-ui`.

### W2 — Command-stream serialization — ✅ **DONE** (Aug 2026)
`rill_ui::stream::{encode, decode}`: big-endian, tag bytes, length-prefixed
strings, strict decode + encode-side validation (mirror limits, so every
encoded stream decodes) — same discipline as the `.rill` codec. Hit-regions
serialize alongside paint (the stream carries semantics). Verified: full
round-trip of every command/action/value kind, truncation sweep over every
prefix, hostile-byte rejection, and the size premise — a 200-command frame is
single-digit KB. Fuzzing joins the P1 list (W4 feeds this decoder client
bytes — fuzz before W4 ships).

### W3 — Compositor on wgpu — ✅ **DONE** (Aug 2026), D2 landed
The exit gate passed live: the full desktop (wallpaper, dock, five app
windows, focus glow, stacking, move/resize grabs, live palette switching,
fullscreen) composites through wgpu end-to-end, alacritty (shm) + gpui/Vulkan
rill apps (dmabuf) together. Landed in three slices:

* **W3a** — `rill_gpu::dmabuf`: `DmabufDevice` (W0 recipe productionized) +
  `import()` (fd → `vk::Image` → first-class `wgpu::Texture`) +
  `supported_modifiers()`; tested by Vulkan-exporting a dmabuf and importing
  it back (exact pixel round-trip).
* **W3b** — compositor-facing renderer API: `Renderer::with_device` (external
  device + surface format), `build_frame`/`paint_frame` split,
  `composite(background, WindowLayers, overlay)` in one pass,
  `read_texture_rgba`.
* **W3c** — the cutover: Smithay keeps the Wayland *frontend*; the window is
  **raw winit** with hand-translated input (smithay's exact mappings). The
  planned "dormant EGL + Vulkan WSI on one window" shortcut failed honestly —
  the host's DRM-syncobj protocol allows one sync object per `wl_surface`, and
  EGL's claim locks Vulkan out — so EGL/GLES left the compositor entirely
  (cleaner than planned). dmabufs import per `wl_buffer` (cached); shm uploads
  per frame; AutoVsync paces the loop (retiring the old busy-spin). Gotcha
  found live: request the **adapter's real limits** — downlevel's 2048px cap
  panicked `Surface::configure` at fullscreen.

Follow-ups: premultiplied-alpha blending for window layers (visible only on
translucent window edges), damage tracking (P3), subsurface trees, multi-plane
dmabuf formats, drop the now-unused GLES smithay features.

### W4 — Vector-native windows — ✅ **CORE DONE** (Aug 2026, verified live)

`rill-vector`'s window carries **no pixel buffer**: every frame is a ~2 KB
encoded DrawCommand list the compositor renders via rill-gpu. Focus/stacking/
border treat it as a first-class window; link clicks run the full loop
(wl_seat → local hit-test → relayout → memfd → commit → compositor render)
with no pixels on the wire. Landed: the protocol below, scanner glue both
sides, `SceneLayer` compositing (command frames interleave with texture
windows in z-order), `offset_commands`, and the gpui-free SCTK client.
Gotcha: bufferless surfaces defeat smithay's bbox-clamped xdg geometry —
window rects for hit-testing/border come from the stream frame's declared
size. Follow-ups: decorations + move/resize for vector windows, keyboard
input, declared-size validation, fuzz the stream decoder (P1 — it now eats
client bytes), and `AppView`'s full engine (state/actions/fetch) behind the
stream sink.

*(original scope + D4 rationale below)*
A Rill app attaches a **command stream to its `wl_surface`** instead of a
dmabuf; the compositor decodes and renders via `rill-gpu`. **WM unchanged** —
focus/stacking/input/resize still flow through `xdg_toplevel`; only the surface
*content* changes from pixels to commands.

**D4 — Transport: custom Wayland protocol + memfd** (decided Aug 2026).
`rill_stream_v1`: a manager global with `get_stream(wl_surface)`; the stream
object's `attach(fd, size, width, height)` passes the W2-encoded frame in a
memfd (the wl_shm bulk-transfer pattern); the frame latches on
`wl_surface.commit`. Chosen over a side-channel socket because the Wayland
connection gives **identity** (a stream is bound to the client's own surface —
nothing to mint or leak), **atomicity** (frame + surface state are one commit;
no resize/frame races across channels), and **lifecycle** (client dies →
protocol cleans up) for free — the exact bug categories side channels bleed.
The socket option's remoting-uniformity argument dissolved on inspection:
remote windows need a session layer (auth, window announcement, reverse input
routing) that no local transport provides, and the shared part — `bytes →
strict decode → per-window frame state` — is kept **transport-agnostic by
construction** so the future TLS listener lands beside the Wayland door, not
through it. Two doors, one room; each door uses its side's native auth.

Client side: gpui can't speak custom protocols through its own connection, and
a vector-native client doesn't need gpui at all — no rendering. New
lightweight client (SCTK for xdg-shell/seat): layout via `rill-ui` +
`EngineMeasurer` (CPU-only), encode via `rill_ui::stream`, frames paced by
frame callbacks, input via plain `wl_seat` hit-testing the client's own
command list. Resize = configure → relayout → new stream: **reflow, not pixel
scaling**.

### Measured baseline — the vector-native desktop (Aug 2026)

Live measurements of the full desktop (compositor + shell + **five real apps
as vector windows** + alacritty), debug builds, taken the day the dock went
vector-native by default:

* **Frame data:** 0.4–1.2 KB per window per frame (whole desktop's app
  content ≈ 4.5 KB live); **zero bytes idle** — frames only exist on change.
  The same windows as pixel buffers: ~1.8 MB/frame, ~107 MB/s at 60 fps —
  **~1,500–4,000× smaller per frame.** A full-tilt zoom/scroll animation
  streams ~60 KB/s; that figure is also the future remoting cost, with text
  staying typeset.
* **RAM:** one vector app process ≈ **10.5 MB RSS** (its tokio runtime, font
  DB, and layout engine included); all five apps = 53 MB — less than half a
  typical Chrome tab. (Electron: 150–500 MB *per app*.)
* **CPU:** app processes idle at a flat **0.0%** — purely event-driven, no
  timers, no GC, no render loop. Compositor idled at ~17% pre-damage-gating,
  ~3.3% with the first damage gate; two D5-era fixes finished the job:
  a **15ms frame budget** (frame callbacks are the only pacing clients get —
  present is non-blocking here — so an eager client could previously spin
  commit→render→callback unthrottled), and **content-commit filtering**
  (gpui clients ping-pong *empty* frame-callback commits ~60/s/surface, one
  buffer attach total; only commits carrying a buffer/damage/stream frame
  count as damage now). Result: **1 render/s (heartbeat), 0.0% idle CPU**,
  frosted windows and a static effect shader included. An animated shader
  runs the paced ~67fps loop at ~11% (debug) — the cost the user opts into.
* **GPU: apps hold no GPU context at all** (rill-vector VSZ 150 MB vs
  alacritty's 5.5 GB of GPU mappings). One device, one glyph atlas, one
  pipeline set for the whole desktop — **GPU memory scales O(1) with app
  count instead of O(n).**
* **Processes:** 7 for the whole desktop with five apps (five Electron apps
  ≈ 25–30).
* **Path:** input → sub-ms relayout → µs-scale ~1 KB encode (the fuzzer
  sustained ~160K decode round-trips/s single-threaded) → memfd → decode →
  GPU. No per-app GPU round-trip exists.

Caveats: debug builds inflate compositor/binary numbers; alacritty and other
foreign clients remain pixel clients. The shape — kilobyte frames, zero-idle
apps, ~10 MB per app, GPU-free clients — is structural, not tuning.

**Images (2026-08-19).** They still do not ride the stream, and deliberately
never will: a frame names an image by source string and carries no pixels, so
a window stays kilobytes. Pixels take a second door — `attach_image` on
`rill_stream_v1` v2, the same sealed-memfd handoff a frame uses, once per
source rather than once per frame. In-band was measured against the thing this
whole design is for and rejected: one 1080p RGBA image is 8.3 MB against a
0.4–1.2 KB frame, four times `MAX_STREAM_SIZE` before anything else, and it
would break the allocation-light property the thin-client tier depends on.

**The client resolves images, not the compositor**, and that is a security
decision rather than a plumbing one. The client already must — the layout box
comes from the image's natural size, so it has fetched and decoded the image
before it can emit a frame mentioning it — and it does so under its own device
identity on its own connection. A compositor resolving source strings would
need credentials it does not have: identity here is a per-connection client
certificate with no delegation, so it would have to act as a desktop-wide
principal that any client can aim at any path by naming it in a document, and
the render-or-placeholder outcome would report whether that path exists. That
is a confused deputy and an existence oracle, against a threat model that
spends a dedicated constant on making denial indistinguishable from absence.
This is D4's rule applied unchanged: two doors, one room, each door using its
side's native auth.

Where media goes from here — residency, eviction, texture compression, the
raster-to-vector pipeline, and what is still undecided — is
[docs/media.md](../docs/media.md).

### W5 — Payoffs, each now cheap
HiDPI/fractional scale as a command-space transform · live restyle in-flight ·
~~post-process shader slot (theming.md §4)~~ · GPU-uniform palette swaps · then
the north-star trio: remoting (stream over mutual-TLS), session recording
(append-only command log), agent surface (structured screen = the a11y tree).

**Shader slot — DONE (Aug 2026, D5+D6).** Both halves live:

* `Renderer::set_effect(Option<&str>)` — user WGSL fragment stage over the
  finished frame; renderer supplies `EFFECT_PREAMBLE` (scene texture, `fx`
  resolution/cursor uniforms, `time`, fullscreen vertex stage). naga-validated
  with clean error strings; naga `GlobalUse` marks `time`-reading shaders
  animated (compositor renders continuously) while static shaders keep the
  damage-gated idle. Compositor hot-reloads from `[desktop] shader` in
  `theme.toml` (300ms mtime poll; a broken shader logs and keeps the last
  good one). Example shaders in `assets/shaders/` (crt/night/vignette),
  pinned by a validation test.
* `DrawCommand::Backdrop { rect, blur, corner_radius }` (tag 10) — frosted
  glass in the command vocabulary. `composite_scene` keeps the old single
  direct pass when unused; otherwise accumulates offscreen, breaks the pass
  per backdrop, dual-Kawase blurs (full→half→quarter→half), and paints the
  pane as a rounded-masked sample of the blurred half-res chain, then the
  effect (or an identity blit) runs accumulation→target. Wire caps:
  `MAX_BACKDROPS = 32`/frame, blur `0..=256`, enforced encode+decode
  (a 65k-backdrop stream would otherwise be a GPU DoS). `scale_commands`
  clamps blur at the cap so zoom can't make a frame unencodable.
  rill-vector's titlebar frosts the desktop behind the window; gpui hosts
  no-op the command (translucent fill carries the look). Headless tests
  cover blur mixing, the rounded mask, identity bit-exactness, inversion,
  rejection, and animation detection; fuzz corpus reseeded (8.8M-exec smoke
  clean).

---

## Risks

* **Text measure/paint parity (W1).** Layout correctness depends on the measurer
  and the painter agreeing on wrap to the pixel (today's two-phase cosmic-text
  engine guarantees this). A new stack must reproduce it or layouts shift. Seed
  the golden-image / metrics tests before cutting over.
* **dmabuf import on wgpu (W3).** ~~The D2 make-or-break.~~ **Retired by W0** —
  extensions present on all drivers, a dmabuf-capable `wgpu::Device` builds and
  runs on this box, and 14b already imports the same buffers via EGL/GLES. What
  remains is `texture_from_raw` plumbing (first W3 task), plus the multi-GPU
  device-matching constraint (compositor's wgpu device must match the client's
  allocating GPU; use `VK_EXT_physical_device_drm`).
* **Scope creep of "own the window."** Dropping gpui means reimplementing input,
  clipboard, timers, and native chrome. Kept off the critical path by D1
  (standalone `rill-view` keeps gpui); the compositor path needs none of gpui's
  windowing since the compositor already owns the window.
* **Determinism as an invariant.** The render cache (north star) needs
  `(doc, viewport, theme, zoom) → commands` to stay deterministic; keep the
  layout-determinism test extended as `rill-gpu` grows.

---

## Open questions / decisions still to settle (one at a time)

1. ~~**Text stack**~~ — RESOLVED (D3): hand-roll raster/atlas/pipeline over
   cosmic-text shaping; reuse `wrap_segments` for parity. glyphon rejected.
2. **Command-stream transport + surface model (W4):** custom Wayland protocol
   (`zrill_surface`) carrying serialized commands, vs. a Rill-protocol side
   channel; confirm the "`wl_surface` for WM + command stream for content"
   split.
3. **Windowing for standalone `rill-view` (later):** stay on gpui indefinitely,
   or move it to winit + `rill-gpu` once the compositor path is proven?
4. **Foreign-client story during W3:** any transitional GLES fallback, or hard
   cut to wgpu-only (D2 implies the latter — confirm no interim dual-path)?
