//! W1.4a — the measurement half of the owned text stack (D3).
//!
//! cosmic-text does **shaping only** (phase 1: per-segment advance widths);
//! segmentation and wrapping are the shared arithmetic in [`rill_ui::text`],
//! so this engine's numbers agree with the gpui backend's by construction —
//! same segmentation, same wrap, same line-height factor, and the same
//! cosmic-text shaper underneath (gpui uses it internally on Linux).
//!
//! Rasterization (swash → atlas) and the glyph pipeline are W1.4b/c.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Weight, Wrap};
use rill_ui::text::{
    LINE_HEIGHT_FACTOR, MONO_CANDIDATES, Prepared, SANS_CANDIDATES, SERIF_CANDIDATES, Segment,
    SegmentKind, split_runs, wrap_segments,
};
use rill_ui::{LineMetrics, TextMeasurer};

/// The OpenType weight axis. One tag, used by the scan below and by the
/// rasterizer in `atlas`.
pub(crate) const WGHT_AXIS: swash::Tag = swash::tag_from_bytes(b"wght");

#[derive(Clone, PartialEq, Eq, Hash)]
struct PrepKey {
    text: String,
    family: String,
    weight: u16,
    size_bits: u32,
}

/// Two-phase text engine over cosmic-text (mirrors the gpui backend's
/// `TextShaper`): `prepare` shapes a text once per (text, font, size) and
/// caches per-segment advances; measurement is then pure arithmetic.
/// `place_line` caches the same way on the drawing side, so a frame that
/// shows the same lines as the last one shapes nothing.
pub struct TextEngine {
    font_system: Mutex<FontSystem>,
    /// Lowercased installed family name → exact installed name.
    installed: HashMap<String, String>,
    default_family: Option<String>,
    serif_family: Option<String>,
    mono_family: Option<String>,
    /// Lowercased family → the weights it actually ships. A request for a
    /// weight a family does not have is matched to its nearest, rather than
    /// being allowed to fall out of the family entirely.
    family_weights: HashMap<String, Vec<u16>>,
    /// Lowercased family → it exposes a `wght` variation axis, so the
    /// rasterizer can produce that weight's real outlines instead of faking
    /// it. See [`TextEngine::has_variable_weight`].
    variable_weight: HashSet<String>,
    cache: Mutex<ByteCache<PrepKey, Prepared>>,
    placed: Mutex<ByteCache<PlaceKey, Vec<PlacedGlyph>>>,
}

/// Shaped-text cache, bounded by bytes rather than by entry count.
///
/// Entries are as big as the text they shaped — a terminal screen or a long
/// document body can be tens of kilobytes each — so a 4096-entry bound was a
/// bound on nothing in particular: a few hundred megabytes in the bad case, on
/// a device with a gigabyte. It also cleared wholesale when it filled, which
/// on a page that churns text means re-shaping everything still on screen.
///
/// This evicts the least recently used entries instead, and only down to a
/// low-water mark, so a clear-and-refill storm cannot repeat every few frames.
///
/// Two of these exist, one per half of the engine: `prepare` (advances, for
/// measuring) and `place_line` (positioned glyphs, for drawing). The second
/// came from a profile, not a hunch: with only the first in place, the
/// compositor spent 39% of its main thread re-shaping every visible line
/// on every frame (workstation, release, 2026-09-29, samply — see
/// docs/cognitive-load-2026-09-28.md). Text on a display changes rarely;
/// the work was almost all repeats.
struct ByteCache<K, V> {
    entries: HashMap<K, (Arc<V>, u64)>,
    /// Sum of [`ByteCache::entry_bytes`] over `entries`.
    bytes: usize,
    /// Monotonic access stamp — cheaper than moving entries in a list, and
    /// exact enough for "which half of this is cold".
    clock: u64,
}

/// What an entry costs, in the bytes that matter: the key's strings and the
/// value's per-item payload. Ignores allocator overhead and the fixed struct
/// sizes, which are small beside the text for anything worth caching.
trait CacheCost {
    fn cost(&self) -> usize;
}

impl CacheCost for PrepKey {
    fn cost(&self) -> usize {
        self.text.len() + self.family.len()
    }
}

impl CacheCost for Prepared {
    fn cost(&self) -> usize {
        self.segments.len() * std::mem::size_of::<Segment>()
    }
}

impl<K, V> Default for ByteCache<K, V> {
    fn default() -> Self {
        ByteCache { entries: HashMap::new(), bytes: 0, clock: 0 }
    }
}

impl<K: Clone + Eq + std::hash::Hash + CacheCost, V: CacheCost> ByteCache<K, V> {
    fn entry_bytes(key: &K, value: &V) -> usize {
        key.cost() + value.cost()
    }

