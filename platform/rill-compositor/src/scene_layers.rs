//! The vector wallpaper: layered SVG scenes that follow the day.
//!
//! `[desktop] wallpaper_scene = "path/scene.toml"` names a list of SVG layers, back
//! to front, each with a **depth** (0 = foreground, 1 = horizon) and an
//! optional **night** flag. The compositor flattens every path once, then
//! at each minute tints the layers from the two numbers the sky table gives
//! it (specs/theming.md): a layer's fills mix towards the sky colour by
//! depth — atmospheric perspective, distance fading into the air — and are
//! desaturated by the table's strength, more so the further away. A night
//! layer (windows lit, streetlamps) fades in as the sky's luminance drops
//! and is gone by day, so lights need no schedule of their own.
//!
//! The whole scene is filled contours in logical units drawn under every
//! window, cover-fitted to the output the way the pixel wallpaper is. It is
//! static between minutes, so damage gating keeps the GPU idle on it; it is
//! kilobytes of geometry, so a Pi paints it as cheaply as a flat colour.
//! The SVGs are meant to be swapped freely: paths with hex fills, one
//! viewBox, nothing else is read.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rill_ui::{Color, DrawCommand, Point, Rect, icons};

/// How far the farthest layer goes towards the horizon colour: nearly all
/// the way, so the last ridge is a breath darker than the sky it stands
/// against. The mix follows depth on a curve (see [`tint`]), so mid layers
/// keep most of their own colour and the fade gathers at the horizon.
const ATMOSPHERE: f32 = 0.9;
const ATMOSPHERE_CURVE: f32 = 1.5;
/// Layers at or beyond this depth are "far": the horizon haze paints over
/// them and under everything nearer.
const HAZE_SPLIT: f32 = 0.6;
const HAZE_BANDS: usize = 16;
/// Sky luminance below which night layers are fully on, and above which
/// they are fully off; a straight fade between.
const NIGHT_ON: f32 = 0.12;
const NIGHT_OFF: f32 = 0.34;
/// How much a daylight layer darkens when the sky goes dark: the ground
/// and the trees do not stay noon-bright under a night sky.
const NIGHT_DARKEN: f32 = 0.7;
/// Share of lit windows dark in any given minute, in per cent. A city at
/// night is never all on: a few lights go out and others come on.
const WINDOWS_DARK_PERCENT: u64 = 5;

/// Whether window `ri` of path `gi` in layer `li` is dark this `minute`.
/// A SplitMix64 hash of the four, so the set changes every minute and is
/// the same for every viewer of the same minute; nothing is remembered.
pub fn window_dark(minute: u64, li: usize, gi: usize, ri: usize) -> bool {
    let mut z = minute
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((li as u64) << 42)
        .wrapping_add((gi as u64) << 21)
        .wrapping_add(ri as u64);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z % 100 < WINDOWS_DARK_PERCENT
}
/// The sky is a band gradient: this many horizontal strips from the table's
/// colour at the top to a lighter horizon at the bottom. Forty-eight is
/// where the steps stop reading as steps on a 1080p glass; it is still
/// forty-eight rects.
const SKY_BANDS: usize = 48;
/// How far the horizon goes towards a pale warm white: strong, because a
/// real sky is lightest at the horizon and the silhouettes need it to sit
/// against.
const HORIZON_LIFT: f32 = 0.55;

/// What the compositor knows about the sky right now: the table's colour
/// and haze, plus where the sun is when the table says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkySample {
    pub color: Color,
    pub desat: f32,
    /// Hours since local midnight, fractional.
    pub hour: f32,
    /// Solar elevation in degrees, when the table carries it.
    pub elevation: Option<f32>,
    /// Where in the day's daylight this is: 0 at sunrise, 1 at sunset,
    /// outside that range at night. `None` when the table has no sunrise
    /// and sunset lines.
    pub day_frac: Option<f32>,
}

impl SkySample {
    /// The sample rounded to what the wallpaper can show: desaturation to
    /// the percent, the hour to the minute, elevation to a quarter degree,
    /// the daylight fraction to the minute of a twelve-hour day. Two
    /// samples equal after this paint the same picture (see `CacheKey`).
    pub fn quantised(self) -> SkySample {
        let q = |v: f32, step: f32| (v / step).round() * step;
        SkySample {
            color: self.color,
            desat: q(self.desat.clamp(0.0, 1.0), 0.01),
            hour: q(self.hour, 1.0 / 60.0),
            elevation: self.elevation.map(|e| q(e, 0.25)),
            day_frac: self.day_frac.map(|f| q(f, 1.0 / 720.0)),
        }
    }
}

/// Scene-wide settings from `[sky]` in the scene file.
#[derive(Clone, Copy)]
struct SkyConf {
    /// Where the horizon line sits, as a fraction of the output height.
    horizon: f32,
    /// Height of the horizon haze band, as a fraction of the output height.
    haze: f32,
    /// Peak opacity of the haze over the far layers, 0..1. Weather adds to
    /// it and night takes from it.
    haze_strength: f32,
    /// Whether to paint the sun.
    sun: bool,
}

impl Default for SkyConf {
    fn default() -> SkyConf {
        SkyConf { horizon: 0.62, haze: 0.22, haze_strength: 0.28, sun: true }
    }
}

/// When a layer is drawn, by the local date.
///
/// ```text
/// when = "02-14"              one day
/// when = "12-15..12-26"       a range; "12-31..01-01" wraps the year
/// when = "easter"             Easter Sunday and the two days before it
/// when = "no-holiday"         only when no dated layer in the scene matches
/// ```
/// `RILL_DATE=MM-DD` on the compositor overrides the date, for previews.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum When {
    Always,
    Range { from: (u8, u8), to: (u8, u8) },
    Easter,
    NoHoliday,
}

