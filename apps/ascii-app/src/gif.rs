//! A GIF, played as ASCII.
//!
//! The whole point is *where the work happens*. Piping an image converter
//! into a terminal makes the terminal re-parse a screenful of escape
//! sequences, rebuild a document of one text node per cell, and ship it,
//! every single tick — for a picture that never changes after it is decoded.
//!
//! Here the file is decoded once, and each frame is turned into a grid of
//! characters once per grid size. After that a tick is a lookup: the widget
//! asks for the frame at this instant and gets a `Vec<String>` that already
//! exists. Nothing is recomputed for a frame that has been seen before, and
//! nothing at all is recomputed while the animation loops.
//!
//! Frames keep the GIF's own timing, so a loop runs at the speed it was made
//! at rather than at whatever the widget's clock happens to be.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use image::AnimationDecoder;
use image::codecs::gif::GifDecoder;

/// Darkest → lightest. Ten steps is more than a glyph grid can really show
/// at widget size, and fewer makes gradients band badly.
const RAMP: &[u8] = b" .:-=+*#%@";

/// A ceiling on decoded frames. A long GIF is a lot of grids to hold, and a
/// widget is not a media player — the first few seconds are the loop anyone
/// actually watches.
const MAX_FRAMES: usize = 240;

/// The luma a frame is kept at, per side. The renderer draws *characters* —
/// a few dozen columns — so holding every frame at the GIF's own resolution
/// was pure waste: an 8 MB, 240-frame file cost ~50 MiB of resident luma
/// for output that samples a couple of thousand cells. Box-averaged down at
/// decode, a frame keeps more detail than any character grid can show.
const MAX_LUMA_SIDE: u32 = 192;

/// Ceiling on luma held per file, whatever the frame count says. The frame
/// cap bounds time; this bounds memory, and memory is the budget that
/// matters on the machine this has to run on.
const LUMA_BUDGET: usize = 4 << 20;

/// How many character sizes to keep rendered per file. A resize drag asks
/// for a run of sizes, and each one holds the whole animation as strings —
/// unbounded, the drag itself became a leak.
const MAX_RENDERED_SIZES: usize = 3;

/// One decoded frame: luminance at the GIF's own resolution, plus how long
/// it is meant to be shown.
struct Decoded {
    w: u32,
    h: u32,
    /// Row-major luma, 0..=255.
    luma: Vec<u8>,
    delay_ms: f32,
}

/// A decoded file, and the grids rendered from it so far.
struct Entry {
    /// What the file was when it was read, so an edited GIF reloads.
    stamp: Option<SystemTime>,
    frames: Vec<Decoded>,
    /// Cumulative end time of each frame, for picking one by the clock.
    ends_ms: Vec<f32>,
    total_ms: f32,
    /// (cols, rows) → the whole animation as character grids.
    rendered: HashMap<(usize, usize), Vec<Vec<String>>>,
}

#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<PathBuf, Entry>>,
}

impl Cache {
    /// The frame to show at `now` seconds, as `rows` lines of `cols`
    /// characters. `Err` carries something printable for the widget to show
    /// instead — a missing or unreadable file should say so, not blank.
    pub fn frame(
        &self,
        path: &Path,
        cols: usize,
        rows: usize,
        now: f32,
    ) -> Result<Vec<String>, String> {
        let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());

        // Decode on first sight, or again if the file moved underneath us.
        //
        // A file we cannot stat right now is *not* treated as changed: a GIF
        // is often rewritten in place, and for the moment it is missing or
        // half-written a live widget should keep playing what it already has
        // rather than flickering to an error and back.
        let stale = match (&stamp, entries.get(path)) {
            (_, None) => true,
            (Some(now), Some(entry)) => entry.stamp.as_ref() != Some(now),
            (None, Some(_)) => false,
        };
        if stale {
            entries.insert(path.to_path_buf(), decode(path, stamp)?);
        }
        let entry = entries.get_mut(path).expect("just inserted");
        if entry.frames.is_empty() {
            return Err(format!("{}: no frames", path.display()));
        }

        // Render this grid size once. A widget is resized rarely and plays
        // constantly, so the cache hit is the normal case by a wide margin.
        if !entry.rendered.contains_key(&(cols, rows)) {
            // A drag sweeps through sizes; each holds the whole animation
            // as strings. Keep the last few, not the whole sweep.
            if entry.rendered.len() >= MAX_RENDERED_SIZES {
                entry.rendered.clear();
            }
            let grids = entry.frames.iter().map(|f| render(f, cols, rows)).collect();
            entry.rendered.insert((cols, rows), grids);
        }
        let grids = &entry.rendered[&(cols, rows)];

        // Which frame the clock is in, by the GIF's own timing. A pure
        // function of the wall clock, so two widgets showing the same file
        // are on the same frame without talking to each other.
        let at = if entry.total_ms > 0.0 {
            (now * 1000.0).rem_euclid(entry.total_ms)
        } else {
            0.0
        };
        let index = entry.ends_ms.partition_point(|end| *end <= at).min(grids.len() - 1);
        Ok(grids[index].clone())
    }
}

