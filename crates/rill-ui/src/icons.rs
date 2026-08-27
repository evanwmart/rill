//! Filled vector icons: Phosphor's SVG outlines flattened into contours.
//!
//! Phosphor ships each glyph as pre-outlined *filled* paths (`fill=
//! "currentColor"`, 256-unit viewBox) — so an icon is a set of closed
//! rings rendered even-odd by [`DrawCommand::FillPath`]
//! (crate::DrawCommand::FillPath). The parse is the same `d`-attribute
//! flattener the stroked era used; only the destination changed.
//!
//! Assets are vendored as plain SVGs under `crates/rill-ui/phosphor/`
//! (MIT — see the LICENSE beside them), pinned by scripts/vendor-icons.sh,
//! and only the names an app can ask for. Rill's icon names stay stable
//! across sets; the vendor script owns the mapping.
//!
//! Icons from Phosphor Icons (MIT), <https://phosphoricons.com>.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::Point;

/// Coordinate space the vendored set is drawn in.
pub const ICON_VIEWBOX: f32 = 256.0;

/// Longest curve segment produced when flattening, in viewBox units.
/// 2.5/256 ≈ the old 0.35/24: no visible faceting at UI sizes.
const FLATTEN_STEP: f32 = 2.5;

/// One icon: its closed rings, already flattened, in viewBox space.
pub struct Icon {
    pub contours: Vec<Vec<Point>>,
}

impl Icon {
    /// The icon's rings placed at `(x, y)` and scaled to `size`, in the
    /// packed form [`DrawCommand::FillPath`](crate::DrawCommand::FillPath)
    /// carries: one point list plus ring lengths.
    pub fn at(&self, x: f32, y: f32, size: f32) -> (Vec<Point>, Vec<u32>) {
        let scale = size / ICON_VIEWBOX;
        let mut points = Vec::new();
        let mut rings = Vec::new();
        for ring in &self.contours {
            rings.push(ring.len() as u32);
            points.extend(ring.iter().map(|p| Point::new(x + p.x * scale, y + p.y * scale)));
        }
        (points, rings)
    }
}

/// Vendored SVGs, by Rill's stable names.
const SOURCES: &[(&str, &str)] = &[
    ("folder", include_str!("../phosphor/folder.svg")),
    ("file", include_str!("../phosphor/file.svg")),
    ("home", include_str!("../phosphor/home.svg")),
    ("world", include_str!("../phosphor/world.svg")),
    ("lock", include_str!("../phosphor/lock.svg")),
    ("star", include_str!("../phosphor/star.svg")),
    ("trash", include_str!("../phosphor/trash.svg")),
    ("plus", include_str!("../phosphor/plus.svg")),
    ("minus", include_str!("../phosphor/minus.svg")),
    // The Rill mark itself — same fill pipeline as the set, so the dock's
    // corner glyph is a first-class icon, not a special case.
    ("rill-logo", include_str!("../rill-logo.svg")),
    ("pencil", include_str!("../phosphor/pencil.svg")),
    ("search", include_str!("../phosphor/search.svg")),
    ("dots-vertical", include_str!("../phosphor/dots-vertical.svg")),
    ("chevron-up", include_str!("../phosphor/chevron-up.svg")),
    ("chevron-down", include_str!("../phosphor/chevron-down.svg")),
    ("chevron-left", include_str!("../phosphor/chevron-left.svg")),
    ("chevron-right", include_str!("../phosphor/chevron-right.svg")),
    ("close", include_str!("../phosphor/close.svg")),
    ("list", include_str!("../phosphor/list.svg")),
    ("grid", include_str!("../phosphor/grid.svg")),
    ("refresh", include_str!("../phosphor/refresh.svg")),
    // Fill-weight variants: solid glyphs for the things that read better
    // as shapes than as outlines.
    ("folder-fill", include_str!("../phosphor/folder-fill.svg")),
    ("file-fill", include_str!("../phosphor/file-fill.svg")),
    ("home-fill", include_str!("../phosphor/home-fill.svg")),
    ("world-fill", include_str!("../phosphor/world-fill.svg")),
    ("lock-fill", include_str!("../phosphor/lock-fill.svg")),
    ("trash-fill", include_str!("../phosphor/trash-fill.svg")),
    // The sidebar places (upstream names in comments where ours differ).
    ("clock-fill", include_str!("../phosphor/clock-fill.svg")),
    ("star-fill", include_str!("../phosphor/star-fill.svg")),
    ("download-fill", include_str!("../phosphor/download-fill.svg")), // download-simple-fill
    ("file-text-fill", include_str!("../phosphor/file-text-fill.svg")),
    ("image-fill", include_str!("../phosphor/image-fill.svg")),
    ("film-fill", include_str!("../phosphor/film-fill.svg")), // film-strip-fill
    ("music-fill", include_str!("../phosphor/music-fill.svg")), // music-notes-fill
    ("music-note", include_str!("../phosphor/music-note.svg")),
    ("play", include_str!("../phosphor/play.svg")),
    ("pause", include_str!("../phosphor/pause.svg")),
    ("skip-back", include_str!("../phosphor/skip-back.svg")),
    ("skip-forward", include_str!("../phosphor/skip-forward.svg")),
    ("speaker", include_str!("../phosphor/speaker.svg")), // speaker-high
    ("speaker-mute", include_str!("../phosphor/speaker-mute.svg")), // speaker-simple-slash
];