impl When {
    pub fn parse(s: &str) -> Option<When> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("easter") {
            return Some(When::Easter);
        }
        if s.eq_ignore_ascii_case("no-holiday") {
            return Some(When::NoHoliday);
        }
        if s.eq_ignore_ascii_case("always") || s.is_empty() {
            return Some(When::Always);
        }
        let md = |t: &str| {
            let (m, d) = t.split_once('-')?;
            let (m, d) = (m.parse::<u8>().ok()?, d.parse::<u8>().ok()?);
            ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((m, d))
        };
        match s.split_once("..") {
            Some((a, b)) => Some(When::Range { from: md(a)?, to: md(b)? }),
            None => {
                let d = md(s)?;
                Some(When::Range { from: d, to: d })
            }
        }
    }

    /// Whether this rule is a dated one (counts towards "a holiday is on").
    fn is_dated(&self) -> bool {
        matches!(self, When::Range { .. } | When::Easter)
    }

    /// `(year, month, day)` → does this rule fire? `NoHoliday` needs the
    /// scene's answer and returns false here.
    fn matches(&self, date: (i32, u8, u8)) -> bool {
        let (y, m, d) = date;
        match *self {
            When::Always => true,
            When::NoHoliday => false,
            When::Range { from, to } => {
                let x = (m, d);
                if from <= to { x >= from && x <= to } else { x >= from || x <= to }
            }
            When::Easter => {
                let (em, ed) = easter(y);
                // Sunday and the two days before, by day-of-year distance.
                let doy = |m: u8, d: u8| day_of_year(y, m, d);
                let (e, t) = (doy(em, ed), doy(m, d));
                t <= e && e - t <= 2
            }
        }
    }
}

/// Easter Sunday (Gregorian) as (month, day): the anonymous algorithm.
pub fn easter(y: i32) -> (u8, u8) {
    let a = y % 19;
    let b = y / 100;
    let c = y % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    (month as u8, day as u8)
}

fn day_of_year(y: i32, m: u8, d: u8) -> i32 {
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    days.iter().take(m.saturating_sub(1) as usize).sum::<i32>() + d as i32
}

/// The demo's holiday tour: with `RILL_DATE=cycle` and the time-lapse on,
/// each fake day takes the next date here — an ordinary day, then the
/// holidays in calendar order. The page server keeps the same list, so
/// the date on the glass and the lights agree.
pub fn cycle_date(year: i32, day_index: u64) -> (u8, u8) {
    let easter = easter(year);
    let dates = [(9, 26), (2, 14), (3, 17), easter, (6, 15), (7, 4), (10, 31), (12, 20), (12, 31)];
    dates[(day_index % dates.len() as u64) as usize]
}

/// Today's local date; `RILL_DATE=MM-DD` (this year) previews a day, and
/// `RILL_DATE=cycle` with `RILL_SKY_TIMELAPSE` walks [`cycle_date`].
pub fn local_date() -> (i32, u8, u8) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let secs = now as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    let year = tm.tm_year + 1900;
    match std::env::var("RILL_DATE").ok().as_deref() {
        Some("cycle") => {
            let period = std::env::var("RILL_SKY_TIMELAPSE").ok().and_then(|v| v.parse::<f64>().ok()).filter(|p| *p > 0.0);
            if let Some(p) = period {
                let (m, d) = cycle_date(year, (now / p).floor() as u64);
                return (year, m, d);
            }
        }
        Some(v) => {
            if let Some(When::Range { from, .. }) = When::parse(v) {
                return (year, from.0, from.1);
            }
        }
        None => {}
    }
    (year, (tm.tm_mon + 1) as u8, tm.tm_mday as u8)
}

struct Layer {
    depth: f32,
    night: bool,
    /// For a night layer: switch on as the sun's elevation drops below this
    /// many degrees (fading over a couple of degrees), instead of by the
    /// sky's luminance. `Some(10.0)` is about an hour before sunset,
    /// `Some(0.0)` sunset, `Some(-8.0)` about an hour after.
    on_below: Option<f32>,
    /// Which dates the layer is drawn on (see [`When`]).
    when: When,
    /// Scale about the bottom centre of the output after cover fit, and an
    /// offset as a fraction of the output size: composition without
    /// redrawing.
    scale: f32,
    dx: f32,
    dy: f32,
    /// One entry per SVG path element: its rings in viewBox units and its
    /// fill. A path of many small rings (a lights layer's windows) stays one
    /// draw command with many contours.
    rings: Vec<(Vec<Vec<Point>>, Color)>,
    view: (f32, f32, f32, f32),
}

/// The files a scene was built from, each with its mtime at load.
type Sources = Vec<(PathBuf, Option<SystemTime>)>;

/// One layer as named, before its SVG is read.
struct Entry {
    svg: String,
    depth: f32,
    night: bool,
    on_below: Option<f32>,
    when: When,
    scale: f32,
    dx: f32,
    dy: f32,
}

pub struct VectorScene {
    sky: SkyConf,
    layers: Vec<Layer>,
    /// Every file the scene was built from, with its mtime, so a swapped
    /// SVG is noticed without watching directories.
    sources: Sources,
    cache: Option<(CacheKey, Vec<DrawCommand>)>,
}

#[derive(Clone, Copy, PartialEq)]
struct CacheKey {
    w: u32,
    h: u32,
    sky: Color,
    /// Desaturation quantised to a percent: a table interpolated by the
    /// minute changes by less than that most minutes.
    desat_pct: u8,
    /// The minute of the day: the sun, haze and light gates all move with
    /// it, and a minute is the finest step the sky table is sampled at.
    minute: u16,
    /// Elevation to a quarter degree: sun-gated layers fade over 2.5°.
    elev_q: i16,
    /// The local date: holiday layers come and go with it.
    date: (i32, u8, u8),
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color { r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b), a: m(a.a, b.a) }
}