    fn get(&mut self, key: &K) -> Option<Arc<V>> {
        self.clock += 1;
        let clock = self.clock;
        let (value, last_used) = self.entries.get_mut(key)?;
        *last_used = clock;
        Some(value.clone())
    }

    fn insert(&mut self, key: K, value: Arc<V>) {
        self.clock += 1;
        let cost = Self::entry_bytes(&key, &value);
        if let Some((old, _)) = self.entries.insert(key.clone(), (value, self.clock)) {
            self.bytes = self.bytes.saturating_sub(Self::entry_bytes(&key, &old));
        }
        self.bytes += cost;

        if self.bytes <= Self::BUDGET {
            return;
        }
        // Oldest first, dropping to the low-water mark rather than merely
        // under budget — stopping at the line would evict again next insert.
        let mut by_age: Vec<(u64, K)> =
            self.entries.iter().map(|(k, (_, used))| (*used, k.clone())).collect();
        by_age.sort_unstable_by_key(|(used, _)| *used);
        for (_, key) in by_age {
            if self.bytes <= Self::LOW_WATER {
                break;
            }
            if let Some((value, _)) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(Self::entry_bytes(&key, &value));
            }
        }
    }

    /// Enough for every distinct string on a busy screen several times over,
    /// and small enough to be unremarkable on the 1 GB target.
    const BUDGET: usize = 8 * 1024 * 1024;
    const LOW_WATER: usize = Self::BUDGET * 3 / 4;
}

/// Atkinson Hyperlegible Next (OFL 1.1 — see fonts/…/OFL.txt), variable
/// weight. Chosen for legibility as a first-class goal, which is the same
/// bet the whole interface makes.
const UI_FONT_REGULAR: &[u8] =
    include_bytes!("../../../fonts/atkinson-hyperlegible-next/AtkinsonHyperlegibleNext-Variable.ttf");
const UI_FONT_ITALIC: &[u8] = include_bytes!(
    "../../../fonts/atkinson-hyperlegible-next/AtkinsonHyperlegibleNext-Italic-Variable.ttf"
);
/// The matching mono cut, for code and paths. Upstream ships wght default
/// = 200; that is fine — the engine applies weight variations, so a 400
/// request interpolates to Regular. (Do not "fix" the fvar default: deltas
/// are relative to it, and moving it makes the base outlines unreachable.)
const UI_MONO_REGULAR: &[u8] = include_bytes!(
    "../../../fonts/atkinson-hyperlegible-mono/AtkinsonHyperlegibleMono-Variable.ttf"
);
const UI_MONO_ITALIC: &[u8] = include_bytes!(
    "../../../fonts/atkinson-hyperlegible-mono/AtkinsonHyperlegibleMono-Italic-Variable.ttf"
);

