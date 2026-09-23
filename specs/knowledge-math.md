# Rill Knowledge — Math Rendering Working Doc

Status: **draft / working doc** (Sep 2026). Parser and Unicode tier built
and tested; the structural backend is specified here and **unbuilt**,
deliberately, until §6 has been measured on a real pack.

Related: [knowledge.md](knowledge.md) §4.3 (formulas kept as TeX source),
[document-format.md](document-format.md) (node types, styles — note that
file is stale against the code).

---

## 1. The problem

The ingest keeps formulas as TeX source: inline ones as `$…$` in the prose,
display ones as `$$…$$` in a paragraph of their own (knowledge.md §4.3).
The document format has no math node, so today a reader sees
`$$x={\frac {-b\pm {\sqrt {b^{2}-4ac}}}{2a}}$$` on screen, literally.

## 2. Why not render to an image (resolved 2026-09-23)

Rastering each formula server-side and emitting `image "/knowledge/expr/…"`
is the only route to typographically correct math, and it is the one option
ruled out on principle. architecture-advantages.md opens by naming the
project's actual claim — *"the compositor receives meaning, not pixels"* —
and adds that the accessibility tree and the agent surface are not features
to bolt on but the wire format itself. A rastered formula:

* cannot be found by desktop-wide live text search, which that same file
  calls the highest-value item on its list;
* does not survive semantic zoom (`scale_commands` re-rasterises text; an
  image only scales);
* is absent from the accessibility tree and the agent surface;
* is unselectable and defeats structural damage diffing;
* and, in a **search** app, is unfindable by the engine displaying it.