fn luminance(c: Color) -> f32 {
    (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0
}

/// A layer's fill under this sky: towards the sky by depth, then towards
/// grey by the atmosphere's desaturation, which thickens with distance.
/// `sky` is the table's colour (it decides night); `toward` is what the
/// far layers fade into — the horizon colour, since that is what stands
/// behind them.
pub fn tint(fill: Color, sky: Color, toward: Color, depth: f32, desat: f32) -> Color {
    let depth = depth.clamp(0.0, 1.0);
    // Night falls on the local colour first, and harder up close: the far
    // ridges keep the sky's glow, the foreground goes to shadow. Darkening
    // after the atmosphere mix would push every layer to the same black.
    let dark = Color { r: 0, g: 0, b: 0, a: fill.a };
    let local = mix(fill, dark, NIGHT_DARKEN * night_alpha(sky) * (1.0 - 0.5 * depth));
    let toward_sky = mix(local, Color { a: fill.a, ..toward }, depth.powf(ATMOSPHERE_CURVE) * ATMOSPHERE);
    let l = (luminance(toward_sky) * 255.0).round() as u8;
    let grey = Color { r: l, g: l, b: l, a: fill.a };
    mix(toward_sky, grey, desat.clamp(0.0, 1.0) * (0.35 + 0.65 * depth))
}

/// A night layer's fill under this sky: the same distance fade as land
/// (towards the horizon by depth, greyed by haze) with no night darkening —
/// these are the lights. Far windows dim into the sky; near ones burn.
pub fn tint_light(fill: Color, toward: Color, depth: f32, desat: f32) -> Color {
    let depth = depth.clamp(0.0, 1.0);
    let toward_sky = mix(fill, Color { a: fill.a, ..toward }, depth.powf(ATMOSPHERE_CURVE) * ATMOSPHERE);
    let l = (luminance(toward_sky) * 255.0).round() as u8;
    let grey = Color { r: l, g: l, b: l, a: fill.a };
    mix(toward_sky, grey, desat.clamp(0.0, 1.0) * (0.35 + 0.65 * depth))
}

/// How visible a night layer is under this sky: 1 in the dark, 0 by day.
pub fn night_alpha(sky: Color) -> f32 {
    let l = luminance(sky);
    ((NIGHT_OFF - l) / (NIGHT_OFF - NIGHT_ON)).clamp(0.0, 1.0)
}

impl VectorScene {
    /// Read `scene.toml` and every SVG it names (paths relative to the
    /// scene file). A scene with no drawable layer is an error, not an
    /// empty wallpaper: silence would hide a typo in a path.
    pub fn load(path: &Path) -> Result<VectorScene, String> {
        let mut sky = SkyConf::default();
        let (dir, entries, mut sources) = if path.is_dir() {
            Self::directory_entries(path)?
        } else {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let root: toml::Value = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let num = |t: &toml::Value, k: &str, d: f32| {
                t.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64))).unwrap_or(d as f64)
                    as f32
            };
            if let Some(sk) = root.get("sky") {
                sky = SkyConf {
                    horizon: num(sk, "horizon", 0.62).clamp(0.1, 0.95),
                    haze: num(sk, "haze", 0.22).clamp(0.0, 0.6),
                    haze_strength: num(sk, "haze_strength", 0.28).clamp(0.0, 0.9),
                    sun: sk.get("sun").and_then(|v| v.as_bool()).unwrap_or(true),
                };
            }
            let entries: Vec<Entry> = root
                .get("layer")
                .and_then(|v| v.as_array())
                .ok_or("no [[layer]] entries")?
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let svg = entry.get("svg").and_then(|v| v.as_str()).ok_or_else(|| format!("layer {i}: no svg"))?;
                    let night = entry.get("night").and_then(|v| v.as_bool()).unwrap_or(false);
                    let on_below = entry
                        .get("on_below")
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
                        .map(|d| d as f32);
                    let when = match entry.get("when").and_then(|v| v.as_str()) {
                        Some(w) => When::parse(w).ok_or_else(|| format!("layer {i}: bad when {w:?}"))?,
                        None => When::Always,
                    };
                    Ok(Entry {
                        svg: svg.to_string(),
                        depth: num(entry, "depth", 0.5),
                        night: night || on_below.is_some(),
                        on_below,
                        when,
                        scale: num(entry, "scale", 1.0).clamp(0.05, 4.0),
                        dx: num(entry, "x", 0.0),
                        dy: num(entry, "y", 0.0),
                    })
                })
                .collect::<Result<_, String>>()?;
            (dir, entries, vec![(path.to_path_buf(), mtime(path))])
        };
        let mut layers = Vec::new();
        for Entry { svg: svg_rel, depth, night, on_below, when, scale, dx, dy } in entries {
            let svg_path = dir.join(&svg_rel);
            let svg = std::fs::read_to_string(&svg_path).map_err(|e| format!("{}: {e}", svg_path.display()))?;
            let view = icons::viewbox(&svg).ok_or_else(|| format!("{}: no viewBox", svg_path.display()))?;
            let mut rings = Vec::new();
            for (paths, fill) in icons::paths(&svg) {
                let fill = fill.unwrap_or(Color { r: 128, g: 128, b: 128, a: 255 });
                let kept: Vec<Vec<Point>> = paths.into_iter().filter(|r| r.len() >= 3).collect();
                if !kept.is_empty() {
                    rings.push((kept, fill));
                }
            }
            if rings.is_empty() {
                return Err(format!("{}: no filled paths", svg_path.display()));
            }
            sources.push((svg_path.clone(), mtime(&svg_path)));
            layers.push(Layer { depth, night, on_below, when, scale, dx, dy, rings, view });
        }
        if layers.is_empty() {
            return Err("scene has no layers".into());
        }
        Ok(VectorScene { sky, layers, sources, cache: None })
    }

    /// A scene given as a directory: every `*.svg` in name order, back to
    /// front, so `01-sky-far.svg` … `06-front.svg` is the whole authoring
    /// step. Depth is spaced evenly from the horizon (0.9) to the front
    /// (0.1); a file whose name mentions `night`, `light` or `star` is a
    /// night layer. The directory itself is a source, so a file added or
    /// removed reloads the scene.
    fn directory_entries(dir: &Path) -> Result<(PathBuf, Vec<Entry>, Sources), String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.to_ascii_lowercase().ends_with(".svg"))
            .collect();
        names.sort();
        if names.is_empty() {
            return Err(format!("{}: no .svg files", dir.display()));
        }
        let n = names.len();
        let entries = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let depth = if n == 1 { 0.5 } else { 0.9 - 0.8 * i as f32 / (n - 1) as f32 };
                let lower = name.to_ascii_lowercase();
                let night = ["night", "light", "star"].iter().any(|k| lower.contains(k));
                Entry { svg: name, depth, night, on_below: None, when: When::Always, scale: 1.0, dx: 0.0, dy: 0.0 }
            })
            .collect();
        Ok((dir.to_path_buf(), entries, vec![(dir.to_path_buf(), mtime(dir))]))
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Forget that the sources moved: after a reload *failed*, the last good
    /// scene stays up and is not retried three times a second.
    pub fn refresh_stamps(&mut self) {
        for (p, m) in &mut self.sources {
            *m = mtime(p);
        }
    }

    /// True when the scene file or any SVG changed on disk since load.
    pub fn stale(&self) -> bool {
        self.sources.iter().any(|(p, m)| mtime(p) != *m)
    }

    /// The scene painted for this output and sky. Cached until the output,
    /// the colour, the desaturation (to the percent) or the sun (to a tenth
    /// of an hour) changes.
    pub fn commands(&mut self, out_w: f32, out_h: f32, sky: &SkySample) -> &[DrawCommand] {
        let key = CacheKey {
            w: out_w.max(0.0) as u32,
            h: out_h.max(0.0) as u32,
            sky: sky.color,
            desat_pct: (sky.desat.clamp(0.0, 1.0) * 100.0).round() as u8,
            minute: (sky.hour.rem_euclid(24.0) * 60.0).round() as u16 % 1440,
            elev_q: sky.elevation.map(|e| (e * 4.0).round() as i16).unwrap_or(i16::MIN),
            date: local_date(),
        };
        if self.cache.as_ref().is_none_or(|(k, _)| *k != key) {
            let cmds = self.paint(out_w, out_h, sky);
            self.cache = Some((key, cmds));
        }
        &self.cache.as_ref().expect("filled above").1
    }

    fn paint(&self, out_w: f32, out_h: f32, sample: &SkySample) -> Vec<DrawCommand> {
        let mut out = Vec::new();
        let desat = sample.desat.clamp(0.0, 1.0);
        let grey = Color { r: 128, g: 128, b: 128, a: 255 };
        let sky = mix(sample.color, grey, desat * 0.3);
        let night = night_alpha(sky);
        let horizon_y = out_h * self.sky.horizon;

        // The sky: darker at the zenith, lifting hard towards a warm pale
        // horizon. The warmth follows the sun: pink-orange near the horizon,
        // near white by day, and the lift collapses at night so the horizon
        // does not glow with no sun behind it.
        let low_sun = sample.elevation.map(|e| 1.0 - (e / 12.0).clamp(0.0, 1.0)).unwrap_or(0.0);
        let warm = mix(Color { r: 0xee, g: 0xf4, b: 0xfb, a: 255 }, Color { r: 0xff, g: 0xc8, b: 0x9a, a: 255 }, low_sun);
        let lift = HORIZON_LIFT * (1.0 - 0.8 * night) * (1.0 - 0.4 * desat);
        let zenith = mix(sky, Color { r: 0, g: 0, b: 0, a: 255 }, 0.14);
        let horizon = mix(sky, warm, lift);
        let band_h = out_h / SKY_BANDS as f32;
        for i in 0..SKY_BANDS {
            let y = i as f32 * band_h;
            // Lift concentrates towards the horizon line; below it the land
            // covers the sky anyway.
            let t = ((y + band_h * 0.5) / horizon_y).clamp(0.0, 1.0).powf(1.7);
            out.push(DrawCommand::Rect {
                rect: Rect { x: 0.0, y, w: out_w, h: band_h + 1.0 },
                color: mix(zenith, horizon, t),
                corner_radius: 0.0,
            });
        }

        // The sun: from the east edge at 6 to the west at 18, its height by
        // elevation, fading in across the horizon. A disc and three halo
        // rings — four polygons.
        if self.sky.sun
            && let Some(elev) = sample.elevation
            && elev > -3.0
        {
            let alpha = ((elev + 3.0) / 5.0).clamp(0.0, 1.0);
            // Across the output from sunrise to sunset when the table says
            // when those are; a 6-to-18 clock otherwise.
            let frac = sample.day_frac.unwrap_or((sample.hour - 6.0) / 12.0);
            let x = (0.06 + 0.88 * frac.clamp(0.0, 1.0)) * out_w;
            let up = (elev / 65.0).clamp(0.0, 1.0).powf(0.8);
            let y = horizon_y - up * horizon_y * 0.92;
            let r = out_h * 0.032;
            let disc = mix(Color { r: 0xff, g: 0xf6, b: 0xd8, a: 255 }, Color { r: 0xff, g: 0x9a, b: 0x4a, a: 255 }, low_sun);
            for (k, a) in [(4.2, 0.05), (2.8, 0.09), (1.7, 0.16)] {
                out.push(circle(x, y, r * k, Color { a: (255.0 * a * alpha) as u8, ..warm }));
            }
            out.push(circle(x, y, r, Color { a: (255.0 * alpha) as u8, ..disc }));
        }

        let date = local_date();
        let holiday_active = self.layers.iter().any(|l| l.when.is_dated() && l.when.matches(date));
        let drawn = |layer: &Layer| match layer.when {
            When::NoHoliday => !holiday_active,
            w => w.matches(date),
        };
        // Lit windows blink: each minute a few per cent of them, picked by
        // a hash of the minute and the window, are dark, and the next
        // minute a different few. The scene repaints on the minute anyway
        // (CacheKey), so this costs nothing between minutes.
        let minute = (sample.hour.rem_euclid(24.0) * 60.0).floor() as u64;
        let paint_layer = |out: &mut Vec<DrawCommand>, (li, layer): (usize, &Layer)| {
            if !drawn(layer) {
                return;
            }
            let alpha = match (layer.night, layer.on_below, sample.elevation) {
                (false, ..) => 1.0,
                // Sun-gated: fully on 2.5° below the threshold, off above it.
                (true, Some(th), Some(e)) => ((th - e) / 2.5).clamp(0.0, 1.0),
                (true, ..) => night,
            };
            if alpha < 0.02 {
                return;
            }
            let (vx, vy, vw, vh) = layer.view;
            // Cover fit: scale by the larger ratio, centre, crop the rest;
            // then the layer's own scale about the bottom centre and its
            // offset, so a traced skyline can be sat on the horizon smaller
            // than it was drawn.
            let fit = (out_w / vw).max(out_h / vh);
            let ox = (out_w - vw * fit) / 2.0 - vx * fit;
            let oy = (out_h - vh * fit) / 2.0 - vy * fit;
            let (cx, cy) = (out_w / 2.0, out_h);
            let (dx, dy) = (layer.dx * out_w, layer.dy * out_h);
            let windows = layer.on_below.is_some();
            for (gi, (group, fill)) in layer.rings.iter().enumerate() {
                let mut color = if layer.night {
                    tint_light(*fill, horizon, layer.depth, desat)
                } else {
                    tint(*fill, sky, horizon, layer.depth, desat)
                };
                color.a = (color.a as f32 * alpha).round() as u8;
                let mut points = Vec::new();
                let mut contours = Vec::with_capacity(group.len());
                for (ri, ring) in group.iter().enumerate() {
                    if windows && window_dark(minute, li, gi, ri) {
                        continue;
                    }
                    contours.push(ring.len() as u32);
                    points.extend(ring.iter().map(|p| {
                        let (x, y) = (ox + p.x * fit, oy + p.y * fit);
                        Point::new(cx + (x - cx) * layer.scale + dx, cy - (cy - y) * layer.scale + dy)
                    }));
                }
                if !contours.is_empty() {
                    out.push(DrawCommand::FillPath { points, contours, color });
                }
            }
        };

        // Far layers, then the horizon haze over them, then the rest.
        for layer in self.layers.iter().enumerate().filter(|(_, l)| l.depth >= HAZE_SPLIT) {
            paint_layer(&mut out, layer);
        }
        if self.sky.haze > 0.0 {
            // Ramps in over the band above the horizon line, then holds at
            // full strength to the bottom edge: everything far is under the
            // haze, and the near layers cover what the eye should not see.
            // Translucent bands must abut, not overlap: an overlapping pixel
            // row blends twice and draws a line. Edges are snapped to whole
            // logical pixels so neighbours share one edge exactly.
            let top = horizon_y - out_h * self.sky.haze;
            let hb = (horizon_y - top) / HAZE_BANDS as f32;
            let strength = (self.sky.haze_strength + 0.3 * desat).min(0.9) * (1.0 - 0.6 * night);
            let edge = |i: usize| (top + i as f32 * hb).round();
            for i in 0..HAZE_BANDS {
                let t = (i as f32 + 0.5) / HAZE_BANDS as f32;
                let (y0, y1) = (edge(i), edge(i + 1));
                out.push(DrawCommand::Rect {
                    rect: Rect { x: 0.0, y: y0, w: out_w, h: y1 - y0 },
                    color: Color { a: (255.0 * strength * t * t) as u8, ..horizon },
                    corner_radius: 0.0,
                });
            }
            let y0 = edge(HAZE_BANDS);
            out.push(DrawCommand::Rect {
                rect: Rect { x: 0.0, y: y0, w: out_w, h: out_h - y0 },
                color: Color { a: (255.0 * strength) as u8, ..horizon },
                corner_radius: 0.0,
            });
        }
        for layer in self.layers.iter().enumerate().filter(|(_, l)| l.depth < HAZE_SPLIT) {
            paint_layer(&mut out, layer);
        }
        out
    }
}