/// The width of an SVG's viewBox, for normalizing differently-scaled
/// sources (Phosphor draws in 256; the Rill mark in 24) into one space.
fn viewbox_width(svg: &str) -> f32 {
    svg.find("viewBox=\"")
        .and_then(|i| {
            let rest = &svg[i + 9..];
            let end = rest.find('"')?;
            let mut parts = rest[..end].split_whitespace();
            let (_, _, w) = (parts.next()?, parts.next()?, parts.next()?);
            w.parse::<f32>().ok()
        })
        .filter(|w| *w > 0.0)
        .unwrap_or(ICON_VIEWBOX)
}

/// Every `d` attribute in an SVG, in document order.
fn d_attributes(svg: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = svg;
    while let Some(i) = rest.find(" d=\"") {
        let after = &rest[i + 4..];
        let Some(end) = after.find('"') else { break };
        out.push(&after[..end]);
        rest = &after[end..];
    }
    out
}

/// Look up an icon by name, flattening it on first use.
pub fn icon(name: &str) -> Option<&'static Icon> {
    static CACHE: OnceLock<HashMap<&'static str, Icon>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            SOURCES
                .iter()
                .map(|(name, svg)| {
                    // Normalize every source into ICON_VIEWBOX space, so an
                    // icon drawn in a 24-unit box sizes like the 256 set.
                    let scale = ICON_VIEWBOX / viewbox_width(svg);
                    let contours: Vec<Vec<Point>> = d_attributes(svg)
                        .iter()
                        .flat_map(|d| flatten(d))
                        .filter(|ring| ring.len() > 2)
                        .map(|ring| {
                            ring.into_iter()
                                .map(|p| Point::new(p.x * scale, p.y * scale))
                                .collect()
                        })
                        .collect();
                    (*name, Icon { contours })
                })
                .collect()
        })
        .get(name)
}

/// Every icon this build knows, for diagnostics and tests.
pub fn names() -> impl Iterator<Item = &'static str> {
    SOURCES.iter().map(|(name, _)| *name)
}

// ------------------------------------------------------------ path parsing

