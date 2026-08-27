# File explorer — design spec

A working document. Blocks and *relative* spacing, not pixels: every measure
below names a step on the theme's scale, so density stays a theme decision
(see `crates/rill-ui/src/tree.rs` for the tables).

Scales in play:

| | xs | sm | md | lg | xl |
|---|---|---|---|---|---|
| **space** | 4 | 8 | 12 | 20 | 32 |
| **type** | 11 | 13 | 15 | 20 | 26 |
| **elevation** | 8 | 18 | 32 | | |

Sections marked **▢ decide** are yours to fill in — this maps the frame, not
the taste.

**Scope decision (2026-08-10): the explorer browses; it does not read.**
File pages show metadata only (name, kind, size) — no inline text preview,
no image embedding, no content reads at all. Viewing/editing belongs to
dedicated apps (text app first, later image/media), which open a file under
their own capability grants. Rationale: keeps the explorer a pure view over
the store + policy, and keeps "what can read your files" an explicit,
per-app grant instead of an explorer feature.

---

## 1. Regions

```
┌──────────────────────────────────────────────────────────────────────┐
│ TITLEBAR                                                    h ≈ 34   │  ← client-drawn
├──────────────┬───────────────────────────────────────────────────────┤
│░░░░░░░░░░░░░░│░ TOOLBAR         pad md · gap md           h ≈ 44    ░│  ┐ one
│░ SIDEBAR    ░├───────────────────────────────────────────────────────┤  ┘ surface
│░ width 190  ░│                                                       │
│░ pad lg     ░│  CONTENT          pad lg · gap md                     │
│░ gap sm     ░│  (strictly gridded — nothing else lives here)         │
│░░░░░░░░░░░░░░│                                                       │
└──────────────┴───────────────────────────────────────────────────────┘
   ░ = `surface`                    content = page background
```

**The chrome is one surface, the content is another.** Titlebar and sidebar
share `background="chrome"` — a *translucent* token, derived once per palette
from `surface-raised`, so on a glass window the whole L frosts the desktop
behind it instead of the titlebar frosting alone. They meet at the corner
with no rule between them and read as a single surface; the content pane
sits on the page background and holds nothing but the grid. Transient panels
(new folder, rename) drop into the chrome surface, not into the content —
they are chrome, and putting them in the grid is what made it feel like a
document.

The column that wraps toolbar + content therefore contributes nothing of its
own: `style="pane" padding=0 gap=0`. Style spacing takes a literal for
exactly this reason — zero is not a step on the space scale and should not
become one.

* Sidebar is a **pinned** column (`style width=190 height="fill"`); content
  fills the rest.
* Status bar: dropped. The counts it would carry (hidden items, "+N more")
  sit in the toolbar, where they are facts about the directory rather than a
  strip trailing the content.

---

## 2. The titlebar is ours — and now the app's

This is the part with no equivalent on other desktops.

A Rill window's titlebar is **not** the compositor's. It is ordinary
`DrawCommand`s in the same frame as the document, so there is no WM boundary
to negotiate — and a document can now claim it outright:

```kdl
titlebar {
    row style="titlebar" { ...breadcrumb... ; spacer; ...controls... }
}
```

`resolve` lifts that subtree out of the page flow into `UiTree.chrome`;
`layout_chrome` places it into the rect the *window* owns. It is unzoomed —
chrome belongs to the window, so it holds still while the page scales — and
it is outside the document's tab order, because a titlebar is not part of
the page.

```
┌──────────────────────────────────────────────────────────────────────┐
│  ⌂ Root ▸ work ▸ reports              [ ⊞ ▤ ]   + New folder      ×  │
│  └────── breadcrumb ──────┘           └view┘   └── verbs ──┘   └close│
└──────────────────────────────────────────────────────────────────────┘
   pad sm · gap sm            spacer                          40px kept
```

The explorer has **no toolbar row of its own**. The window spends 44px on
navigation where it used to spend 34 + 44, and the content pane holds
nothing but the grid.

A host that cannot lend its bar does not lose the toolbar: rill-view draws
its titlebar with native gpui elements, so the app's chrome lands in a strip
directly beneath it — same commands, one strip lower.