impl TextEngine {
    /// Load the system font database (one scan; keep the engine around),
    /// plus the UI face the binary carries. Bundling the face is what makes
    /// every install — and the eventual appliance — render identically: the
    /// system fonts are fallback, not the interface.
    pub fn new() -> TextEngine {
        let mut font_system = FontSystem::new();
        for bytes in [UI_FONT_REGULAR, UI_FONT_ITALIC, UI_MONO_REGULAR, UI_MONO_ITALIC] {
            font_system.db_mut().load_font_data(bytes.to_vec());
        }
        let mut installed = HashMap::new();
        let mut family_weights: HashMap<String, Vec<u16>> = HashMap::new();
        let mut variable_weight = HashSet::new();
        // Faces are keyed by id here and inspected below: `with_face_data`
        // needs its own borrow of the database, so the scan cannot hold the
        // `faces()` iterator across it.
        let face_ids: Vec<_> = font_system.db().faces().map(|f| f.id).collect();
        for id in face_ids {
            let Some(face) = font_system.db().face(id) else { continue };
            let families: Vec<String> = face.families.iter().map(|(n, _)| n.clone()).collect();
            let weight = face.weight.0;
            for name in &families {
                installed
                    .entry(name.to_lowercase())
                    .or_insert_with(|| name.clone());
                let weights = family_weights.entry(name.to_lowercase()).or_default();
                if !weights.contains(&weight) {
                    weights.push(weight);
                }
            }
            // Does this face carry a `wght` axis? A variable font registers
            // exactly one face — its default instance — so `family_weights`
            // above sees a single weight and would otherwise conclude the
            // family cannot do any other. It can; the rasterizer just has to
            // be told to move the axis.
            let varies = font_system
                .db()
                .with_face_data(id, |data, index| {
                    swash::FontRef::from_index(data, index as usize)
                        .map(|font| font.variations().find_by_tag(WGHT_AXIS).is_some())
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if varies {
                for name in &families {
                    variable_weight.insert(name.to_lowercase());
                }
            }
        }
        let pick = |candidates: &[&str]| -> Option<String> {
            candidates
                .iter()
                .find_map(|c| installed.get(&c.to_lowercase()).cloned())
        };
        let default_family = pick(SANS_CANDIDATES);
        let serif_family = pick(SERIF_CANDIDATES);
        let mono_family = pick(MONO_CANDIDATES);
        TextEngine {
            family_weights,
            variable_weight,
            font_system: Mutex::new(font_system),
            installed,
            default_family,
            serif_family,
            mono_family,
            cache: Mutex::new(ByteCache::default()),
            placed: Mutex::new(ByteCache::default()),
        }
    }

    /// Requested family → concrete installed family (same policy as the gpui
    /// backend's `FontBook::resolve`, over the same candidate lists). `None`
    /// falls through to cosmic-text's own generic matching.
    /// Whether a request lands on the monospace stack — the one family
    /// whose text is a *grid*. Terminal rows, code, readouts: their columns
    /// are the contract, and it holds only if measurement and placement
    /// agree to snap to cells.
    fn is_mono(&self, requested: &str) -> bool {
        matches!(requested.to_lowercase().as_str(), "monospace" | "mono")
    }

    /// One cell of the mono grid at this size: the font's own advance for
    /// `0`, which for the bundled cut is exactly the 0.632 em the terminal
    /// sizes its grid with. Derived from the font rather than a constant so
    /// the two can never drift apart.
    fn mono_cell(&self, font_size: f32, font_weight: u16) -> f32 {
        let p = self.prepare_natural("0", font_size, font_weight, "mono");
        let w: f32 = p.segments.iter().map(|s| s.width).sum();
        if w > 0.0 { w } else { font_size * 0.632 }
    }

    /// How many grid cells a piece of text occupies: wide characters take
    /// two, zero-width marks none, everything else one.
    fn cells_of(text: &str) -> usize {
        use unicode_width::UnicodeWidthChar;
        text.chars().map(|c| c.width().unwrap_or(1).min(2)).sum()
    }

    fn resolve(&self, requested: &str) -> Option<&str> {
        if requested.is_empty() {
            return self.default_family.as_deref();
        }
        match requested.to_lowercase().as_str() {
            "sans-serif" | "sans" => self.default_family.as_deref(),
            "serif" => self.serif_family.as_deref(),
            "monospace" | "mono" => self.mono_family.as_deref(),
            lower => self
                .installed
                .get(lower)
                .map(|s| s.as_str())
                .or(self.default_family.as_deref()),
        }
    }

    /// The nearest weight a family actually ships.
    ///
    /// A variable font registers one face at its default instance — the
    /// bundled mono cut's is ExtraLight (200) — and cosmic-text will not
    /// interpolate to a weight that face does not claim. Asking for 400
    /// therefore did not give a lighter mono: it left the family and landed
    /// on a *proportional* fallback, silently, everywhere `font="mono"` was
    /// used. Matching within the family first is what a font matcher is
    /// supposed to do; leaving the family is the last resort, not the first.
    fn nearest_weight(&self, family: &str, requested: u16) -> u16 {
        let Some(weights) = self.family_weights.get(&family.to_lowercase()) else {
            return requested;
        };
        if weights.contains(&requested) {
            return requested;
        }
        weights
            .iter()
            .copied()
            .min_by_key(|w| w.abs_diff(requested))
            .unwrap_or(requested)
    }

    /// Whether this family can be rendered at an arbitrary weight for real,
    /// by moving its `wght` axis.
    ///
    /// This is the honest alternative to [`TextEngine::weight_deficit`], and
    /// takes precedence over it: a variable face has genuine outlines at
    /// every weight, so there is nothing to fake. Only families that ship a
    /// fixed set of static cuts still need the synthetic smear.
    ///
    /// Note what this does *not* change: shaping still uses the face's
    /// default instance, so advances come from that instance. For the mono
    /// family — the one that needed this — advances are weight-invariant by
    /// definition, and the synthetic path deliberately preserved advances
    /// too, so nothing about metrics moves either way.
    pub fn has_variable_weight(&self, font_family: &str) -> bool {
        let Some(family) = self.resolve(font_family) else { return false };
        self.variable_weight.contains(&family.to_lowercase())
    }

    /// How much weight a family cannot supply, in weight units.
    ///
    /// A variable font registers exactly one face — its default instance —
    /// and cosmic-text 0.14 has no way to drive the `wght` axis, so the
    /// bundled mono cut is ExtraLight (200) and *nothing else*. Asking for
    /// Regular got 200 and looked it: every mono surface in Rill — the
    /// terminal, both widgets, every path and code span — was rendering
    /// thin, and a bold request vanished entirely.
    ///
    /// The deficit is what the renderer makes up by drawing the glyph twice
    /// a fraction of an em apart. Synthetic weight is a compromise, but it
    /// is the honest one available: the alternative is shipping static
    /// instances of a font we already ship.
    pub fn weight_deficit(&self, font_family: &str, requested: u16) -> u16 {
        let Some(family) = self.resolve(font_family) else { return 0 };
        requested.saturating_sub(self.nearest_weight(family, requested))
    }

    /// Phase 1 (cached): shape once, extract per-segment advance widths.
    pub fn prepare(
        &self,
        text: &str,
        font_size: f32,
        font_weight: u16,
        font_family: &str,
    ) -> Arc<Prepared> {
        let prepared = self.prepare_natural(text, font_size, font_weight, font_family);
        if !self.is_mono(font_family) {
            return prepared;
        }
        // The mono grid: every segment is worth its cells, not its shaped
        // advance. The bundled cut's own glyphs already advance exactly one
        // cell, so ASCII is untouched — this pins the glyphs that arrive
        // by *fallback*. Measured with a TUI on screen: box-drawing,
        // braille and symbol glyphs came in at 0.949 of a cell (arrows at
        // 1.326), so every border drifted five percent per character and
        // no vertical ever met its column.
        let cell = self.mono_cell(font_size, font_weight);
        let snapped = Prepared {
            segments: prepared
                .segments
                .iter()
                .map(|seg| {
                    let mut seg = *seg;
                    seg.width = TextEngine::cells_of(&text[seg.start..seg.end]) as f32 * cell;
                    seg
                })
                .collect(),
        };
        Arc::new(snapped)
    }

    fn prepare_natural(
        &self,
        text: &str,
        font_size: f32,
        font_weight: u16,
        font_family: &str,
    ) -> Arc<Prepared> {
        let family = self.resolve(font_family).unwrap_or("sans-serif").to_string();
        let font_weight = self.nearest_weight(&family, font_weight);
        let key = PrepKey {
            text: text.to_string(),
            family: family.clone(),
            weight: font_weight,
            size_bits: font_size.to_bits(),
        };
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return hit;
        }

        let mut fs = self.font_system.lock().unwrap();
        let mut segments = Vec::new();
        let mut paragraph_start = 0;
        for piece in text.split_inclusive('\n') {
            let (body, has_newline) = match piece.strip_suffix('\n') {
                Some(body) => (body, true),
                None => (piece, false),
            };
            if !body.is_empty() {
                // Shape the whole paragraph unwrapped; glyph advances carry
                // contextual shaping (kerning, ligatures) intact.
                let mut buffer =
                    Buffer::new(&mut fs, Metrics::new(font_size, font_size * LINE_HEIGHT_FACTOR));
                buffer.set_size(&mut fs, None, None);
                buffer.set_wrap(&mut fs, Wrap::None);
                let attrs = Attrs::new()
                    .family(Family::Name(&family))
                    .weight(Weight(font_weight));
                buffer.set_text(&mut fs, body, &attrs, Shaping::Advanced);
                buffer.shape_until_scroll(&mut fs, false);

                // (glyph start byte, advance width) in logical order.
                let glyphs: Vec<(usize, f32)> = buffer
                    .layout_runs()
                    .flat_map(|run| run.glyphs.iter().map(|g| (g.start, g.w)))
                    .collect();

                // Shared segmentation; segment width = sum of its glyph
                // advances (boundaries fall on whitespace transitions, which
                // are safe cluster boundaries).
                for (seg_start, seg_end, kind) in split_runs(body) {
                    let width: f32 = glyphs
                        .iter()
                        .filter(|(gs, _)| *gs >= seg_start && *gs < seg_end)
                        .map(|(_, w)| w)
                        .sum();
                    segments.push(Segment {
                        start: paragraph_start + seg_start,
                        end: paragraph_start + seg_end,
                        width: width.max(0.0),
                        kind,
                    });
                }
            }
            if has_newline {
                let at = paragraph_start + body.len();
                segments.push(Segment {
                    start: at,
                    end: at + 1,
                    width: 0.0,
                    kind: SegmentKind::Newline,
                });
            }
            paragraph_start += piece.len();
        }
        drop(fs);

        let prepared = Arc::new(Prepared { segments });
        self.cache.lock().unwrap().insert(key, prepared.clone());
        prepared
    }
}

/// A glyph positioned for painting, relative to its line box's top-left.
/// Snapped to whole device pixels — this is the renderer's half of the
/// pixels-vs-vectors split (layout stays in logical units; snapping is here).
pub(crate) struct PlacedGlyph {
    pub key: cosmic_text::CacheKey,
    pub x: i32,
    pub y: i32,
}

impl CacheCost for Vec<PlacedGlyph> {
    fn cost(&self) -> usize {
        self.len() * std::mem::size_of::<PlacedGlyph>()
    }
}

/// Everything `place_line`'s output depends on. `grid` is here because it
/// follows the *requested* family name (`mono`), not the resolved face: the
/// mono face asked for by its own name draws at shaped advances, the same
/// face asked for as `mono` draws on the cell grid.
#[derive(Clone, PartialEq, Eq, Hash)]
struct PlaceKey {
    text: String,
    family: String,
    weight: u16,
    size_bits: u32,
    grid: bool,
}

impl CacheCost for PlaceKey {
    fn cost(&self) -> usize {
        self.text.len() + self.family.len()
    }
}

impl TextEngine {
    /// Run `f` with the font system locked (the atlas rasterizer needs it).
    pub(crate) fn with_font_system<R>(&self, f: impl FnOnce(&mut FontSystem) -> R) -> R {
        f(&mut self.font_system.lock().unwrap())
    }

