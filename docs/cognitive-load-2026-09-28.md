# Cognitive load and hot spots — 2026-09-28

Status: **dated record of one review pass.** Six read-only reviews of the
tree (compositor; gpu + draw; viewport + ui + doc; platform + wire crates;
apps + history; repo wayfinding), one clippy run with only the complexity
lints on, and a first measurement pass on the Pi. Nothing was changed.
Companion to [audit-2026-08-19.md](audit-2026-08-19.md) in form. Numbers
are MEASURED (box named) or COUNTED (from source); nothing here is projected.

## 1. Hot spots: what is measured and what is not yet

**MEASURED, Pi 5, kiosk profile, 2026-09-28 21:5x, 14.2 days of uptime,
binaries of 2026-09-25 11:20:** the compositor's main thread has used
29,246 s of CPU in 1,226,330 s of wall time, 2.4% of one core. A
per-second series over 85 s shows a flat 3% baseline and two 12% seconds,
no bursts. rill-vector: 432 s total across three threads. signage-app:
194 s in 3.4 days, 0.07%. The kiosk at rest has no hot spot worth a
sampler; its cost is the 1 Hz heartbeat frame.

**Correction, 2026-09-29 11:27, from the kiosk's own exit report (journal,
`rill-session.sh[18556]`, stopped for the A/B after 1,275,269 s = 14.8
days):** the kiosk was *not* at rest. `frames=4118108 heartbeat=1
damage=4118107 mean_fps=3.23`, `commits=127537 frames_per_commit=32.29`,
`frame_ms mean=204.63`, `work_ms_mean=204.62`, histogram pinned at its
top bucket (p50 = p95 = p99 = max = 418.61). Read together with the
2.4%-of-a-core CPU figure above: the compositor drew a damage frame every
~310 ms for two weeks, each costing ~7 ms of CPU and ~200 ms of wall
time, with 32 frames between client commits. The damage gate was open
the whole run and the frame was bound on something other than the CPU
(the V3D, or a wait). What holds the gate open and what the 200 ms is
are NOT YET MEASURED; the theme on the board has no shader (ember's
colours only, plus `sky`, `wallpaper_scene = scenes/morning`, and a
remote `temp` widget pointed at the workstation). `bench-busy.sh` on the
Pi now records the kiosk configuration as its own run, with the V3D
clock sampled mid-run, so the next pass has the number.

**MEASURED on the Pi, 2026-09-29 11:40–11:47, `~/bench-busy.sh` under the
real session unit, four 72 s runs, exit reports from the journal
(`~/bench-bin/results.txt` on the Pi):**

| run | mode | frames | damage | mean fps | frame_ms mean | p50 |
|---|---|---:|---:|---:|---:|---:|
| kiosk-orig (09-25 binary, ember theme) | 1080p@60 | 224 | 224 | 3.09 | 88.7 | 168.5 (top bucket) |
| kiosk-after (text cache) | 1080p@60 | 227 | 227 | 3.10 | 100.8 | 145.7 (top bucket) |
| busy-before (HEAD) | 4K@30 dock | 79 | 10 | 1.09 | 26.2 | 22.3 |
| busy-after (text cache) | 4K@30 dock | 80 | 9 | 1.09 | 27.3 | 21.8 |

Three readings:

* **The text cache does nothing for the kiosk**, as expected: the board's
  text is static and already hit the measuring cache; the drawing cache
  saves CPU the kiosk was not spending.
* **The busy A/B is inconclusive.** The dock ran at the panel's native
  4K@30 (no `RILL_DRM_MODE` in the dock branch of rill-session.sh) and the
  workload never animated: 10 damage frames in 72 s against 976 on the
  workstation for the same theme. The two widgets spawned (three
  rill-vector pids in the journal) but produced no commits worth the
  name. Why is not known; the bench server logs requests only under
  `RILL_LOG=requests`. Redo needs the mode pinned and that flag set.