fn decode(path: &Path, stamp: Option<SystemTime>) -> Result<Entry, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let decoder = GifDecoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("{}: {e}", path.display()))?;

    let mut frames = Vec::new();
    let mut budget = LUMA_BUDGET;
    for frame in decoder.into_frames().take(MAX_FRAMES) {
        let frame = frame.map_err(|e| format!("{}: {e}", path.display()))?;
        // GIF delays are in units the crate normalises to a ratio; a zero
        // delay means "as fast as possible", which every viewer treats as
        // about 100ms rather than as a busy loop.
        let (num, den) = frame.delay().numer_denom_ms();
        let delay_ms = match den {
            0 => 100.0,
            d => (num as f32 / d as f32).max(10.0),
        };
        let rgba = frame.into_buffer();
        let (w, h) = rgba.dimensions();
        // Luma, with alpha folded toward black: a transparent GIF over a
        // dark desktop should read as empty, not as a bright rectangle.
        let luma: Vec<u8> = rgba
            .pixels()
            .map(|p| {
                let [r, g, b, a] = p.0;
                let lum = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
                (lum * (a as f32 / 255.0)) as u8
            })
            .collect();
        let (w, h, luma) = shrink(w, h, luma);
        budget = budget.saturating_sub(luma.len());
        frames.push(Decoded { w, h, luma, delay_ms });
        if budget == 0 {
            break;
        }
    }
    if frames.is_empty() {
        return Err(format!("{}: no frames", path.display()));
    }

    let mut ends_ms = Vec::with_capacity(frames.len());
    let mut total = 0.0;
    for f in &frames {
        total += f.delay_ms;
        ends_ms.push(total);
    }
    Ok(Entry { stamp, frames, ends_ms, total_ms: total, rendered: HashMap::new() })
}

/// Box-average a luma grid down so neither side exceeds [`MAX_LUMA_SIDE`].
/// Integer factors only: exactness is not worth a resampler here, the next
/// stop is a character cell.
fn shrink(w: u32, h: u32, luma: Vec<u8>) -> (u32, u32, Vec<u8>) {
    let f = (w.div_ceil(MAX_LUMA_SIDE)).max(h.div_ceil(MAX_LUMA_SIDE)).max(1);
    if f == 1 {
        return (w, h, luma);
    }
    let (nw, nh) = ((w / f).max(1), (h / f).max(1));
    let mut out = Vec::with_capacity((nw * nh) as usize);
    for y in 0..nh {
        for x in 0..nw {
            let mut sum = 0u32;
            let mut n = 0u32;
            for yy in y * f..((y + 1) * f).min(h) {
                for xx in x * f..((x + 1) * f).min(w) {
                    sum += luma[(yy * w + xx) as usize] as u32;
                    n += 1;
                }
            }
            out.push(sum.checked_div(n).unwrap_or(0) as u8);
        }
    }
    (nw, nh, out)
}

/// One frame → `rows` lines of `cols` characters.
///
/// Aspect is preserved by letterboxing rather than by stretching, because a
/// character cell is about twice as tall as it is wide: mapping the image
/// straight onto the grid would squash everything into a caricature.
fn render(frame: &Decoded, cols: usize, rows: usize) -> Vec<String> {
    // A cell's shape, from the same constants the widget lays out with.
    const CELL_ASPECT: f32 = super::MONO_ADVANCE / super::LINE_FACTOR;
    if cols == 0 || rows == 0 || frame.w == 0 || frame.h == 0 {
        return vec![String::new(); rows];
    }
    let image_aspect = frame.w as f32 / frame.h as f32;
    // How many cells the image wants, if it filled the height.
    let want_cols = (rows as f32 * image_aspect / CELL_ASPECT).round().max(1.0) as usize;
    let (fit_cols, fit_rows) = if want_cols <= cols {
        (want_cols, rows)
    } else {
        let scaled = (cols as f32 * CELL_ASPECT / image_aspect).round().max(1.0) as usize;
        (cols, scaled.min(rows))
    };
    let pad_x = (cols - fit_cols) / 2;
    let pad_y = (rows - fit_rows) / 2;

    let mut out = vec![" ".repeat(cols); rows];
    for row in 0..fit_rows {
        let mut line = String::with_capacity(cols);
        line.push_str(&" ".repeat(pad_x));
        // The source band this row of cells covers.
        let y0 = row * frame.h as usize / fit_rows;
        let y1 = (((row + 1) * frame.h as usize) / fit_rows).max(y0 + 1);
        for col in 0..fit_cols {
            let x0 = col * frame.w as usize / fit_cols;
            let x1 = (((col + 1) * frame.w as usize) / fit_cols).max(x0 + 1);
            // Mean luminance over the cell's whole source rect, so
            // downscaling averages rather than point-samples — point
            // sampling a GIF makes dithered areas flicker as it loops.
            let mut sum = 0u32;
            let mut n = 0u32;
            for y in y0..y1.min(frame.h as usize) {
                for x in x0..x1.min(frame.w as usize) {
                    sum += frame.luma[y * frame.w as usize + x] as u32;
                    n += 1;
                }
            }
            let mean = sum.checked_div(n).unwrap_or(0);
            let step = (mean as usize * (RAMP.len() - 1)) / 255;
            line.push(RAMP[step] as char);
        }
        line.push_str(&" ".repeat(cols - fit_cols - pad_x));
        out[pad_y + row] = line;
    }
    out
}