    /// Shape one already-wrapped line slice (no `\n`) and return its glyphs
    /// positioned relative to the line box's top-left, integer-snapped.
    /// Cached per (slice, face, weight, size, grid): the caller asks for
    /// every visible line on every frame, and nearly all of them are the
    /// lines it asked for last frame.
    pub(crate) fn place_line(
        &self,
        text: &str,
        font_size: f32,
        font_weight: u16,
        font_family: &str,
    ) -> Arc<Vec<PlacedGlyph>> {
        if text.is_empty() {
            return Arc::new(Vec::new());
        }
        let family = self.resolve(font_family).unwrap_or("sans-serif").to_string();
        // Same snap as measuring, or the glyphs drawn would come from a
        // different face than the widths that positioned them.
        let font_weight = self.nearest_weight(&family, font_weight);
        let on_grid = self.is_mono(font_family);
        let key = PlaceKey {
            text: text.to_string(),
            family: family.clone(),
            weight: font_weight,
            size_bits: font_size.to_bits(),
            grid: on_grid,
        };
        if let Some(hit) = self.placed.lock().unwrap().get(&key) {
            return hit;
        }
        // The grid cell, before the font lock: mono_cell measures through
        // prepare, which takes the same lock.
        let grid = on_grid.then(|| self.mono_cell(font_size, font_weight));
        let mut fs = self.font_system.lock().unwrap();
        let mut buffer =
            Buffer::new(&mut fs, Metrics::new(font_size, font_size * LINE_HEIGHT_FACTOR));
        buffer.set_size(&mut fs, None, None);
        buffer.set_wrap(&mut fs, Wrap::None);
        let attrs = Attrs::new()
            .family(Family::Name(&family))
            .weight(Weight(font_weight));
        buffer.set_text(&mut fs, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut fs, false);

        // The mono grid, drawing half: measurement already promised every
        // character its cells, so each glyph is drawn *in* its cell —
        // centred, since a fallback glyph narrower than the cell reads
        // best in the middle and one wider overhangs both sides equally.
        // Without this the shaped advances win and a box border walks off
        // its own corners.
        use unicode_width::UnicodeWidthChar;
        let mut out = Vec::new();
        for run in buffer.layout_runs() {
            // `line_y` is the baseline within the line box (cosmic positions
            // it from our Metrics, so line boxes match measurement).
            let baseline = run.line_y.round() as i32;
            match grid {
                None => {
                    for glyph in run.glyphs {
                        let physical = glyph.physical((0.0, 0.0), 1.0);
                        out.push(PlacedGlyph {
                            key: physical.cache_key,
                            x: physical.x,
                            y: baseline + physical.y,
                        });
                    }
                }
                Some(cell) => {
                    // Cells before each byte offset, so cluster starts map
                    // to grid columns.
                    let mut cells_before = vec![0usize; text.len() + 1];
                    let mut acc = 0usize;
                    for (i, ch) in text.char_indices() {
                        cells_before[i] = acc;
                        acc += ch.width().unwrap_or(1).min(2);
                        for j in i + 1..(i + ch.len_utf8()).min(text.len()) {
                            cells_before[j] = cells_before[i];
                        }
                    }
                    cells_before[text.len()] = acc;
                    for glyph in run.glyphs {
                        let start_cell = cells_before[glyph.start.min(text.len())] as f32;
                        let end_cell = cells_before[glyph.end.min(text.len())] as f32;
                        let span = (end_cell - start_cell).max(1.0) * cell;
                        let slot = start_cell * cell;
                        let centre = (span - glyph.w) / 2.0;
                        let physical = glyph.physical((0.0, 0.0), 1.0);
                        // The glyph's own bearing (physical.x - glyph.x) is
                        // kept; only the pen position moves to the cell.
                        let pen_shift = slot + centre - glyph.x;
                        out.push(PlacedGlyph {
                            key: physical.cache_key,
                            x: physical.x + pen_shift.round() as i32,
                            y: baseline + physical.y,
                        });
                    }
                }
            }
        }
        drop(fs);
        let placed = Arc::new(out);
        self.placed.lock().unwrap().insert(key, placed.clone());
        placed
    }
}

impl Default for TextEngine {
    fn default() -> Self {
        TextEngine::new()
    }
}

/// [`TextMeasurer`] over a shared [`TextEngine`] — arithmetic identical to the
/// gpui backend's `ShaperMeasurer`, over the shared wrap.
pub struct EngineMeasurer<'a>(pub &'a TextEngine);