/// Flatten SVG path data into polylines — one per subpath.
///
/// Supports the commands the vendored set uses and the ones adjacent to them:
/// moves, lines, horizontal/vertical lines, cubic and quadratic curves,
/// elliptical arcs, and close. Unknown commands end the parse rather than
/// guessing, so a bad path draws less instead of drawing nonsense.
pub fn flatten(d: &str) -> Vec<Vec<Point>> {
    let mut lexer = Lexer { bytes: d.as_bytes(), pos: 0 };
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    let mut cursor = Point::new(0.0, 0.0);
    let mut start = cursor;
    let mut command = 0u8;

    loop {
        lexer.skip_separators();
        match lexer.peek() {
            None => break,
            Some(c) if c.is_ascii_alphabetic() => {
                command = c;
                lexer.pos += 1;
            }
            // A bare number repeats the previous command, as SVG allows.
            Some(_) if command != 0 => {}
            Some(_) => break,
        }
        let relative = command.is_ascii_lowercase();
        let rel = |v: f32, base: f32| if relative { base + v } else { v };

        match command.to_ascii_lowercase() {
            b'm' => {
                let Some((x, y)) = lexer.pair() else { break };
                if current.len() > 1 {
                    out.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
                cursor = Point::new(rel(x, cursor.x), rel(y, cursor.y));
                start = cursor;
                current.push(cursor);
                // Subsequent pairs after a move are implicit line-tos.
                command = if relative { b'l' } else { b'L' };
            }
            b'l' => {
                let Some((x, y)) = lexer.pair() else { break };
                cursor = Point::new(rel(x, cursor.x), rel(y, cursor.y));
                current.push(cursor);
            }
            b'h' => {
                let Some(x) = lexer.number() else { break };
                cursor = Point::new(rel(x, cursor.x), cursor.y);
                current.push(cursor);
            }
            b'v' => {
                let Some(y) = lexer.number() else { break };
                cursor = Point::new(cursor.x, rel(y, cursor.y));
                current.push(cursor);
            }
            b'c' => {
                let (Some((x1, y1)), Some((x2, y2)), Some((x, y))) =
                    (lexer.pair(), lexer.pair(), lexer.pair())
                else {
                    break;
                };
                let c1 = Point::new(rel(x1, cursor.x), rel(y1, cursor.y));
                let c2 = Point::new(rel(x2, cursor.x), rel(y2, cursor.y));
                let end = Point::new(rel(x, cursor.x), rel(y, cursor.y));
                cubic(cursor, c1, c2, end, &mut current);
                cursor = end;
            }
            b'q' => {
                let (Some((x1, y1)), Some((x, y))) = (lexer.pair(), lexer.pair()) else { break };
                let c = Point::new(rel(x1, cursor.x), rel(y1, cursor.y));
                let end = Point::new(rel(x, cursor.x), rel(y, cursor.y));
                // A quadratic is a cubic with the control points pulled two
                // thirds of the way in.
                let c1 = Point::new(
                    cursor.x + 2.0 / 3.0 * (c.x - cursor.x),
                    cursor.y + 2.0 / 3.0 * (c.y - cursor.y),
                );
                let c2 = Point::new(
                    end.x + 2.0 / 3.0 * (c.x - end.x),
                    end.y + 2.0 / 3.0 * (c.y - end.y),
                );
                cubic(cursor, c1, c2, end, &mut current);
                cursor = end;
            }
            b'a' => {
                let (Some(rx), Some(ry), Some(rot), Some(large), Some(sweep), Some((x, y))) = (
                    lexer.number(),
                    lexer.number(),
                    lexer.number(),
                    lexer.flag(),
                    lexer.flag(),
                    lexer.pair(),
                ) else {
                    break;
                };
                let end = Point::new(rel(x, cursor.x), rel(y, cursor.y));
                arc(cursor, rx, ry, rot, large, sweep, end, &mut current);
                cursor = end;
            }
            b'z' => {
                if !current.is_empty() {
                    current.push(start);
                    out.push(std::mem::take(&mut current));
                }
                cursor = start;
                current.push(cursor);
            }
            _ => break,
        }
    }
    if current.len() > 1 {
        out.push(current);
    }
    out
}

fn cubic(p0: Point, c1: Point, c2: Point, p3: Point, out: &mut Vec<Point>) {
    // Steps from the control polygon's length: long curves get more segments,
    // short ones do not pay for precision nobody can see.
    let rough = dist(p0, c1) + dist(c1, c2) + dist(c2, p3);
    let steps = ((rough / FLATTEN_STEP).ceil() as usize).clamp(2, 64);
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let u = 1.0 - t;
        out.push(Point::new(
            u * u * u * p0.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * p3.x,
            u * u * u * p0.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * p3.y,
        ));
    }
}