* **The kiosk's gate is held open by the theme tick, and it is a
  one-line bug.** main.rs:1936–1950: every 300 ms the tick compares
  `fx_conf.cursor.draw` (the theme's `draw = false`) against
  `cursor_style.draw`, which the previous tick OR-ed with `metal_backend`
  to true, and asks for a redraw. On the metal with a theme that hides
  the cursor, that is a redraw every tick: 1/0.3 s = 3.3 Hz, and the
  kiosk measured 3.09–3.23 fps across 15 days. CONFIRMED live on the
  running kiosk 2026-09-29 11:5x by flipping the theme's `draw` (no
  visual change on the metal, which draws the cursor regardless):

  | theme `[cursor] draw` | compositor main thread CPU |
  |---|---:|
  | false (as deployed) | 2.40% of a core (40 s) |
  | true | 1.03% of a core (60 s) |
  | false again | 2.42% of a core (40 s) |

  The residual 1% under `draw = true` has another source, not yet
  identified (candidates: the remote `temp` widget, the sky sampler, a
  second sentinel in the same tick). Fix shape: compare the *effective*
  cursor (theme value OR-ed with `metal_backend`) against the last
  effective value, not the raw theme value against it.
* **Each kiosk frame costs ~90–100 ms of wall time for ~7 ms of CPU.**
  The scene is twelve full-screen SVG layers (the `scenes/morning`
  directory holds the numbered set *and* a `scene-N` set, so every layer
  is drawn twice) plus 48 sky bands, at 1080p on the V3D. Attribution to
  the GPU is PROJECTED: rill-gpu has no timestamp queries
  (`timestamp_writes: None` at four sites). With the gate closed this
  costs once a minute instead of three times a second.

**MEASURED earlier, same board, release, [memory-footprint.md](memory-footprint.md)
"Frame times":** busy desktop p50 29.0 ms per frame, p99 32.75, tight,
on every frame. The doc's own diagnosis stands: no render cache, every
composite rebuilds each window's GPU buffers. Run #2 kiosk frames were
p50 8.25 ms, p99 12.5 ([pi-soak.md](pi-soak.md)).

**NOT YET MEASURED:** where inside those 29 ms the time goes. State of
the attempts, 2026-09-29 morning:

* `samply` 0.13.1 is on both boxes (`~/.cargo/bin` here, `~/.local/bin`
  on the Pi) and `kernel.perf_event_paranoid` is now 1 on both.