knowledge.md §3 already says the same thing in the pack's own terms: binary
never appears, `grep -R` is a supported interface. The decision matches the
one already taken for `Icon`, which is named rather than carrying path data
("an icon a document could draw itself would be arbitrary untrusted
geometry"): a constrained structured representation over expressive raw
geometry, accepting a smaller feature set as the price.

## 3. The constraint that shapes the design

From `rill-ui/src/layout.rs`, on wrapping rows:

> A wrapping row is a grid: children keep their own width and start a new
> line when the next one will not fit.

Children are atomic, and a `text` node takes no inline runs — one node, one
colour, one size, one family. So a prose paragraph decomposed into
`[text, math, text]` children wraps at **fragment** boundaries rather than
word ones, and a long text child claims a whole line. Inline math inside
flowing prose therefore *cannot* be a node tree, however good the math
layout is.

That is not an obstacle to route around; it is where the seam goes.

## 4. Tiers

```text
$$…$$   display, already its own paragraph   structural nodes   [PLANNED §7]
$…$     inline, inside flowing prose         Unicode, one node  [BUILT   §5]
neither parses                               the TeX source     [BUILT]
```

Inline math stays inside the single `text` node holding its paragraph, so
the paragraph keeps ordinary word wrap. Display math has no surrounding
prose and can afford a node tree. A formula that does not parse shows its
source in a marked style — never a half-rendering, which would be a wrong
formula shown confidently.

## 5. `rill-math` (built)

A crate with **no dependencies**: a parser over `&str` and two lookup
tables. TeX semantics sit below presentation, so both the knowledge build
tool (§6) and — later — the node emitter can use it without either pulling
in the other.

* `segments(&str) -> Vec<Segment>` splits a paragraph into prose, inline
  and display.
* `parse(&str) -> Option<Expr>` builds the expression tree.
* `to_unicode(&Expr) -> Option<String>` renders the one-dimensional subset.
* `inline(&str) -> Option<String>` is the two composed.

`None` everywhere means *outside the subset*, and the caller shows source.

### 5.1 Telling a formula from a price

`$$…$$` is unambiguous — no currency amount doubles the sign. A single `$`
is not: *"it cost $5 million and $3 billion"* puts ordinary prose between
two of them. `looks_like_math` is conservative, because a missed formula
renders as the prose it already was while a false positive eats a sentence.
A control sequence, a script marker or a math operator settles it; failing
those, only a run of at most three alphanumerics counts (`$x$`, `$ab$`).

The ASCII hyphen is deliberately **not** a signal: `$50,000-$60,000` is a
price range. `$a-b$` is missed as a result.

The robust fix is ingest-side: escape literal dollars when walking, so the
delimiters are unambiguous by construction. That changes the text shards
and so requires a pack rebuild — **open, §8.1**.

### 5.2 Subset

In, and rendering to Unicode: `^` `_` scripts, `\frac` (vulgar fractions
where one exists, else `a/b` with brackets when an operand is compound),
`\sqrt` (bracketing the same way), grouping, `\text`/`\mathrm` with spaces
kept, `\left(`/`\right)` as plain delimiters, spacing commands, the Greek
alphabet, and the relations, arrows and operators in `SYMBOLS`.

Out, falling back to source: matrices and anything `\begin{…}`, alignment,
`\sqrt[n]{…}` (a cube root shown as a square root is a wrong formula),
stretchy delimiters, `\over`, user macros, and any script whose characters
are missing from the Unicode script tables (`x^{q}` — there is no `q`
superscript, so the whole expression is refused rather than dropping it).

Sum and integral limits land *beside* the sign rather than above and below
it, which is the ordinary inline convention anyway: `\sum_{i=1}^{n} i`
renders `∑ᵢ₌₁ⁿi`.

### 5.3 What Unicode costs

Variables are upright, not italic. Fraction bars are a slash. Radicals do
not stretch. These are the price of one text node, and they are why display
math gets the structural tier instead.

## 6. Coverage — measure before building §7

`rill-knowledge-build math-coverage <pack-dir> [--show N]` scans every
chunk and reports, per tier, how many formulas parse, how many render, and
the commonest failures. The subset in §5.2 was chosen from what Simple
English articles looked likely to use, **not from what they do use**.

This is the same order as brute force before the tree (knowledge.md §2):
establish the ceiling, then decide what is worth building. The numbers
belong here once run:

```text
chunks scanned            —
chunks carrying a formula —
display: parsed / rendered —
inline:  parsed / rendered —
```

## 7. Structural backend (planned)

Display math becomes a node tree. `\frac` is a `column` of numerator, a
`rect` rule, denominator — `rect` being the only rule primitive a document
can draw. `\sqrt` is a `√` glyph plus a `rect` overline; it does not
stretch. Scripts are nested rows using `valign` and a smaller size token.

It lands in `rill-appkit` — every function there already returns a KDL
fragment — over `rill-math`, keeping `rill-knowledge` free of presentation
(knowledge.md §1.2).

Result-card snippets stay on the Unicode tier whatever §6 says: cards are
dense, and `snippet_of` collapses whitespace.

## 8. What the format is missing

Each is small, independently landable, and **not** a blocker: v0 degrades
visibly rather than badly without all three.

1. **No `italic` style bit.** The italic Atkinson faces are loaded
   (`rill-gpu/src/text.rs`) but `Attrs` carries family and weight only, so
   nothing can request them. Needs the property in the compiler allow-list,
   a field on `Style`, and a line to `cosmic_text::Style::Italic`.
   *Without it: variables render upright.*
2. **No baseline shift.** `valign` is top/center/bottom on a row; there is
   no fractional offset. *Without it: scripts are faked with valign plus a
   smaller size.*
3. **`theme.fonts_dir` is parsed and read by nothing**
   (`rill-viewport/src/theme.rs`). There is no supported way to ship glyph
   coverage for `√ ∑ ∫ ±`, so they depend on the host's installed fonts —
   against `rill-gpu/src/text.rs`'s own "system fonts are fallback, not the
   interface". *Without it: symbol coverage is not guaranteed on an
   appliance.*

## 9. Open

1. Escape literal `$` at ingest so delimiters are unambiguous, at the cost
   of a pack rebuild (§5.1).
2. Whether §7 earns its place at all, or whether the Unicode tier plus a
   marked-source fallback is enough. §6 decides.
3. Whether `rill-math` should own the KDL emitter too, rather than
   `rill-appkit`, if a non-appkit consumer appears.