impl TextMeasurer for EngineMeasurer<'_> {
    fn measure(
        &mut self,
        text: &str,
        font_size: f32,
        font_weight: u16,
        font_family: &str,
        max_width: f32,
    ) -> LineMetrics {
        let prepared = self.0.prepare(text, font_size, font_weight, font_family);
        let lines = wrap_segments(&prepared.segments, max_width.max(1.0));
        let line_height = font_size * LINE_HEIGHT_FACTOR;
        LineMetrics {
            width: lines.iter().fold(0.0f32, |w, line| w.max(line.width)),
            height: lines.len() as f32 * line_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// One engine per test run — `FontSystem::new()` scans the font dirs.
    fn engine() -> &'static TextEngine {
        static ENGINE: OnceLock<TextEngine> = OnceLock::new();
        ENGINE.get_or_init(TextEngine::new)
    }

    fn measure(text: &str, max_width: f32) -> LineMetrics {
        EngineMeasurer(engine()).measure(text, 16.0, 400, "sans-serif", max_width)
    }

    /// The cache is bounded by bytes because entries are the size of the text
    /// they shaped. Counting entries bounded nothing: a terminal screen or a
    /// long body is tens of kilobytes, and 4096 of those is a quarter of a
    /// gigabyte on a machine with one.
    ///
    /// Eviction is by age and stops at the low-water mark, so the cache cannot
    /// empty itself and re-shape everything still on screen.
    #[test]
    fn the_shape_cache_is_bounded_by_bytes_and_evicts_the_coldest() {
        let mut cache: ByteCache<PrepKey, Prepared> = ByteCache::default();
        let entry = |n: usize| {
            (
                PrepKey {
                    text: "x".repeat(n),
                    family: "sans-serif".into(),
                    weight: 400,
                    size_bits: 16f32.to_bits(),
                },
                Arc::new(Prepared { segments: Vec::new() }),
            )
        };

        // One entry, touched on every round so it stays the warmest thing in
        // the cache, and enough others to force eviction several times over.
        let (hot_key, hot) = entry(1024);
        cache.insert(hot_key.clone(), hot);
        let big = 256 * 1024;
        for i in 0..64 {
            assert!(cache.get(&hot_key).is_some(), "the hot entry was evicted at {i}");
            let (k, v) = entry(big + i); // distinct lengths => distinct keys
            cache.insert(k, v);
            assert!(
                cache.bytes <= ByteCache::<PrepKey, Prepared>::BUDGET,
                "over budget at {i}: {} bytes",
                cache.bytes
            );
        }
        // It really did evict — this pushed ~16 MiB through an 8 MiB cache.
        assert!(cache.entries.len() < 64, "nothing was evicted: {}", cache.entries.len());
        // And never emptied itself to get there.
        assert!(cache.bytes > ByteCache::<PrepKey, Prepared>::LOW_WATER / 2, "cache cleared rather than trimmed");
    }

    /// The drawing half is cached like the measuring half: the second ask
    /// for a line is the same allocation, and asking for the mono face on
    /// the grid (`mono`) and off it (by name) are two entries, since the
    /// glyph positions differ.
    #[test]
    fn placed_lines_are_cached_and_the_grid_is_part_of_the_key() {
        let engine = TextEngine::new();
        let a = engine.place_line("cache me", 16.0, 400, "sans-serif");
        let b = engine.place_line("cache me", 16.0, 400, "sans-serif");
        assert!(Arc::ptr_eq(&a, &b), "second placement re-shaped the line");
        assert!(!a.is_empty());
        let mono_name = engine.mono_family.clone().expect("bundled mono face registered");
        let on_grid = engine.place_line("|_|", 16.0, 400, "mono");
        let by_name = engine.place_line("|_|", 16.0, 400, &mono_name);
        assert!(!Arc::ptr_eq(&on_grid, &by_name), "grid and shaped placements shared an entry");
    }

    #[test]
    fn resolves_a_concrete_sans_family() {
        // The box has fonts installed; the shared candidate list should hit.
        let family = engine().resolve("sans-serif");
        assert!(family.is_some(), "no sans candidate resolved — fonts missing?");
        // Unknown names fall back to the default rather than vanishing.
        assert_eq!(engine().resolve("No Such Font 9000"), family);
    }

    #[test]
    fn segments_words_and_spaces_with_positive_widths() {
        let prepared = engine().prepare("aaa aaa", 16.0, 400, "sans-serif");
        let kinds: Vec<SegmentKind> = prepared.segments.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![SegmentKind::Word, SegmentKind::Space, SegmentKind::Word]);
        assert!(prepared.segments[0].width > 0.0);
        assert!(prepared.segments[1].width > 0.0);
        // Identical words shape to identical widths.
        assert!((prepared.segments[0].width - prepared.segments[2].width).abs() < 0.01);
        // Byte ranges cover the text exactly.
        assert_eq!((prepared.segments[0].start, prepared.segments[0].end), (0, 3));
        assert_eq!((prepared.segments[2].start, prepared.segments[2].end), (4, 7));
    }

    #[test]
    fn measure_wraps_at_narrow_widths() {
        let one_line = measure("hello hello", 10_000.0);
        assert_eq!(one_line.height, 16.0 * LINE_HEIGHT_FACTOR);
        assert!(one_line.width > 0.0);

        // Force a break between two identical words: a width just past one
        // word wraps to exactly two lines, each exactly one word wide
        // (trailing spaces excluded from line width).
        let word = measure("hello", 10_000.0);
        let two_lines = measure("hello hello", word.width + 1.0);
        assert_eq!(two_lines.height, 2.0 * 16.0 * LINE_HEIGHT_FACTOR);
        assert!((two_lines.width - word.width).abs() < 0.01);
    }

    /// The bundled face must actually win resolution — if it ever fails to
    /// load, the UI silently falls back to whatever the system has, and
    /// installs stop rendering identically.
    #[test]
    fn the_bundled_face_is_the_default_family() {
        let engine = TextEngine::new();
        assert_eq!(
            engine.default_family.as_deref(),
            Some("Atkinson Hyperlegible Next"),
            "bundled UI face resolves as the sans default"
        );
        assert_eq!(
            engine.mono_family.as_deref(),
            Some("Atkinson Hyperlegible Mono"),
            "and the bundled mono cut as the mono default"
        );
    }

    #[test]
    fn newlines_force_lines_and_multibyte_is_safe() {
        let two = measure("a\nb", 10_000.0);
        assert_eq!(two.height, 2.0 * 16.0 * LINE_HEIGHT_FACTOR);

        // Multi-byte text: no panics, sane segment boundaries.
        let prepared = engine().prepare("héllo wörld", 16.0, 400, "sans-serif");
        assert_eq!(prepared.segments.len(), 3);
        assert_eq!(prepared.segments[0].end, "héllo".len());

        // Empty text still measures one line box.
        let empty = measure("", 100.0);
        assert_eq!(empty.height, 16.0 * LINE_HEIGHT_FACTOR);
        assert_eq!(empty.width, 0.0);
    }

    /// The bundled mono cut registers exactly one face — ExtraLight (200) —
    /// because that is a variable font's default instance and fontdb does not
    /// enumerate the rest. Every mono surface therefore *looks* like it can
    /// only be 200, and for a long time it rendered that way.
    ///
    /// The escape is the `wght` axis, so this asserts the axis is really
    /// there. If the bundled font is ever swapped for a static cut this fails
    /// loudly, which is the point: the failure it guards against is silent —
    /// text quietly going thin again, or falling back to a synthetic smear
    /// nobody asked for.
    #[test]
    fn bundled_mono_is_one_static_weight_but_has_a_weight_axis() {
        let e = engine();
        // The trap: by static weights alone, mono can only do 200.
        assert_eq!(e.weight_deficit("mono", 200), 0);
        assert_eq!(e.weight_deficit("mono", 700), 500, "no static bold to match");
        // The escape: it varies, so the rasterizer can produce a real 700.
        assert!(e.has_variable_weight("mono"), "bundled mono lost its wght axis");
        assert!(e.has_variable_weight("sans"), "bundled sans lost its wght axis");
        // A family that does not vary must not claim to.
        assert!(!e.has_variable_weight("No Such Font 9000") || e.resolve("sans").is_some());
    }

    #[test]
    fn larger_font_measures_wider_and_taller() {
        let small = EngineMeasurer(engine()).measure("hello", 12.0, 400, "sans-serif", 1e6);
        let large = EngineMeasurer(engine()).measure("hello", 24.0, 400, "sans-serif", 1e6);
        assert!(large.width > small.width);
        assert_eq!(large.height, 24.0 * LINE_HEIGHT_FACTOR);
    }
}

