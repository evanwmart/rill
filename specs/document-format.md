# Rill Document Format — Working Doc

Status: **draft / working doc**. Covers Application Phase 1 (milestone 8):
the compiled binary `.rill` document and its KDL source form. Same
conventions as every Rill format: big-endian, explicit lengths, strict
decoding, canonical encodings validated on read.

The binary format is the contract; the KDL source language is authoring UX,
compiled away at build time and swappable without a format change
(§8 decision 1).

---

## 1. Model

A document is four tables and a root reference:

```text
strings   deduplicated, sorted UTF-8 strings (text, paths, names)
styles    flat resolved property sets — no cascade, no inheritance
nodes     flat node table; children reference earlier indices
root      index of the root node
```

There is **no executable code and no external reference** except asset paths
(fetched through the same authorized resource pipeline as everything else).

## 2. File layout

```text
[ header 32B ][ string table ][ style table ][ state table ][ action table ][ node table ]
```

### Header (32 bytes)

| Offset    | Size | Field        | Value                          |
|-----------|------|--------------|--------------------------------|
| `[0..3]`  | 4    | magic        | `"RDOC"`                       |
| `[4]`     | 1    | version      | `0x01`                         |
| `[5..7]`  | 3    | reserved     | 0                              |
| `[8..11]` | 4    | total size   | u32, must equal the file size  |
| `[12..13]`| 2    | string count | u16                            |
| `[14..15]`| 2    | style count  | u16                            |
| `[16..19]`| 4    | node count   | u32 (≤ 65 536)                 |
| `[20..23]`| 4    | root index   | u32                            |
| `[24..25]`| 2    | state count  | u16                            |
| `[26..27]`| 2    | action count | u16                            |
| `[28..31]`| 4    | reserved     | 0                              |

The state and action tables (below) sit between the style table and the node
table. Both are empty in a purely static document. Document size limit: 16 MiB.

## 3. String table

`count` entries of `len:u16 + UTF-8 bytes`, **strictly ascending bytewise**
(canonical: enforces dedup; unsorted → reject). Everything textual lives
here: text runs, asset paths, link targets, style names, font families.

## 4. Style table

One entry per **resolved** style. Source-level layering (§7) is flattened by
the compiler; the runtime resolves a node's style with one table lookup and
no merging.

```text
name_idx  u16   into string table (debugging/inspection)
bitmap    u16   which properties are present; unknown bits → reject
fields, in bit order, only if present:
  bit 0  color          ColorRef (see below)
  bit 1  background     ColorRef
  bit 2  font_size      f32 (finite)
  bit 3  font_weight    u16 (1–1000)
  bit 4  corner_radius  f32 (finite, ≥ 0)
  bit 5  font_family    u16 string index
```

**ColorRef** is a 1-byte tag then its payload:

```text
tag 0  literal  RGBA8 (4 bytes)
tag 1  token    u16 string index — a semantic theme token (accent, surface, …)
```

A literal is a baked-in colour; a token is resolved by the client against the
active theme at render time (a named-token lookup, not a cascade — see
`specs/theming.md`). `font_family` may likewise name a font token (`ui`,
`mono`) resolved by the theme. Properties a style doesn't set fall to renderer
defaults. There is no inheritance from parent nodes — ever (§8 decision 3).

## 4a. State table

`state_count` entries — the document's mutable variables, the whole state space
of an interactive document (application-model.md §10). Empty for static
documents.

```text
name_idx  u16    into string table
value            a typed Value (below): the slot's initial contents and type
```

A **Value** is a 1-byte tag then its payload (the same encoding ACTION fields
use, protocol.md §7.5):

```text
tag 1  string  u16 len (≤ 1024) + UTF-8 bytes
tag 2  number  f64 big-endian, finite
tag 3  bool    1 byte (0 = false, 1 = true)
```

A slot's declared type is fixed by its initial value; later writes must match.

## 4b. Action table

`action_count` entries, referenced by Button/TextInput nodes by index. Each is
a 1-byte **kind** then its body:

```text
kind 1  Navigate   target_idx u16 (string; a valid resource path)
kind 2  SetState   state u16, Value (§4a) — type must match the slot
kind 3  Toggle     state u16 (must be a bool slot)
kind 4  Submit     endpoint_idx u16, field_count u8 (≤ 16),
                   then field_count × (name_idx u16, state u16)
kind 5  PickFile   into u16 (a string state slot) — request one file through
                   the capability broker; its text fills the slot
```

Submit gathers the named state slots into an ACTION request (protocol.md §7.5)
and renders the returned document. PickFile is the one brokered capability
(application-model.md §10): the client, not the app, chooses the file.

## 5. Dimensions (5 bytes)

```text
[0]     tag: 0 = auto, 1 = px, 2 = fill-weight
[1..4]  value: f32 BE, must be finite; auto ⇒ value must be 0; fill ⇒ > 0
```

Phase 1 compilers emit only `px` (and `auto`); `fill` is reserved for the
layout engine (§8 decision 4).

## 6. Node table

`count` entries of:

```text
type      u16
body_len  u16   bytes following
body      …     starts with style_ref u16 (0xFFFF = none) for every type
```

### Evolution rule (§8 decision 2)

```text
type 0x0000–0x7FFF   critical:  unknown → reject the document
type 0x8000–0xFFFF   ignorable: unknown → skip this node, render the rest
```