* Every samply run then fails at `mmap` with EPERM (strace'd here): it maps
  a 1 MiB ring per CPU and the per-user lock ceiling is
  `kernel.perf_event_mlock_kb` = 516 (528 on the Pi) times CPUs plus an
  8 MiB `RLIMIT_MEMLOCK`. Fix is `sudo sysctl kernel.perf_event_mlock_kb=8192`
  on each box (resets on reboot).
* The Pi compositor additionally runs with an ambient capability
  (`CapAmb` bit set, from the session unit), so attaching to it as `evan`
  is refused outright; it needs `sudo samply record -p <pid>`. rill-vector
  and signage-app attach fine once the lock ceiling is raised.
* A ptrace fallback (`eu-stack` every 30 ms) was tried here and returns
  nothing: `kernel.yama.ptrace_scope` is 1, so a sibling process cannot
  read the compositor's stacks.
**MEASURED, workstation (Ryzen 9 / RTX 5070, nested winit, release,
2026-09-29 08:56), bench desktop with the meter widget, samply at
1997 Hz for 60 s after a 12 s settle:** 976 frames in 71.5 s, all
damage, mean 13.6 fps; frame_ms mean 1.55, p50 1.50, p95 2.25, p99 2.75,
max 16.11. Profile: `target/bench/profiles/ws-busy.json.gz`, summary
beside it (`scripts/profsum.py` symbolicates with addr2line and prints
self and inclusive time per thread). Samples by thread, of 7,619:

| thread | samples | share | what it is doing |
|---|---:|---:|---|
| compositor main | 3,871 | 51% | 58% inside `composite_scene` → `build_frame`; **39% inside `TextEngine::place_line` → cosmic-text `set_text` → rustybuzz shaping**; 10% `ioctl` (GPU submission) |
| rill-history writer | 2,080 | 27% | 77% in the **boot-time retention pass**: `age_older_than` → `read_seal_with` → `decode_seal` → `Index::from_bytes` → `tokenize` (31% self); 13% `fdatasync` |
| rill-vector (dock) | 648 | 9% | 64% in layout → `TextMeasurer::measure` → `TextEngine::prepare` → cosmic-text shaping |
| parec / pump_events / others | ~1,000 | 13% | audio tap idle wait, winit event pump |

Per frame on this box: about 2.0 ms of main-thread CPU (3,871 samples at
1997 Hz over 976 frames), of which roughly 1.2 ms is `composite_scene`
and 0.8 ms is text shaping.

What that says, read against the source:

1. **Text is re-shaped from scratch on every frame.** `prepare` (the
   measuring half) has a byte-bounded `ShapeCache` (rill-gpu text.rs:51,
   :364). `place_line` (the drawing half, text.rs:448–474) has none: it
   builds a new cosmic-text `Buffer`, calls `set_text` and shapes every
   line slice every frame, and `build_frame` calls it per text command
   (lib.rs:2616). Inside that, 31% of the main thread is
   `hb_ot_shape_plan_t::new` with `ttf_parser` feature and script
   table parsing under it, so the shaper is rebuilding its plan per
   call as well. This is the render-cache gap the footprint doc named,
   located: it is the text half, and it is the largest single CPU item
   in the frame.
2. **The history boot pass decodes every sealed index to decide aging.**
   `age_older_than` runs once at writer start (history_writer.rs:185,
   "at boot rather than on a clock"). To age frames it reads each
   segment's seal and `Index::from_bytes` rebuilds the postings by
   tokenising every transcript entry (index.rs:481). Here that was
   108 segments, 1.1 GiB, in `~/.local/share/rill/history`, costing
   about 1.0 s of CPU plus fdatasync. It is one-shot, so it is not in
   the 29 ms frame; it is a boot cost that grows with history and it is
   the second boot pass beside the seal pass that pi-soak.md already
   projected. PROJECTED, not measured: on the Pi's cores the same pass
   is several seconds.
3. **The bench desktop is not hermetic for history.** `bench-stack.sh`
   and the profile wrapper redirect config, cache and demo data to the
   bench root, but the compositor wrote to `~/.local/share/rill/history`
   (no `XDG_DATA_HOME` override), so every bench run reads and extends
   the real history directory.
4. The dock's CPU is small in absolute terms (0.9% of a core) and is
   almost all text measurement through the cached `prepare` path.

**MEASURED after the fix, same box, same scenario, 2026-09-29 11:24
(samply at 997 Hz; the kernel had throttled `perf_event_max_sample_rate`
to 1000 by then, so shares within a thread are the comparable figure and
the compositor's own frame report is the headline):**

| | before | after |
|---|---:|---:|
| frame_ms mean | 1.55 | 0.77 |
| frame_ms p50 | 1.50 | 0.75 |
| frame_ms p95 / p99 | 2.25 / 2.75 | 2.00 / 2.50 |
| main thread inside `composite_scene` | 58% | 19% |
| main thread inside cosmic-text `set_text` | 46% | 11% |
| main thread inside `hb_ot_shape_plan_t::new` | 31% | 0.8% |

The fix: `place_line` now has the same byte-bounded LRU cache as `prepare`
(rill-gpu text.rs, `ByteCache`, keyed on slice, resolved face, snapped
weight, size and the mono-grid flag). What remains under `set_text` is the
text that genuinely changes every tick: the ASCII cube and the meter
readings. Profile: `target/bench/profiles/ws-busy-cached.json.gz`.

**NOT YET MEASURED: the same profile on the Pi.** samply 0.13.1 panics
in its own perf-event reader on the Pi's 6.18 kernel even in launch
mode, so it is out there; the Pi compositor and rill-vector also carry
an ambient capability from the session unit, so attaching needs root.
The route is Debian's `linux-perf` with `--call-graph dwarf` (Rust
release builds omit frame pointers), run as root against the live
kiosk, and `scripts/perfsum.py`-style aggregation of `perf script`
output. The kiosk at rest will show the heartbeat frame; the busy
number needs a scene with damage on the Pi, which means replacing the
kiosk with the bench stack for a minute.

**Found on the way: the nested compositor segfaults on exit, here, every
time.** `coredumpctl` has three dumps (2026-09-27 17:26 and 20:44 debug,
2026-09-29 08:45 release). Stack: `main` drops `Presenter` → wgpu
`Surface::drop` → `wgpu_hal::vulkan::Surface::unconfigure` → NVIDIA
glcore → `wl_proxy_marshal_flags` → SEGV_MAPERR in libwayland-client.
The Vulkan surface is destroyed after the Wayland display it was
created on is gone; a drop-order bug in the nested exit path. The exit
report has already been written by then, so measurements are unaffected.
Not seen on the Pi (DRM backend, no libwayland-client in that path).

## 2. COUNTED: clippy with only the size lints on

`cargo clippy --workspace -- -A clippy::all -W clippy::cognitive_complexity
-W clippy::too_many_lines`, 2026-09-28: 54 functions over 100 lines, 19
over cognitive complexity 25. The top of both lists:

| lines | cc  | function |
|------:|----:|----------|
| 1609  | 169 | platform/rill-compositor/src/main.rs:1403 (`main`, ends at 3461) |
|  890  |  59 | apps/studio-app/src/lib.rs:920 (`page`) |
|  889  | 100 | crates/rill-ui/src/layout.rs:355 (`layout_node`) |
|  781  |   – | crates/rill-gpu/src/lib.rs:741 (`with_device`) |
|  738  |  47 | apps/studio-app/src/lib.rs:2451 (`action`) |
|  583  |  53 | crates/rill-doc/src/codec.rs:602 |
|  542  |   – | crates/rill-gpu/src/lib.rs:2959 (`composite_scene`) |
|  414  |   – | crates/rill-doc/src/compile.rs:821 |
|  362  |   – | crates/rill-gpu/src/lib.rs:2408 (`build_frame`) |
|  348  |  36 | apps/signage-app/src/airport.rs:461 |
|  328  |  34 | platform/rill-vector/src/main.rs:1150 |
|  294  |  37 | apps/files-app/src/main.rs:542 |
|  203  |  53 | platform/rill-server/src/lib.rs:814 (`handle_connection`) |

Three of the four biggest are the three cores (compositor loop, renderer,
layout). The studio app is the outlier among apps: its size is eleven
pages in one function, not essential complexity.

## 3. What loads a newcomer, ranked across the tree

1. **Three cores are one file each with no seams.** `rill-compositor/src/main.rs`
   (6,578 lines, eight concerns; `main` holds 61 `let mut` locals beside a
   45-field state struct and interleaves ~25 phases per frame, both
   backends inline via four `match &mut presenter` and seven `cfg(drm)`
   blocks). `rill-gpu/src/lib.rs` (device bring-up, frame building,
   compositing, fx, particles, model behind one 37-field `Renderer` with
   ten `Mutex<Option<_>>` slots and no lock order; tests are 27% of the
   file). `rill-ui/src/layout.rs:355` (measure, distribute, place, paint,
   emit-regions in one match; Row measures by re-laying-out into a probe
   vec, so paint cost silently changes measurement cost).

2. **One concept, several shapes or names.** A node has four types
   (`Ir`, `Node`, `ResolvedNode`, DrawCommand hit-areas) and a style has
   four (`Partial`, `Style`, `ResolvedStyle`, hover box); a new style
   property touches seven files and a codec bit. Hit regions are not a
   type: they are DrawCommand variants that layout emits, `trim_scroll_regions`
   trims, `cull_offscreen` must skip, and the host reads back off the
   wire. In the compositor, five "background" concepts carry four names
   (`fx_conf.background`, `state.background`, `background_color`,
   `wallpaper`, `wallpaper_scene`); `fx` means three things in rill-gpu;
   `boids` is the legacy name for particles; `tier` means spacing tier,
   document sensitivity and app model; `Identity` is the peer, `--identity`
   is key material; the DRM backend is `drm` in the CLI and feature and
   `Metal` as a type. Across docs the Tier-0 client has ten aliases,
   two of them deleted crates.

3. **Invariants held by comments and push order.** Hit order equals
   document order, children before container (stated at
   rill-doc/lib.rs:329, implemented by push order in layout, consumed by
   `position()` and `menu_areas`). In `build_frame`, a span's mask is
   constant only if each arm calls `cut` before switching pipeline. The
   mask bind group is 1 for quad/line and 2 for fill/image/glyph, mirrored
   only in shader `@group`. The literal `64` (obstacles) lives in five
   places across Rust and two WGSL preambles; the mask slot `32` in six.
   `set_boids` → `step_boids` → composite is an order nothing checks.
   `space.refresh()` must precede `widgets.retain` and the recorder tick;
   `plan_release` must precede the fd read in stream dispatch. In
   rill-history, open → sealed → aged is not a type but a runtime bool.
   `AppHandler` is sync and "keep it quick" by comment only.

4. **Four coordinate spaces in `AppView`, none named** (rill-viewport
   lib.rs:1971–1990, :2310, :2337): layout runs at bounds/zoom, commands
   are scaled back, focusables are zoomed pre-scroll, region offsets are
   unzoomed, the cursor is stored zoomed plus scroll and passed to layout
   divided by zoom. Every click, hint and selection path picks one.

5. **Three freshness caches with three keys** on the fetch path: the
   disk cache keyed by an untyped `"host:port"` string
   (rill-client/lib.rs:335), the in-memory held hash that bypasses disk,
   and the server's `HashMemo` + `RevMemo`. The pair `(cached: bool,
   held: Option<[u8;32]>)` threads through four functions to express
   three modes and nothing names them.

6. **Bare booleans and tuples at call sites, everywhere.** `style_of(..,
   false)` twelve times; `dump(&mut tap, sent: bool, ..)` ~20 sites;
   `layout_document` with ten positional args; `key_binds: Vec<(String,
   Option<String>, Option<UiAction>)>`; `chrome_palette() -> (Color, Color,
   Color)`; `read_back -> (Buffer, u32)`; `type_colours -> (String, String,
   String)`; `begin_accum(.., first: &mut bool)` out-param; `key_input ->
   bool`.

7. **Configuration read mid-logic, scattered, twice duplicated.**
   `RILL_SKY_TIMELAPSE` is parsed in compositor main.rs:4240 (once) and
   scene_layers.rs:241 (per call). `RILL_TRACE` means "write legend" in
   appkit and "read legend" in vector. `RILL_SHOT_DIR`, `RILL_FULLSCREEN`,
   `RILL_DRM_*`, `RILL_HISTORY_FRAME_DAYS`, `RILL_IDENTITY`, `RILL_CACHE`,
   `RILL_DATA`, raw `HOME` in history_cmd.rs: no list exists anywhere.
   `unix_now` is defined four times in signage-app; `cycle_date`/`easter`
   exist in both the compositor and morning.rs "kept identical" by hand.

8. **The app pattern exists only on the trait.** Every app is
   `impl AppHandler { get, action, revision? }` + `compile_page(kdl)` +
   `server.dynamic(..)`, and the hardest rule (`revision()` must move on
   theme change and carry liveness) sits on the trait at
   rill-server/lib.rs:342–348 and nowhere a stranger looks. Nine apps
   diverge: two call `rill_doc::compile` directly and bypass the trace
   hooks; term hand-rolls field extraction; six use the kit `shell()`,
   four write raw KDL; bad input is `Internal`, `PathInvalid` or `NotFound`
   because `Status` has no bad-request variant; studio mutates on GET
   through a global `Mutex<String>`; three hand-written arg loops.
   Best teaching example: meter-app. Worst: studio-app.

9. **Docs that contradict the tree.** README has no build or run command
   and names no crate, script or directory; the one-command path
   (`scripts/demo-desktop.sh --launch`) and the lib64 shim
   (`scripts/link-shim.sh`) are mentioned only from measurement docs.
   No workspace map; all 33 `Cargo.toml` files have a `description` that
   is published nowhere. apps/README lists 9 of 13 apps. Front-door files
   (proj-plan, TODO, risks, external brief, pitches, grind log) are
   git-excluded while 13 tracked files link to them. Stale status lines:
   specs/history.md line 3 says "not yet designed" above §11 "built
   2026-08-21"; theming.md, appliance.md, compositor.md,
   application-model.md, wgpu-renderer.md still name `rill-shell`,
   `rill-view`, `rill-ui-gpui`; bare-metal-plan.md says "nothing built";
   icons.md says Tabler, CREDITS says Phosphor. Stale module docs:
   rill-gpu lib.rs:1–22 (W1 only, links a nonexistent `Renderer::composite`),
   compositor main.rs:1–2 ("nested"), rill-vector main.rs:1–13 ("W4 demo"),
   :355/:363 (retired sidecar), term lib.rs:17–19 (no scrollback),
   signage main.rs:1/28 ("three documents", five mounted), rill-history
   lib.rs:23–25 (crypt and retention "not owned"), platform/rill main.rs:1–7
   (subcommands missing). Glued doc comments (a `///` on the wrong item):
   rill-ui tree.rs:247, layout.rs:1460, viewport lib.rs:101, meter
   lib.rs:376, term lib.rs:48, segment.rs:887.