/// Endpoint-parameterised elliptical arc → polyline, following the SVG
/// implementation notes (F.6.5): recover the centre, then sample the sweep.
#[allow(clippy::too_many_arguments)]
fn arc(
    from: Point,
    rx: f32,
    ry: f32,
    rotation_deg: f32,
    large: bool,
    sweep: bool,
    to: Point,
    out: &mut Vec<Point>,
) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx < 1e-6 || ry < 1e-6 || (from.x == to.x && from.y == to.y) {
        out.push(to);
        return;
    }
    let phi = rotation_deg.to_radians();
    let (sin_phi, cos_phi) = phi.sin_cos();

    let dx2 = (from.x - to.x) / 2.0;
    let dy2 = (from.y - to.y) / 2.0;
    let x1 = cos_phi * dx2 + sin_phi * dy2;
    let y1 = -sin_phi * dx2 + cos_phi * dy2;

    // Scale up radii that are too small to span the endpoints (F.6.6).
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }

    let num = (rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1).max(0.0);
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let coef = if den <= 0.0 { 0.0 } else { (num / den).sqrt() };
    let coef = if large == sweep { -coef } else { coef };
    let cx1 = coef * rx * y1 / ry;
    let cy1 = -coef * ry * x1 / rx;

    let cx = cos_phi * cx1 - sin_phi * cy1 + (from.x + to.x) / 2.0;
    let cy = sin_phi * cx1 + cos_phi * cy1 + (from.y + to.y) / 2.0;

    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let dot = ux * vx + uy * vy;
        let len = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let start = angle(1.0, 0.0, (x1 - cx1) / rx, (y1 - cy1) / ry);
    let mut delta = angle(
        (x1 - cx1) / rx,
        (y1 - cy1) / ry,
        (-x1 - cx1) / rx,
        (-y1 - cy1) / ry,
    );
    if !sweep && delta > 0.0 {
        delta -= std::f32::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f32::consts::TAU;
    }

    let radius = rx.max(ry);
    let steps = (((delta.abs() * radius) / FLATTEN_STEP).ceil() as usize).clamp(2, 96);
    for i in 1..=steps {
        let t = start + delta * (i as f32 / steps as f32);
        let (sin_t, cos_t) = t.sin_cos();
        out.push(Point::new(
            cx + cos_phi * rx * cos_t - sin_phi * ry * sin_t,
            cy + sin_phi * rx * cos_t + cos_phi * ry * sin_t,
        ));
    }
}