`body_len` is what makes skipping mechanical. `0x0000` is unassigned
(zero-buffer canary, as everywhere in Rill).

### Assigned types (version 1) — bodies after the style_ref

| Type     | Name      | Body fields                                        |
|----------|-----------|----------------------------------------------------|
| `0x0001` | Text      | value_idx u16                                      |
| `0x0002` | Image     | source_idx u16 (must be a valid resource path)     |
| `0x0003` | Row       | gap Dim, padding Dim, child_count u16, children u32×n |
| `0x0004` | Column    | same as Row                                        |
| `0x0005` | Rectangle | width Dim, height Dim                              |
| `0x0006` | Spacer    | size Dim                                           |
| `0x0007` | Link      | label_idx u16, target_idx u16 (valid path)         |
| `0x0008` | Scroll    | child u32                                          |
| `0x0009` | Button    | label_idx u16, action_idx u16 (into action table)  |
| `0x000A` | TextInput | bind u16 (string state), placeholder_idx u16, action u16 (`0xFFFF` = none, else fires on Enter), multiline u8 (0/1) |
| `0x000B` | When      | state u16 (bool), invert u8 (0/1), child u32       |
| `0x000C` | Icon      | name_idx u16, size Dim                             |
| `0x000D` | Chrome    | child u32                                          |
| `0x000E` | Key       | key_idx u16, target_idx u16 (`0xFFFF` = none), action u16 (`0xFFFF` = none) |
| `0x000F` | Menu      | item_count u8, then per item: label_idx u16, icon_idx u16, target_idx u16, action u16, flags u8 (bit0 danger, bit1 separator) |
| `0x0010` | Keys      | target_idx u16 (valid path)                        |
| `0x0011` | Live      | target_idx u16 (valid path), interval_ms u16 (floor-validated) |
| `0x0012` | Page      | ColorRef                                           |
| `0x0013` | Slider    | bind u16 (num state), min f32, max f32, step f32, action u16 (`0xFFFF` = none) |
| `0x0014` | Sensitive | tier u8 (1..=2; 0 and unknown refused)             |
| `0x8001` | Closing   | target_idx u16 (valid path)                        |

`Closing` is the first assignment in the **ignorable** half, deliberately:
it declares an action the host fires best-effort when the window closes,
and a viewer that predates it must skip the declaration and keep the app's
timeout behaviour — exactly the degradation the split exists to provide.
(`0x8000` itself stays unassigned, mirroring the `0x0000` canary.)

`Sensitive` (`0x0014`) is the counter-example that proves the split is
about consequence, not kind: it is also a declaration, but it sits in the
**critical** half, because a viewer that skipped it would record the page
at tier 0 — a fail-open on a classification control (specs/history.md
decision 4). Skipping `closing` loses a courtesy; skipping `sensitive`
loses a promise. Style refs: `0x000E`–`0x0012`, `0x0014` and `0x8001`
carry no style and must encode `style_ref = 0xFFFF`.

### Tree canonicalization (validated on decode)

* every child index is **strictly less** than its parent's index
  (post-order emission; makes cycles unrepresentable);
* every node except the root is referenced exactly once; the root is
  referenced zero times — the node table is a tree, not a DAG;
* a node's body must consume exactly `body_len` bytes;
* nesting is at most **256 deep**. Node count alone does not bound depth —
  65k single-child containers are a legal tree — and every consumer walks
  the tree by recursion, so an unbounded chain is a stack overflow, which
  aborts a process rather than failing a request. Depth is computed in the
  decoder's forward pass (children precede parents, so each node's height is
  known by the time it is read);
* the file must end exactly at `total size`.

## 7. Source form (KDL)

```kdl
style "heading" size=24 weight="bold" color="#e8e8f0"
style "card" background="#26263a" corner=8
style "serif" font="serif"

column gap=16 padding=12 {
    text "Hello from Rill" style="heading serif"
    image "/assets/moon.webp"
    rect width=320 height=2 style="card"
    link "Open notes" target="/private/notes"
}
```

* Top level: any number of `style` definitions and **exactly one** root UI
  node.
* `style="a b c"`: layered partial styles, merged left→right,
  **last listed wins**; the compiler prints a note for each override and
  emits one flattened style-table entry per distinct combination
  (deduplicated by resolved content).
* Values: numbers → px dimensions; `"auto"` → auto; colors `#rrggbb` /
  `#rrggbbaa`; weight `"normal"` (400), `"bold"` (700), or 1–1000.
* Unknown node names, unknown properties, unknown style references, and
  type mismatches are compile errors — never silently ignored.

## 8. Decisions (resolved 2026-08)

1. **KDL source** — authoring UX only; binary format is the contract;
   syntax swappable later without a format change.
2. **Critical/ignorable node-type split** with per-node length prefixes —
   the protocol-flags failure-mode design, ported. v1 assigns all types in
   the critical half.
3. **Layered partial named styles, compile-time flattening, last-listed
   wins** (with compiler notes); runtime = single style ID per node, one
   lookup, no cascade, no inheritance.
4. **Tagged f32 dimensions** (`auto | px | fill`), non-finite rejected at
   decode; Phase 1 emits px/auto only.

## 9. Verification (milestone 8 exit)

```bash
rill doc compile page.kdl --output first.rill
rill doc compile page.kdl --output second.rill
cmp first.rill second.rill        # deterministic compilation
rill doc inspect first.rill       # decoded tables and tree
```
