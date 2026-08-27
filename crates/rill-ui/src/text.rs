//! Backend-shared text arithmetic (the parity core of D3, specs/wgpu-renderer.md).
//!
//! Layout correctness depends on every backend agreeing with `rill-ui::layout`
//! about line breaks to the pixel. That agreement is guaranteed by construction:
//! the segmentation ([`split_runs`]) and the wrap ([`wrap_segments`]) live
//! *here*, once, and every backend (gpui today, rill-gpu next) runs this exact
//! code over its own shaper's advance widths. A backend only supplies phase 1
//! (shaping: per-segment widths); phase 2 (wrapping) is shared arithmetic.

/// Uniform line height factor (font size → line box). Shared so every backend
/// produces the same line boxes.
pub const LINE_HEIGHT_FACTOR: f32 = 1.4;

/// Candidate concrete families per generic name, tried against the actually
/// installed family list. Shared so every backend resolves `sans-serif` (etc.)
/// to the same concrete font on the same machine.
pub const SANS_CANDIDATES: &[&str] = &[
    // The bundled UI face first: every install renders identically, and the
    // rest of the list is fallback for text the face lacks glyphs for.
    "Atkinson Hyperlegible Next",
    "Inter", "Ubuntu", "Cantarell", "DejaVu Sans", "Liberation Sans", "Noto Sans", "FreeSans",
    "Arial",
];
pub const SERIF_CANDIDATES: &[&str] = &[
    "DejaVu Serif", "Liberation Serif", "Noto Serif", "FreeSerif", "Times New Roman", "Georgia",
];
pub const MONO_CANDIDATES: &[&str] = &[
    // The bundled mono cut first — same face family, same guarantee.
    "Atkinson Hyperlegible Mono",
    "JetBrains Mono", "Fira Code", "DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono",
    "Ubuntu Mono", "FreeMono",
];

/// One break-opportunity unit of a prepared text (word, whitespace run, or a
/// forced newline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    pub width: f32,
    pub kind: SegmentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Word,
    Space,
    Newline,
}

/// Phase-1 output: per-segment advance widths from one full shaping pass
/// (kerning and ligatures intact). Wrapping thereafter is pure arithmetic.
#[derive(Debug)]
pub struct Prepared {
    pub segments: Vec<Segment>,
}

/// A wrapped line: byte range into the source text plus its visual width up
/// to the last word (trailing spaces excluded from width).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WrapLine {
    pub start: usize,
    pub end: usize,
    pub width: f32,
}

/// Split one paragraph body (no `\n`) into alternating whitespace / word runs,
/// as `(start, end, kind)` byte ranges. This is the segmentation both backends
/// feed their shaper widths into — segment boundaries always fall on
/// whitespace transitions, which are safe shaping-cluster boundaries.
pub fn split_runs(body: &str) -> Vec<(usize, usize, SegmentKind)> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut prev_ws: Option<bool> = None;
    for (offset, ch) in body.char_indices() {
        let ws = ch.is_whitespace();
        if let Some(p) = prev_ws
            && p != ws
        {
            runs.push((start, offset, if p { SegmentKind::Space } else { SegmentKind::Word }));
            start = offset;
        }
        prev_ws = Some(ws);
    }
    if let Some(p) = prev_ws {
        runs.push((start, body.len(), if p { SegmentKind::Space } else { SegmentKind::Word }));
    }
    runs
}

/// Fit tolerance for the wrap: a word overflowing by less than half a pixel
/// stays on its line. Without it, text painted into a rect sized *exactly* to
/// its measured width (links; any scaled frame) sits on a float knife-edge —
/// re-shaping at a scaled font size reproduces the measured width only to
/// within a few ulps, and the sign of the error decides the wrap, flickering
/// the last word across lines as the scale animates.
const WRAP_SLACK: f32 = 0.5;