/// A filled circle as a 40-gon.
fn circle(cx: f32, cy: f32, r: f32, color: Color) -> DrawCommand {
    let n = 40;
    let points: Vec<Point> = (0..n)
        .map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            Point::new(cx + r * a.cos(), cy + r * a.sin())
        })
        .collect();
    DrawCommand::FillPath { points, contours: vec![n as u32], color }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Color = Color { r: 0x4a, g: 0x8f, b: 0xe0, a: 255 };
    const NIGHT: Color = Color { r: 0x05, g: 0x07, b: 0x0f, a: 255 };

    fn at(color: Color, desat: f32, hour: f32, elevation: f32) -> SkySample {
        SkySample { color, desat, hour, elevation: Some(elevation), day_frac: None }
    }
    fn day() -> SkySample {
        at(DAY, 0.0, 13.0, 55.0)
    }
    fn midnight() -> SkySample {
        at(NIGHT, 0.0, 0.0, -50.0)
    }
    fn fills(cmds: &[DrawCommand]) -> Vec<&DrawCommand> {
        cmds.iter().filter(|c| matches!(c, DrawCommand::FillPath { .. })).collect()
    }

    #[test]
    fn depth_carries_a_fill_towards_the_sky_and_desaturation_towards_grey() {
        let fill = Color { r: 200, g: 40, b: 40, a: 255 };
        assert_eq!(tint(fill, DAY, DAY, 0.0, 0.0), fill, "foreground keeps its colour");
        let far = tint(fill, DAY, DAY, 1.0, 0.0);
        assert!(far.b > far.r, "horizon layer takes the sky's blue: {far:?}");
        let spread = |c: Color| c.r.max(c.g).max(c.b) as i32 - c.r.min(c.g).min(c.b) as i32;
        // Full haze greys a foreground layer only partway (it keeps its
        // identity) and a horizon layer almost entirely.
        let near_grey = tint(fill, DAY, DAY, 0.0, 1.0);
        assert!(spread(near_grey) < spread(fill) && spread(near_grey) > 40, "near: {near_grey:?}");
        let far_grey = tint(fill, DAY, DAY, 1.0, 1.0);
        assert!(spread(far_grey) < 20, "far: {far_grey:?}");
        // Night darkens a foreground layer that the sky barely touches.
        let at_night = tint(fill, NIGHT, NIGHT, 0.0, 0.0);
        assert!(luminance(at_night) < luminance(fill) * 0.6, "night: {at_night:?}");
        // Distance thickens the air: the same desat greys a far layer more.
        let near = tint(fill, DAY, DAY, 0.2, 0.5);
        let far = tint(fill, DAY, DAY, 0.9, 0.5);
        assert!(spread(far) < spread(near));
    }

    #[test]
    fn night_layers_show_in_the_dark_and_not_by_day() {
        assert_eq!(night_alpha(NIGHT), 1.0);
        assert_eq!(night_alpha(DAY), 0.0);
        let dusk = Color { r: 0x50, g: 0x40, b: 0x60, a: 255 };
        let a = night_alpha(dusk);
        assert!(a > 0.0 && a < 1.0, "dusk fades: {a}");
    }

    fn scene_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rill-scene-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 50"><path d="M0 40 L100 40 L100 50 L0 50 Z" fill="#336699"/></svg>"##;
    const LIGHTS: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 50"><path d="M10 45 L12 45 L12 47 L10 47 Z" fill="#ffd27a"/></svg>"##;

    #[test]
    fn a_scene_loads_cover_fits_and_hides_its_lights_by_day() {
        let dir = scene_dir("load");
        std::fs::write(dir.join("ground.svg"), SQUARE).unwrap();
        std::fs::write(dir.join("lights.svg"), LIGHTS).unwrap();
        std::fs::write(
            dir.join("scene.toml"),
            "[[layer]]\nsvg = \"ground.svg\"\ndepth = 0.2\n[[layer]]\nsvg = \"lights.svg\"\ndepth = 0.2\nnight = true\n",
        )
        .unwrap();
        let mut scene = VectorScene::load(&dir.join("scene.toml")).expect("loads");
        assert!(!scene.stale());

        // A 200x100 output is the viewBox doubled: the ground strip's top
        // edge lands at y = 80, x spans the whole width.
        // No sun at midday for this check: the sun is four more polygons.
        let noon_no_sun = SkySample { elevation: None, ..day() };
        let day_cmds = scene.commands(200.0, 100.0, &noon_no_sun).to_vec();
        let f = fills(&day_cmds);
        assert_eq!(f.len(), 1, "lights hidden by day: {}", f.len());
        let DrawCommand::FillPath { points, .. } = f[0] else { unreachable!() };
        assert_eq!((points[0].x, points[0].y), (0.0, 80.0));
        assert_eq!((points[1].x, points[1].y), (200.0, 80.0));
        let rects = day_cmds.iter().filter(|c| matches!(c, DrawCommand::Rect { .. })).count();
        assert_eq!(rects, SKY_BANDS + HAZE_BANDS + 1);

        // A wider output crops top and bottom: cover, not stretch.
        let wide = scene.commands(400.0, 100.0, &noon_no_sun).to_vec();
        let DrawCommand::FillPath { points, .. } = fills(&wide)[0] else { unreachable!() };
        assert_eq!(points[1].x, 400.0);
        assert!(points[0].y > 100.0, "strip pushed below the crop: {}", points[0].y);

        // With the sun up, four more polygons: three halo rings and the disc.
        let with_sun = scene.commands(200.0, 100.0, &day()).to_vec();
        assert_eq!(fills(&with_sun).len(), 5);

        // At night the lights come on, at their own colour, and no sun.
        let night = scene.commands(200.0, 100.0, &midnight()).to_vec();
        let f = fills(&night);
        assert_eq!(f.len(), 2);
        let DrawCommand::FillPath { color, .. } = f[1] else { unreachable!() };
        // Lights take the distance fade but not the night darkening: at
        // depth 0.2 they stay warm and bright.
        assert!(color.r > 0xe0 && color.g > 0xb0 && color.r > color.b, "{color:?}");
        assert_eq!(color.a, 255);

        // Swapping an SVG is noticed.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("ground.svg"), LIGHTS).unwrap();
        let _ = filetime_touch(&dir.join("ground.svg"));
        assert!(scene.stale());
    }

    fn filetime_touch(p: &Path) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new().append(true).open(p)?;
        f.set_modified(SystemTime::now())
    }

    /// Not a check, a look: renders the placeholder scene at four skies to
    /// PNGs under `$RILL_SCENE_PREVIEW` (or the temp dir). `cargo test -p
    /// rill-compositor -- --ignored scene_preview`.
    #[test]
    fn a_few_windows_are_dark_each_minute_and_different_ones_the_next() {
        // Over a city's worth of windows the dark share sits near the
        // constant, and the set moves minute to minute rather than the
        // same windows staying out all night.
        let (layers, paths, windows) = (6, 40, 60);
        let count = |minute: u64| {
            let mut dark = Vec::new();
            for li in 0..layers {
                for gi in 0..paths {
                    for ri in 0..windows {
                        if window_dark(minute, li, gi, ri) {
                            dark.push((li, gi, ri));
                        }
                    }
                }
            }
            dark
        };
        let total = (layers * paths * windows) as f64;
        let (a, b) = (count(1200), count(1201));
        for d in [&a, &b] {
            let share = d.len() as f64 / total * 100.0;
            assert!((share - WINDOWS_DARK_PERCENT as f64).abs() < 1.0, "share {share:.2}%");
        }
        let same = a.iter().filter(|w| b.contains(w)).count();
        assert!(same < a.len() / 4, "{same} of {} windows stayed dark into the next minute", a.len());
        // And the LIGHTS scene paints fewer window rings at a minute whose
        // hash darkens its one window than at one that does not.
        let dir = scene_dir("blink");
        std::fs::write(dir.join("00-ground.svg"), SQUARE).unwrap();
        std::fs::write(dir.join("01-lights.svg"), LIGHTS).unwrap();
        std::fs::write(
            dir.join("scene.toml"),
            "[[layer]]\nsvg = \"00-ground.svg\"\ndepth = 0.5\n[[layer]]\nsvg = \"01-lights.svg\"\ndepth = 0.5\non_below = 0\n",
        )
        .unwrap();
        let mut scene = VectorScene::load(&dir.join("scene.toml")).unwrap();
        let rings_at = |scene: &mut VectorScene, hour: f32| {
            let mut s = midnight();
            s.hour = hour;
            scene
                .commands(200.0, 100.0, &s)
                .iter()
                .filter_map(|c| if let DrawCommand::FillPath { contours, .. } = c { Some(contours.len()) } else { None })
                .sum::<usize>()
        };
        let dark_minute = (0..1440u64).find(|m| window_dark(*m, 1, 0, 0)).expect("some minute darkens it");
        let lit_minute = (0..1440u64).find(|m| !window_dark(*m, 1, 0, 0)).expect("some minute leaves it lit");
        assert_eq!(rings_at(&mut scene, dark_minute as f32 / 60.0 + 0.001), 1, "ground only");
        assert_eq!(rings_at(&mut scene, lit_minute as f32 / 60.0 + 0.001), 2, "ground and the window");
    }

    #[test]
    #[ignore]
    fn scene_preview() {
        // `RILL_SCENE_DIR` previews any scene directory or file; the
        // placeholder set otherwise.
        let mut root = std::env::var_os("RILL_SCENE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/scenes/morning"));
        // A directory with a scene.toml previews as the theme would load it:
        // directory mode has no sun gates or date rules, so the lights would
        // all be on at once and none before dark.
        if root.is_dir() && root.join("scene.toml").is_file() {
            root = root.join("scene.toml");
        }
        let mut scene = VectorScene::load(&root).expect("scene loads");
        let out = std::env::var_os("RILL_SCENE_PREVIEW")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let renderer = rill_gpu::Renderer::new_headless().expect("a wgpu adapter");
        // `RILL_SCENE_FX=path.wgsl` previews an effect shader over the scene.
        if let Some(fx) = std::env::var_os("RILL_SCENE_FX") {
            let src = std::fs::read_to_string(fx).expect("effect shader readable");
            renderer.set_effect(Some(&src)).expect("effect compiles");
        }
        let samples = [
            ("dawn", at(Color { r: 0xef, g: 0xa9, b: 0x6d, a: 255 }, 0.0, 7.0, 3.0)),
            ("noon", at(Color { r: 0x49, g: 0x8e, b: 0xdf, a: 255 }, 0.0, 13.0, 55.0)),
            ("overcast", at(Color { r: 0x49, g: 0x8e, b: 0xdf, a: 255 }, 0.6, 15.0, 40.0)),
            ("golden", at(Color { r: 0xf0, g: 0xa8, b: 0x78, a: 255 }, 0.0, 17.8, 6.0)),
            ("dusk", at(Color { r: 0x8a, g: 0x4e, b: 0x6e, a: 255 }, 0.0, 18.9, -3.0)),
            ("night", at(Color { r: 0x0f, g: 0x17, b: 0x33, a: 255 }, 0.0, 22.0, -30.0)),
        ];
        // A timing pass first: the night scene (every layer on) at 1080p.
        let (_, night) = samples[samples.len() - 1];
        let cmds = scene.commands(1920.0, 1080.0, &night).to_vec();
        let points: usize = cmds
            .iter()
            .map(|c| match c {
                DrawCommand::FillPath { points, .. } => points.len(),
                _ => 0,
            })
            .sum();
        let _ = renderer.render_to_rgba(&cmds, &rill_gpu::NoImageSource, 1920, 1080, night.color);
        let t0 = std::time::Instant::now();
        for _ in 0..30 {
            let _ = renderer.render_to_rgba(&cmds, &rill_gpu::NoImageSource, 1920, 1080, night.color);
        }
        eprintln!(
            "night scene at 1080p: {} commands, {points} points, {:.2} ms per render (30 renders incl. readback)",
            cmds.len(),
            t0.elapsed().as_secs_f64() * 1000.0 / 30.0
        );
        // `RILL_SCENE_PREVIEW_SIZE=WxH` overrides the half-size default.
        let (w, h) = std::env::var("RILL_SCENE_PREVIEW_SIZE")
            .ok()
            .and_then(|v| v.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
            .unwrap_or((960u32, 540u32));
        for (name, sample) in samples {
            let cmds = scene.commands(w as f32, h as f32, &sample).to_vec();
            let rgba = renderer.render_to_rgba(&cmds, &rill_gpu::NoImageSource, w, h, sample.color);
            let path = out.join(format!("scene-{name}.png"));
            image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8).expect("png");
            eprintln!("wrote {}", path.display());
        }
    }

    #[test]
    fn a_directory_is_a_scene_in_name_order_with_lights_at_night() {
        let dir = scene_dir("dirmode");
        std::fs::write(dir.join("02-mid.svg"), SQUARE).unwrap();
        std::fs::write(dir.join("01-far.svg"), SQUARE).unwrap();
        std::fs::write(dir.join("03-lights.svg"), LIGHTS).unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let mut scene = VectorScene::load(&dir).expect("directory scene");
        assert_eq!(scene.layer_count(), 3);
        assert!((scene.layers[0].depth - 0.9).abs() < 1e-6, "first file is the horizon");
        assert!((scene.layers[2].depth - 0.1).abs() < 1e-6, "last file is the front");
        assert!(scene.layers[2].night && !scene.layers[1].night);
        // By day the lights layer is skipped; at night it paints.
        let noon_no_sun = SkySample { elevation: None, ..day() };
        let d = fills(scene.commands(200.0, 100.0, &noon_no_sun)).len();
        let n = fills(scene.commands(200.0, 100.0, &midnight())).len();
        assert_eq!((d, n), (2, 3));
    }

    #[test]
    fn a_sun_gated_layer_comes_on_as_the_sun_drops_past_its_threshold() {
        let dir = scene_dir("gated");
        std::fs::write(dir.join("ground.svg"), SQUARE).unwrap();
        std::fs::write(dir.join("lights.svg"), LIGHTS).unwrap();
        std::fs::write(
            dir.join("scene.toml"),
            "[sky]\nsun = false\n[[layer]]\nsvg = \"ground.svg\"\ndepth = 0.2\n[[layer]]\nsvg = \"lights.svg\"\ndepth = 0.2\non_below = 10\n",
        )
        .unwrap();
        let mut scene = VectorScene::load(&dir.join("scene.toml")).expect("loads");
        let alpha_at = |scene: &mut VectorScene, elev: f32| {
            let cmds = scene.commands(200.0, 100.0, &at(DAY, 0.0, 17.0, elev)).to_vec();
            let f = fills(&cmds);
            if f.len() < 2 {
                return 0u8;
            }
            let DrawCommand::FillPath { color, .. } = f[1] else { unreachable!() };
            color.a
        };
        assert_eq!(alpha_at(&mut scene, 30.0), 0, "high sun: off");
        let half = alpha_at(&mut scene, 8.75);
        assert!(half > 100 && half < 160, "halfway through the fade: {half}");
        assert_eq!(alpha_at(&mut scene, 5.0), 255, "well below: on, even under a bright sky");
    }

    #[test]
    fn date_rules_parse_and_match() {
        assert_eq!(When::parse("02-14"), Some(When::Range { from: (2, 14), to: (2, 14) }));
        assert_eq!(When::parse("12-31..01-01"), Some(When::Range { from: (12, 31), to: (1, 1) }));
        assert_eq!(When::parse("easter"), Some(When::Easter));
        assert_eq!(When::parse("13-01"), None);
        let r = When::parse("12-15..12-26").unwrap();
        assert!(r.matches((2026, 12, 20)) && !r.matches((2026, 12, 27)));
        let wrap = When::parse("12-31..01-01").unwrap();
        assert!(wrap.matches((2026, 12, 31)) && wrap.matches((2027, 1, 1)) && !wrap.matches((2026, 6, 1)));
        assert_eq!(easter(2026), (4, 5));
        assert_eq!(easter(2027), (3, 28));
        assert_eq!(easter(2024), (3, 31));
        assert!(When::Easter.matches((2026, 4, 5)) && When::Easter.matches((2026, 4, 3)));
        assert!(!When::Easter.matches((2026, 4, 2)) && !When::Easter.matches((2026, 4, 6)));
    }

    #[test]
    fn dated_layers_replace_the_default_lights_on_their_day() {
        let dir = scene_dir("holiday");
        std::fs::write(dir.join("ground.svg"), SQUARE).unwrap();
        std::fs::write(dir.join("lights.svg"), LIGHTS).unwrap();
        std::fs::write(dir.join("hearts.svg"), LIGHTS.replace("#ffd27a", "#ff4d6d")).unwrap();
        std::fs::write(
            dir.join("scene.toml"),
            "[sky]\nsun = false\n[[layer]]\nsvg = \"ground.svg\"\ndepth = 0.2\n\
             [[layer]]\nsvg = \"lights.svg\"\ndepth = 0.2\nnight = true\nwhen = \"no-holiday\"\n\
             [[layer]]\nsvg = \"hearts.svg\"\ndepth = 0.2\nnight = true\nwhen = \"02-13..02-14\"\n",
        )
        .unwrap();
        let mut scene = VectorScene::load(&dir.join("scene.toml")).expect("loads");
        let colour_of_lights = |scene: &mut VectorScene| {
            let cmds = scene.commands(200.0, 100.0, &midnight()).to_vec();
            let f = fills(&cmds);
            assert_eq!(f.len(), 2, "ground plus exactly one lights layer");
            let DrawCommand::FillPath { color, .. } = f[1] else { unreachable!() };
            (color.r, color.g, color.b)
        };
        // SAFETY: tests in this module run single-threaded per binary
        // default? No — set the override, read, unset, and accept that a
        // concurrent test would see it; none of the others read the date.
        unsafe { std::env::set_var("RILL_DATE", "02-14") };
        let heart = colour_of_lights(&mut scene);
        unsafe { std::env::set_var("RILL_DATE", "09-01") };
        scene.cache = None;
        let plain = colour_of_lights(&mut scene);
        unsafe { std::env::remove_var("RILL_DATE") };
        assert!(heart.0 > heart.2 && heart.1 < 0x90, "hearts are red: {heart:?}");
        assert!(plain.1 > 0xa0, "default lights are warm: {plain:?}");
        assert_ne!(heart, plain);
    }

    #[test]
    fn scale_and_offset_compose_a_layer_about_the_bottom_centre() {
        let dir = scene_dir("scale");
        std::fs::write(dir.join("ground.svg"), SQUARE).unwrap();
        std::fs::write(dir.join("scene.toml"), "[sky]\nsun = false\n[[layer]]\nsvg = \"ground.svg\"\ndepth = 0.2\nscale = 0.5\ny = -0.1\n").unwrap();
        let mut scene = VectorScene::load(&dir.join("scene.toml")).expect("loads");
        let noon_no_sun = SkySample { elevation: None, ..day() };
        let cmds = scene.commands(200.0, 100.0, &noon_no_sun).to_vec();
        let DrawCommand::FillPath { points, .. } = fills(&cmds)[0] else { unreachable!() };
        // Unscaled the strip's top-left is (0, 80); at half scale about
        // (100, 100) it is (50, 90), then lifted by a tenth of the height.
        assert_eq!((points[0].x, points[0].y), (50.0, 80.0));
        assert_eq!((points[1].x, points[1].y), (150.0, 80.0));
    }

    #[test]
    fn a_scene_with_a_missing_file_or_no_paths_is_refused() {
        let dir = scene_dir("bad");
        std::fs::write(dir.join("scene.toml"), "[[layer]]\nsvg = \"nope.svg\"\n").unwrap();
        assert!(VectorScene::load(&dir.join("scene.toml")).is_err());
        std::fs::write(dir.join("empty.svg"), r#"<svg viewBox="0 0 10 10"></svg>"#).unwrap();
        std::fs::write(dir.join("scene.toml"), "[[layer]]\nsvg = \"empty.svg\"\n").unwrap();
        assert!(VectorScene::load(&dir.join("scene.toml")).is_err());
    }
}
