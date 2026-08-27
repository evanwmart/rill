# Rill Icons — Set Priorities & Hover Animation (working doc)

Status: **working doc**, Aug 2026. The custom icon set and its animation
model, ahead of building them. Interim implementation:
`crates/rill-ui/src/icons.rs` (vendored Tabler path data → polylines →
`DrawCommand::Path`); this doc describes what replaces it. Related: the
icon-protocol direction (style-driven stroke/fill + declared anchor
points), [theming.md](theming.md).

## Design contract

Inherited from the Feather/Tabler lineage the renderer already draws well:
**24×24 grid, 2px stroke, round caps and joins, `fill="none"`** — pure
strokes, so no fill primitive is needed. Anchor points (icon protocol) are
declared per icon and double as animation origins (below).

## The priority 20

Ordered by how many surfaces break without them.

**Universal glyphs (highest reuse — build first):**
 1. Close/X — window chrome, dialogs, dismiss, clear-field
 2. Chevron — one glyph, four rotations: dropdowns, breadcrumbs, nav
 3. Check — confirmation, checkbox, selection, success
 4. Plus — the "new/add" primary action everywhere
 5. Search (magnifier) — launcher, explorer, every list
 6. Menu/kebab (3 dots) — overflow menus; defers UI decisions
 7. Gear — settings surfaces, tray

**Status tray (visible 100% of uptime — extra polish, must read at 16px):**
 8. Wi-Fi (dot + arcs; degraded/off variants)
 9. Battery (outline + fill region — fill level is an anchor-declared zone)
10. Volume (wedge + arcs; muted variant = slash)
11. Lock/shield — device identity, encryption, capability prompts; the
    brand promise as a glyph

**Files & content:**
12. Folder   13. Document (page + fold + text lines)
14. Image (frame + sun + mountain)   15. Trash (lid + can + ribs)

**Actions & system:**
16. Refresh/sync (circular arrow) — also the stale-data indicator
17. Download (arrow into tray; flip axis = upload)
18. Edit/pencil   19. Terminal (`>_`)   20. Warning (triangle + !)

Weekend cut: 1–7 make apps usable; 8–11 make the desktop feel alive.

## Hover animation model

One mechanism: on hover, stroke opacity animates 0→full over ~120–180ms as
a spatial reveal. Three reveal modes, all one shader:

```text
directional   opacity = f(dot(pos, axis),        progress)
radial        opacity = f(distance(pos, origin), progress)
angular       opacity = f(atan2 around origin,   progress)
```

Per-icon metadata (pure data, carried by the icon protocol):
`{ mode, origin-or-axis, per-stroke stagger offsets }`. The declared anchor
point IS the animation origin. Press/state variants reuse the same
machinery (e.g. reverse, or a second pulse).

| # | Icon | Mode | Origin / axis | Intent |
|---|------|------|---------------|--------|
| 1 | Close | radial | stroke intersection | "this point, gone"; fastest in the set |
| 2 | Chevron | directional | pointing axis, toward tip | the hover says "this way"; rotates with the glyph |
| 3 | Check | directional | along stroke order, L→R | the drawing motion of ticking a box |
| 4 | Plus | radial | center | appearing-from-nothing; matched pair with Close |
| 5 | Search | radial | lens center | circle blooms, handle last — focusing a lens |
| 6 | Kebab | directional | top→bottom, staggered dots | tiny cascade hinting "more below" |
| 7 | Gear | radial | hub | hub first, teeth bloom — machinery waking (no rotation) |
| 8 | Wi-Fi | radial | the dot, arcs in sequence | signal propagating — the icon's own physics |
| 9 | Battery | directional | L→R through fill region only | hover charges it; outline constant |
| 10 | Volume | directional | L→R, wedge then arcs | sound leaving the speaker; mute slash = Close's bloom |
| 11 | Lock | directional + radial finish | top→bottom, then inner mark blooms | shield drops closed; confirmation beat |
| 12 | Folder | directional | bottom→top | filling with contents; deliberately subtle |
| 13 | Document | directional | top→bottom, text lines staggered | the page writes itself |
| 14 | Image | radial then directional | sun first, then landscape L→R | a tiny sunrise |
| 15 | Trash | directional | top→bottom, lid then can | on press: reverse (contents fade down) |
| 16 | Refresh | angular | arc tail → arrowhead | performs its meaning without a rotation transform |
| 17 | Download | directional | top→bottom, arrow before tray | payload arrives, tray catches; upload = negated axis |
| 18 | Edit | directional | along pencil axis tip-first, then underline L→R | action, not object |
| 19 | Terminal | directional + pulse | chevron sweep, then underscore blinks twice | the only icon that earns a repeat |
| 20 | Warning | radial | exclamation dot | attention starts at the alarm; single-shot, never shimmer |

## Taste rules

* One motion token scales every duration (reduced motion = 0 → instant).
* Hover reveals only; no idle animation, no loops (Terminal's two blinks
  are the sole exception).
* Warning/error icons animate once per appearance — alarms don't shimmer.