#[cfg(test)]
mod grid_tests {
    use super::*;

    fn total(p: &Arc<rill_ui::text::Prepared>) -> f32 {
        p.segments.iter().map(|s| s.width).sum()
    }

    /// The mono grid: a character is worth its cells, whatever face it
    /// arrives from. ASCII in the bundled cut is untouched — its advance
    /// already is the cell — and fallback symbols stop drifting.
    #[test]
    fn mono_text_measures_in_cells() {
        let e = TextEngine::new();
        if e.resolve("mono").is_none() {
            eprintln!("no mono on this box; skipping");
            return;
        }
        let cell = e.mono_cell(14.0, 500);
        // Box drawing, braille, and symbols: one cell each, exactly.
        for s in ["──────", "││││││", "⠋⠙⠹⠸", "●✻⏺", "╭─╮"] {
            let w = total(&e.prepare(s, 14.0, 500, "mono"));
            let cells = TextEngine::cells_of(s) as f32;
            assert!(
                (w - cells * cell).abs() < 0.01,
                "{s:?}: measured {w}, wanted {cells} cells of {cell}"
            );
        }
        // ASCII agrees with its natural advance (the grid IS the font).
        let nat = total(&e.prepare_natural("abcdef", 14.0, 500, "mono"));
        let grid = total(&e.prepare("abcdef", 14.0, 500, "mono"));
        assert!((nat - grid).abs() < 0.01, "ASCII moved: natural {nat}, grid {grid}");
        // A wide character is two cells.
        let w = total(&e.prepare("你", 14.0, 500, "mono"));
        assert!((w - 2.0 * cell).abs() < 0.01, "wide char: {w} vs {}", 2.0 * cell);
        // And the sans stack is none of our business.
        let sans_nat = total(&e.prepare_natural("──────", 14.0, 400, "sans-serif"));
        let sans = total(&e.prepare("──────", 14.0, 400, "sans-serif"));
        assert!((sans_nat - sans).abs() < 0.01, "sans was gridded");
    }