/// Greedy arithmetic wrap over prepared segments — the phase-2 `layout()`
/// step: no shaping, no allocation beyond the output, safe to run per
/// frame. A word longer than `max_width` gets its own overflowing line
/// (no mid-word breaking in v1).
pub fn wrap_segments(segments: &[Segment], max_width: f32) -> Vec<WrapLine> {
    let max_width = max_width + WRAP_SLACK;
    let mut lines = Vec::new();
    let mut start: Option<usize> = None;
    let mut end = 0;
    let mut used = 0.0f32; // width consumed on the current line, incl. spaces
    let mut width = 0.0f32; // width up to the last word
    for seg in segments {
        match seg.kind {
            SegmentKind::Newline => {
                lines.push(WrapLine { start: start.unwrap_or(seg.start), end, width });
                start = None;
                (used, width) = (0.0, 0.0);
                end = seg.end;
            }
            SegmentKind::Space => {
                if start.is_none() {
                    start = Some(seg.start);
                }
                used += seg.width;
                end = seg.end;
            }
            SegmentKind::Word => {
                let fits = used + seg.width <= max_width;
                if !fits && width > 0.0 {
                    // Break before this word.
                    lines.push(WrapLine { start: start.unwrap(), end, width });
                    start = Some(seg.start);
                    used = 0.0;
                }
                if start.is_none() {
                    start = Some(seg.start);
                }
                used += seg.width;
                width = used;
                end = seg.end;
            }
        }
    }
    if let Some(s) = start {
        // The last line keeps its trailing spaces. Dropping them is right
        // for *wrapping* — trailing space must never push a word to the next
        // line — but the width reported back is how wide the box is, and a
        // run of spaces is as wide as it is. A terminal is the case that
        // proves it: a gap between two coloured runs measured zero, so every
        // column after it slid left. `used` is that width; `width` is the
        // width up to the last word, which is what the break logic needs.
        lines.push(WrapLine { start: s, end, width: used.max(width) });
    } else if lines.is_empty() {
        lines.push(WrapLine { start: 0, end, width: 0.0 });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{Segment, SegmentKind, split_runs, wrap_segments};

    fn word(start: usize, end: usize, width: f32) -> Segment {
        Segment { start, end, width, kind: SegmentKind::Word }
    }
    fn space(start: usize, end: usize, width: f32) -> Segment {
        Segment { start, end, width, kind: SegmentKind::Space }
    }

    #[test]
    fn wrap_arithmetic() {
        // "aaa bbb ccc": words 30px, spaces 10px.
        let segs = [
            word(0, 3, 30.0), space(3, 4, 10.0),
            word(4, 7, 30.0), space(7, 8, 10.0),
            word(8, 11, 30.0),
        ];
        // Everything fits on one line.
        let lines = wrap_segments(&segs, 200.0);
        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].start, lines[0].end), (0, 11));
        assert_eq!(lines[0].width, 110.0);
        // 75px: two words per line max (30+10+30=70 <= 75; +10+30 overflows).
        let lines = wrap_segments(&segs, 75.0);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].start, lines[0].end), (0, 8));
        assert_eq!(lines[0].width, 70.0);
        assert_eq!((lines[1].start, lines[1].end), (8, 11));
        // 35px: one word per line.
        let lines = wrap_segments(&segs, 35.0);
        assert_eq!(lines.len(), 3);
        // Oversized word overflows alone rather than vanishing.
        let lines = wrap_segments(&segs, 10.0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].width, 30.0);
    }

    #[test]
    fn wrap_handles_newlines_and_empty() {
        let segs = [
            word(0, 3, 30.0),
            Segment { start: 3, end: 4, width: 0.0, kind: SegmentKind::Newline },
            word(4, 7, 30.0),
        ];
        let lines = wrap_segments(&segs, 500.0);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].start, lines[0].end), (0, 3));
        assert_eq!((lines[1].start, lines[1].end), (4, 7));
        // Empty input still yields one empty line.
        assert_eq!(wrap_segments(&[], 100.0).len(), 1);
    }

    #[test]
    fn split_runs_alternates_words_and_spaces() {
        use SegmentKind::*;
        assert_eq!(
            split_runs("aaa bb  c"),
            vec![(0, 3, Word), (3, 4, Space), (4, 6, Word), (6, 8, Space), (8, 9, Word)]
        );
        // Leading/trailing whitespace keeps its own runs.
        assert_eq!(split_runs(" a "), vec![(0, 1, Space), (1, 2, Word), (2, 3, Space)]);
        // Multi-byte chars: boundaries stay on char boundaries.
        assert_eq!(split_runs("é ü"), vec![(0, 2, Word), (2, 3, Space), (3, 5, Word)]);
        assert_eq!(split_runs(""), vec![]);
    }
}