10. **Two real bugs found by reading, not a load issue but recorded here.**
    `Defaults::eq` (rill-ui tree.rs:233) ignores size, space, shadow and
    container tokens, so two themes differing only in density compare
    equal and upstream `theme != old` checks miss metric changes.
    rill-client lib.rs:150–153 documents that pack installs must raise
    `max_resource`; `rill app` (app_cmd.rs:73) uses the default, so a pack
    over 32 MiB cannot be installed, and rill-pack hardcodes the same cap.

11. **Spec versus code in theming.** theming.md §1 says every colour is a
    token lookup; layout.rs has literal colours at :953 (button), :1291
    (focus border), :1299 (input fill), :1343 (placeholder), :1393 (caret).

## 4. Do not touch without a guide (the reviewers' union)

- compositor theme hot-reload main.rs:1936–2306 (eleven `installed_*`
  sentinels; one missed re-uploads every 300 ms); per-window walk
  2701–2925 with the fx tables 3037–3100; stream `Dispatch` 5147–5330
  with `plan_release` 483.
- rill-gpu `composite_scene` 3395–3500 (a `forget_lifetime` render pass
  threaded with a `first` flag around `blur_chain`); `build_frame` span
  cutting 2418–2470; rill-draw `encode`/`decode` 205–890.
