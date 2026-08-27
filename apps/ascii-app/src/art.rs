//! The art: a character grid, and three ways to fill one.
//!
//! Everything here is a pure function of `(cols, rows, t)`. No state, no
//! frame counter, no animation loop — the clock the page already carries
//! decides *when*, and this decides *what*. That is what lets two clients
//! looking at the same widget see the same frame.

/// Dark to light. Ends in a space so an empty cell is genuinely empty
/// rather than a faint dot, which reads as dirt on a desktop.
const RAMP: &[u8] = b" .:-=+*#%@";

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<u8>,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Grid {
        Grid { cols, rows, cells: vec![b' '; cols * rows] }
    }

    /// Shade a cell by brightness, 0..=1. Brighter wins where two things
    /// land on the same cell, so an edge in front is not eaten by a wash
    /// behind it.
    pub fn shade(&mut self, x: i32, y: i32, value: f32) {
        let idx = (value.clamp(0.0, 1.0) * (RAMP.len() - 1) as f32).round() as usize;
        let ch = RAMP[idx];
        if x < 0 || y < 0 || x >= self.cols as i32 || y >= self.rows as i32 {
            return;
        }
        let slot = y as usize * self.cols + x as usize;
        let current = RAMP.iter().position(|c| *c == self.cells[slot]).unwrap_or(0);
        if idx >= current {
            self.cells[slot] = ch;
        }
    }

    pub fn lines(&self) -> Vec<String> {
        self.cells
            .chunks(self.cols)
            .map(|row| String::from_utf8_lossy(row).into_owned())
            .collect()
    }
}

/// A rotating wireframe cube. Vertices are projected with a plain
/// perspective divide and the twelve edges walked in small steps — no
/// depth buffer, just "nearer is brighter", which at this resolution is
/// all the depth cue there is room for.
pub fn cube(cols: usize, rows: usize, t: f32) -> Grid {
    let mut g = Grid::new(cols, rows);
    let (sy, cy) = (t * 0.9).sin_cos();
    let (sx, cx) = (t * 0.6).sin_cos();

    // Characters are about twice as tall as they are wide, so x gets twice
    // the scale or the cube comes out as a lozenge.
    let scale = (rows as f32 * 0.34).min(cols as f32 * 0.17);
    let (ox, oy) = (cols as f32 / 2.0, rows as f32 / 2.0);

    let corners: Vec<(f32, f32, f32)> = (0..8)
        .map(|i| {
            let (x, y, z) = (
                if i & 1 == 0 { -1.0 } else { 1.0 },
                if i & 2 == 0 { -1.0 } else { 1.0 },
                if i & 4 == 0 { -1.0 } else { 1.0 },
            );
            // Yaw then pitch.
            let (x, z) = (x * cy - z * sy, x * sy + z * cy);
            let (y, z) = (y * cx - z * sx, y * sx + z * cx);
            (x, y, z)
        })
        .collect();

    let project = |(x, y, z): (f32, f32, f32)| -> (f32, f32, f32) {
        let depth = 1.0 / (z + 3.2);
        (ox + x * scale * 2.0 * depth * 3.2, oy + y * scale * depth * 3.2, depth)
    };

    const EDGES: [(usize, usize); 12] = [
        (0, 1), (1, 3), (3, 2), (2, 0), // back face
        (4, 5), (5, 7), (7, 6), (6, 4), // front face
        (0, 4), (1, 5), (2, 6), (3, 7), // the struts between them
    ];
    for (a, b) in EDGES {
        let (ax, ay, ad) = project(corners[a]);
        let (bx, by, bd) = project(corners[b]);
        let steps = (((bx - ax).abs() + (by - ay).abs()) as usize).max(1);
        for i in 0..=steps {
            let f = i as f32 / steps as f32;
            let depth = ad + (bd - ad) * f;
            // Depth runs about 0.2 (far) to 0.45 (near) at this camera.
            let bright = ((depth - 0.18) / 0.28).clamp(0.15, 1.0);
            g.shade((ax + (bx - ax) * f).round() as i32, (ay + (by - ay) * f).round() as i32, bright);
        }
    }
    g
}