fn dist(a: Point, b: Point) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(b' ' | b',' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn number(&mut self) -> Option<f32> {
        self.skip_separators();
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        // A second '.' starts a *new* number in SVG shorthand ("-.11.11" is
        // two numbers), so the dot is only consumed once.
        let mut seen_dot = false;
        while let Some(c) = self.peek() {
            match c {
                b'.' if !seen_dot => seen_dot = true,
                b'.' => break,
                c if c.is_ascii_digit() => {}
                _ => break,
            }
            self.pos += 1;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        std::str::from_utf8(&self.bytes[start..self.pos]).ok()?.parse().ok()
    }

    /// Arc flags are single digits and may be written without separators
    /// (`1 0` or `10`), so they cannot be read as ordinary numbers.
    fn flag(&mut self) -> Option<bool> {
        self.skip_separators();
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                Some(false)
            }
            Some(b'1') => {
                self.pos += 1;
                Some(true)
            }
            _ => None,
        }
    }

    fn pair(&mut self) -> Option<(f32, f32)> {
        Some((self.number()?, self.number()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.05
    }

    #[test]
    fn lines_absolute_and_relative() {
        let abs = flatten("M0 0 L10 0 L10 10");
        assert_eq!(abs.len(), 1);
        assert_eq!(abs[0].len(), 3);
        assert!(close(abs[0][2].x, 10.0) && close(abs[0][2].y, 10.0));

        // The same shape written relatively, with h/v shorthands.
        let rel = flatten("M0 0 h10 v10");
        assert!(close(rel[0][2].x, 10.0) && close(rel[0][2].y, 10.0));
    }

    #[test]
    fn a_move_starts_a_new_subpath() {
        let paths = flatten("M0 0 L5 0 M10 0 L15 0");
        assert_eq!(paths.len(), 2, "two strokes, not one joined line");
        assert!(close(paths[1][0].x, 10.0));
    }

    #[test]
    fn close_returns_to_the_start() {
        let paths = flatten("M2 2 h6 v6 z");
        let stroke = &paths[0];
        let first = stroke[0];
        let last = *stroke.last().unwrap();
        assert!(close(first.x, last.x) && close(first.y, last.y), "z closed the loop");
    }

    /// A half-circle arc: endpoints exact, and the middle bulges the way the
    /// sweep flag says. Arcs are the command that is easy to get subtly wrong.
    #[test]
    fn arcs_end_where_they_should_and_bulge_the_right_way() {
        let up = flatten("M0 0 a5 5 0 0 1 10 0");
        let stroke = &up[0];
        let end = *stroke.last().unwrap();
        assert!(close(end.x, 10.0) && close(end.y, 0.0), "arc lands on its endpoint");
        let mid = stroke[stroke.len() / 2];
        assert!(mid.y < -1.0, "sweep=1 bulges upward (negative y), got {}", mid.y);

        let down = flatten("M0 0 a5 5 0 0 0 10 0");
        let mid = down[0][down[0].len() / 2];
        assert!(mid.y > 1.0, "sweep=0 bulges downward, got {}", mid.y);
    }

    #[test]
    fn every_vendored_icon_flattens_to_something_drawable() {
        for name in names() {
            let icon = icon(name).unwrap_or_else(|| panic!("{name} missing"));
            assert!(!icon.contours.is_empty(), "{name} produced no contours");
            for ring in &icon.contours {
                assert!(ring.len() > 2, "{name} has a degenerate ring");
                for p in ring {
                    assert!(p.x.is_finite() && p.y.is_finite(), "{name} has a non-finite point");
                    // Phosphor draws inside 256x256; slack for curve
                    // overshoot, but a wild coordinate means bad parsing.
                    assert!(
                        (-8.0..=264.0).contains(&p.x) && (-8.0..=264.0).contains(&p.y),
                        "{name} point {p:?} escaped the viewBox"
                    );
                }
            }
        }
    }

    #[test]
    fn placing_an_icon_scales_it() {
        let folder = icon("folder").unwrap();
        let (points, rings) = folder.at(100.0, 50.0, 12.0);
        assert_eq!(rings.iter().map(|c| *c as usize).sum::<usize>(), points.len());
        for p in &points {
            assert!((99.0..=113.0).contains(&p.x), "x {} out of place", p.x);
            assert!((49.0..=63.0).contains(&p.y), "y {} out of place", p.y);
        }
    }
}

#[cfg(test)]
mod svg_shorthand {
    /// "-.11.11" is two numbers — the second dot starts a new one. The home
    /// glyph rendered as a sliver until the lexer learned this.
    #[test]
    fn a_second_dot_starts_a_new_number() {
        let rings = super::flatten("M0,0l-.11.11l10.5.5");
        assert_eq!(rings.len(), 1);
        let r = &rings[0];
        assert_eq!(r.len(), 3);
        assert!((r[1].x - -0.11).abs() < 1e-4 && (r[1].y - 0.11).abs() < 1e-4);
        assert!((r[2].x - 10.39).abs() < 1e-4 && (r[2].y - 0.61).abs() < 1e-4);
    }
}

#[cfg(test)]
mod logo_tests {
    /// The Rill mark rides the same fill pipeline as the set: it resolves,
    /// and flattens to real geometry (an empty outline would mean the
    /// flattener choked on the logo's curves).
    #[test]
    fn the_logo_is_a_first_class_icon() {
        let logo = super::icon("rill-logo").expect("rill-logo registered");
        let (points, contours) = logo.at(0.0, 0.0, 24.0);
        assert!(points.len() > 20, "logo flattened to {} points", points.len());
        assert!(!contours.is_empty());
    }
}