- rill-ui `layout_node` Row arm 660–880; `AppView::layout` lib.rs:1912;
  the style-property surface (seven files, one bit, `S_KNOWN`).
- rill-server `handle_connection` 814–1065 (denied == NOT_FOUND secrecy
  rule interleaved with sequencing and the revision memo); viewport
  `with_client` fetcher.rs:269–330; rill-vector `tick` 930–1030 + `draw` 531.
- term-app screen.rs:283–545; rill-history segment.rs:446–547 +
  `retention::rewrite`; signage morning.rs:41–100 demo clock.

## 5. Per-area verdicts

| area | load | biggest contributor | cheapest improvement (per reviewer) |
|---|---|---|---|
| compositor | high | `main` with 61 loop locals + both backends inline | one `ThemeInstall` struct owning the 1936–2306 block |
| gpu + draw | high (draw medium) | one file, one 37-field `Renderer`, ten mutexes | line-anchored concern map replacing lib.rs:1–22; name the shared 64 and 32 once |
| viewport + ui + doc | high | four shapes per node/style; hit regions as DrawCommands | name the four coordinate spaces in `AppView` |
| platform + wire | high at the seams | three freshness caches, `(cached, held)` threading | fix stale sidecar/mode comments and the `max_resource` contradiction |
| apps + history | medium / medium-high | app shape lives only on the trait, nine copies | fix glued doc comments and four stale module docs |
| wayfinding | high | README hands nobody to build, run or a crate map | README build-and-run section + member list from Cargo descriptions |

Everything in the right-hand column is comment, doc or one-line work.
Everything in section 3 items 1–5 is structure, and is not proposed here.