/// Layered sine waves rolling right to left. Each row asks how far it is
/// from the surface and shades accordingly, so the crest is bright and the
/// water under it fades — an ocean at eight characters of dynamic range.
pub fn wave(cols: usize, rows: usize, t: f32) -> Grid {
    let mut g = Grid::new(cols, rows);
    for x in 0..cols {
        let fx = x as f32 / cols.max(1) as f32;
        let surface = 0.5
            + 0.16 * (fx * 9.0 + t * 1.1).sin()
            + 0.09 * (fx * 17.0 - t * 1.7).sin()
            + 0.05 * (fx * 31.0 + t * 2.3).sin();
        let crest = surface * rows as f32;
        for y in 0..rows {
            let below = y as f32 - crest;
            if below < -0.5 {
                continue;
            }
            // Bright at the surface, fading with depth.
            let bright = (1.0 - below / (rows as f32 * 0.55)).clamp(0.0, 1.0);
            g.shade(x as i32, y as i32, bright * bright);
        }
    }
    g
}

/// Interfering sines — the oldest trick in demo graphics, and still the one
/// that fills a small grid with something worth glancing at.
pub fn plasma(cols: usize, rows: usize, t: f32) -> Grid {
    let mut g = Grid::new(cols, rows);
    for y in 0..rows {
        for x in 0..cols {
            // Characters are taller than wide; y is scaled to match so the
            // pattern comes out round rather than stretched.
            let (fx, fy) = (x as f32 / 6.0, y as f32 / 3.0);
            let v = (fx + t).sin() + (fy + t * 0.7).sin() + ((fx + fy) * 0.5 + t * 1.3).sin();
            g.shade(x as i32, y as i32, (v + 3.0) / 6.0);
        }
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(g: &Grid) -> (usize, usize) {
        let lines = g.lines();
        (lines.first().map(|l| l.chars().count()).unwrap_or(0), lines.len())
    }

    /// Every generator fills exactly the grid it was asked for — a widget
    /// that returned a different shape would tear its own layout.
    #[test]
    fn every_generator_fills_the_grid_it_was_given() {
        for (cols, rows) in [(40, 12), (7, 3), (120, 40)] {
            for g in [cube(cols, rows, 1.0), wave(cols, rows, 1.0), plasma(cols, rows, 1.0)] {
                assert_eq!(dims(&g), (cols, rows), "{cols}x{rows}");
            }
        }
    }

    /// They are animations: the same grid at two times is two pictures.
    #[test]
    fn the_frames_move() {
        for (name, a, b) in [
            ("cube", cube(48, 16, 0.0), cube(48, 16, 1.2)),
            ("wave", wave(48, 16, 0.0), wave(48, 16, 1.2)),
            ("plasma", plasma(48, 16, 0.0), plasma(48, 16, 1.2)),
        ] {
            assert_ne!(a.lines(), b.lines(), "{name} stood still");
        }
    }

    /// And they draw something: a generator that returned an empty grid
    /// would pass every test above.
    #[test]
    fn the_frames_have_ink_in_them() {
        for (name, g) in [
            ("cube", cube(48, 16, 0.4)),
            ("wave", wave(48, 16, 0.4)),
            ("plasma", plasma(48, 16, 0.4)),
        ] {
            let ink = g.lines().join("").chars().filter(|c| !c.is_whitespace()).count();
            assert!(ink > 20, "{name} drew {ink} characters");
        }
    }

    /// A degenerate size must not panic — a widget can be one character.
    #[test]
    fn a_tiny_grid_is_not_a_crash() {
        for g in [cube(1, 1, 0.5), wave(1, 1, 0.5), plasma(1, 1, 0.5)] {
            assert_eq!(g.lines().len(), 1);
        }
    }
}