Wire cost is gone from the strip. It was developer telemetry in the most
valuable space in the window; `RILL_WIRE_COST=1` brings it back.

**▢ decide** what else the strip owes each view:

| Region | Grid view | List view | File |
|---|---|---|---|
| left | breadcrumb ✓ | breadcrumb ✓ | breadcrumb ✓ |
| right | view switch ✓, new folder ✓, delete ✓ | same ✓ | size ✓ — kind? actions? |
| missing | back / forward | sort | open-with, reveal |

---

## 3. Content — grid view (default)

Wrapping rows landed, so this is expressible now.

```
row style="grid" wrap=#true          gap md, pad lg
  ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐
  │  ICON  │ │  ICON  │ │  ICON  │ │  ICON  │   tile: width 96
  │  48px  │ │        │ │        │ │        │   pad sm · gap xs
  │ label  │ │ label  │ │ label  │ │ label  │   label: type sm, centred
  └────────┘ └────────┘ └────────┘ └────────┘   selected: elevation sm
  ┌────────┐                                     + accent border
  │  ICON  │  ← wraps automatically
```

* Tile width is fixed (`style "tile" width=116`); grids do not stretch tiles.
* Everything inside a tile aligns itself — `align` is honoured by text, links,
  icons and buttons alike, so the icon and the label centre on the same axis
  by saying `align="center"` rather than by nesting spacer rows.
* **▢ decide** tile width and icon size. 116/52 is the current guess; macOS
  runs larger, GNOME tighter.
* **▢ decide** whether labels truncate or wrap to two lines. *Neither is
  possible today* — there is no ellipsis and no line cap. See §6.

## 4. Content — list view

What we have now. Rows of icon · name · spacer · size.

```
  NAME                                              SIZE     type xs, muted
  ─────────────────────────────────────────────────────────  1px, border
  📁 folder-name                                       —     row: pad sm
  ─────────────────────────────────────────────────────────       gap md
  📄 file.txt                                       6 B           corner sm
```

* Hover already lights the row (`hover="rowline-lit"`).
* **▢ decide** which columns earn their place: modified date? kind? Both
  references show more than we do.

---

## 5. Selection

Currently a permanent *Selected / Clear / Rename / Delete* panel above the
list. That is not a design — it is a limitation showing through, and it should
go.

Target: selection lives **on the item** (accent border, raised surface), and
verbs live in the titlebar or a context menu.

**▢ decide** which. Both references use a context menu; the titlebar is the
cheaper path for us because popovers do not exist yet (§6).

---

## 6. What the platform cannot do yet

Honest list, so the spec above is not describing fiction.

| Needed for | Missing | Cost |
|---|---|---|
| Selection on the item | styling conditional on a *state value* — `when` tests booleans only | small: a compare-and-style form |
| Truncated labels | no ellipsis, no line cap | small, layout-only |
| Context menus | no layering — nothing can draw outside the flow | large: z-order, positioning, dismissal |
| Hover that does not snap | no motion, and no element identity to hang a transition on | large; identity also unblocks the render cache and the agent surface |
| A toolbar that reads as centred | row children are top-aligned; there is no vertical alignment | small: a second pass, or centre against the row's measured height |
| `padding="md"` on a *node* | node spacing is literal (numbers and `auto`); only a style names a step | medium: `Dimension` would need a token variant |
| Thumbnails | image transport in vector windows | medium |
| Tighter icon alignment | glyphs place by bounding box, not optical anchors | see the icon-protocol note |

Everything in §1–§4 is buildable today. §5 needs the first row of this table.

---

## 7. Open, for you

1. **▢** Tile width and icon size for the grid (116/52 today).
2. **▢** What else the titlebar carries per view — back/forward, sort.
3. ~~Wire cost~~ — opt-in via `RILL_WIRE_COST`.
4. **▢** List columns beyond name and size.
5. **▢** Selection verbs: they are in the titlebar now. Context menu instead?
6. ~~Status bar~~ — dropped; its counts moved to the titlebar.
