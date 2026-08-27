# System metrics — sizing as a system vocabulary

Status: **proposed, not built.** Parked deliberately while the file explorer
is made convincing; generalising from an unconvincing app is how you
standardise the wrong thing.

## The idea

An app should choose sizes from a named system vocabulary without knowing
what the numbers are, so a user can re-densify the whole desktop by editing
one table — with no per-app step.

## What exists

Three theme-owned scales, in `crates/rill-ui/src/tree.rs`:

| | xs | sm | md | lg | xl |
|---|---|---|---|---|---|
| space (padding, gap) | 4 | 8 | 12 | 20 | 32 |
| type | 11 | 13 | 15 | 20 | 26 |
| elevation (shadow) | 8 | 18 | 32 | | |

Plus `container_padding` / `container_gap`: the rhythm a container gets when
it says nothing, which is what makes cohesion the default rather than opt-in.

## What is missing

**No height or width vocabulary**, and the window chrome is worse than
untokenised — it is hardcoded per app:

```rust
const TITLEBAR: f32 = 34.0;      // platform/rill-vector/src/main.rs
const WINDOW_RADIUS: f32 = 14.0;
const EDGE: f32 = 8.0;
```

The explorer's sidebar is a raw `width=190`. So "make my desktop denser" is
not a setting; it is an edit in three crates.

## Proposal

Two more scales, same shape as the existing three:

| | sm | md | lg |
|---|---|---|---|
| **control** (toolbars, rows, buttons, titlebars) | 28 | 34 | 44 |
| **pane** (sidebars, panels, drawers) | 190 | 260 | 320 |

Then `TITLEBAR` becomes `theme.control("md")`, the dock height comes from the
same table, and one edit re-densifies the desktop *including window chrome* —
which no application has to know about or cooperate with.

**Control heights must be minimums, not absolutes.** A row holding 15px text
with `md` padding is naturally 39px; forcing 34 clips it. Both GNOME and
macOS do exactly this — standard control heights *and* content-driven rows.

## Open

* Does `pane` want percentages as well as pixels (a sidebar that is 20% of a
  wide window and 190px of a narrow one)?
* Should radius join the scales? `WINDOW_RADIUS` and per-style `corner` are
  unrelated today, so a "sharp" theme cannot square everything at once.