    /// Placement agrees with measurement: in a mixed run the glyph after a
    /// fallback symbol sits exactly one cell along, not 0.949 of one.
    #[test]
    fn mono_glyphs_land_on_their_cells() {
        let e = TextEngine::new();
        if e.resolve("mono").is_none() {
            return;
        }
        let cell = e.mono_cell(14.0, 500);
        let glyphs = e.place_line("│a", 14.0, 500, "mono");
        assert_eq!(glyphs.len(), 2);
        // The second glyph's pen began at one cell; bearing keeps x near it.
        let dx = (glyphs[1].x - glyphs[0].x) as f32;
        assert!(
            (dx - cell).abs() <= 1.5,
            "glyph after a box char sits {dx} along, cell is {cell}"
        );
    }
}

#[cfg(test)]
mod advance_probe {
    use super::*;

    /// Diagnostic: what the resolved mono actually advances for the glyphs
    /// a TUI like Claude Code paints with.
    #[test]
    #[ignore = "diagnostic"]
    fn probe_tui_glyph_advances() {
        let engine = TextEngine::new();
        let f = 14.0;
        let total = |p: &std::sync::Arc<rill_ui::text::Prepared>| -> f32 { p.segments.iter().map(|seg| seg.width).sum() };
        let a = total(&engine.prepare("aaaaaaaaaa", f, 500, "mono")) / 10.0;
        eprintln!("resolved mono family: {:?}", engine.resolve("mono"));
        eprintln!("'a' advance: {a:.3} (MONO_ADVANCE would predict {:.3})", f * 0.632);
        for (label, s) in [
            ("light box ─", "──────────"),
            ("vert box │", "││││││││││"),
            ("corner ╭", "╭╭╭╭╭╭╭╭╭╭"),
            ("braille ⠋", "⠋⠋⠋⠋⠋⠋⠋⠋⠋⠋"),
            ("dot ●", "●●●●●●●●●●"),
            ("sparkle ✻", "✻✻✻✻✻✻✻✻✻✻"),
            ("record ⏺", "⏺⏺⏺⏺⏺⏺⏺⏺⏺⏺"),
            ("arrow ↑", "↑↑↑↑↑↑↑↑↑↑"),
            ("ellipsis …", "……………………………".get(..30).unwrap_or("…")),
            ("block ▪", "▪▪▪▪▪▪▪▪▪▪"),
        ] {
            let w = total(&engine.prepare(s, f, 500, "mono")) / s.chars().count() as f32;
            eprintln!("{label}: {w:.3}  (ratio to a: {:.3})", w / a);
        }
    }
}
