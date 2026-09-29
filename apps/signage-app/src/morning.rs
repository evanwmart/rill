//! The morning board: the first page of the display someone would miss.
//!
//! This is the skeleton — greeting, name, date, clock — served
//! transparent so the glass's wallpaper (the vector scene that follows the
//! day, `sky.rs`) shows through. Weather, the agenda, holidays and stories
//! land here next, each read from a file a fetcher writes into the data
//! dir; the server itself never touches the network.
//!
//! `<data>/morning.toml` is the one config: who the board greets and where
//! it stands.
//!
//! ```toml
//! name = "Evan"
//! latitude = 34.15
//! longitude = -118.45
//! scene = "~/.config/rill/scenes/la/scene.toml"   # the type takes the layers' colours
//! messages_file = "~/.local/share/rill-signage/data/messages.md"   # optional; see messages.rs
//! ```
//!
//! The page carries `live every=60000` and a revision that is the minute,
//! so a poll between minutes is NOT_MODIFIED and costs a hash.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rill_auth::Identity;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

use crate::messages::Planner;
use crate::sky;
use crate::weather::{self, Forecast};

const LIVE_MS: u16 = 60_000;

/// `RILL_SKY_TIMELAPSE=<seconds>`: the same debug knob the compositor
/// honours for its wallpaper — a whole day in that many real seconds, by
/// the wall clock modulo the period, so the page's time agrees with the
/// sky behind it. Returns the instant the page should show and how often
/// it should re-read itself.
fn demo_clock(now: i64) -> (i64, u16) {
    static PERIOD: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    let period = *PERIOD.get_or_init(|| {
        std::env::var("RILL_SKY_TIMELAPSE").ok().and_then(|v| v.parse::<f64>().ok()).filter(|p| *p > 0.0)
    });
    match period {
        Some(p) => {
            let wall = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            let fake_secs = ((wall % p) / p * 86_400.0) as i64;
            let t = Local::at(now);
            let mut midnight = now - (t.hour * 3600 + t.minute * 60 + t.second);
            // `RILL_DATE=cycle`: each fake day is the next holiday date, the
            // same list the compositor walks (scene_layers::cycle_date).
            if std::env::var("RILL_DATE").ok().as_deref() == Some("cycle") {
                let (m, d) = cycle_date(t.year as i32, (wall / p).floor() as u64);
                if let Some(mid) = weather::iso_local_to_unix(&format!("{}-{m:02}-{d:02}T00:00", t.year), t.gmtoff) {
                    midnight = mid;
                }
            }
            // One poll per fake minute, so the clock ticks every minute of
            // the fast day rather than jumping twelve at a time; floored at
            // the document format's live interval.
            let every = ((p / 1440.0) * 1000.0).round().max(rill_doc::MIN_LIVE_INTERVAL_MS as f64) as u16;
            (midnight + fake_secs, every)
        }
        // `RILL_CLOCK_OFFSET=<seconds>`: real speed, shifted — a demo that
        // starts at sunset. The compositor reads the same variable, so the
        // two agree with no coordination (scene_clock_seconds).
        None => (now + clock_offset(), LIVE_MS),
    }
}

/// `RILL_CLOCK_OFFSET` in seconds, parsed once; 0 when unset or unusable.
fn clock_offset() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| std::env::var("RILL_CLOCK_OFFSET").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// The demo's holiday tour, kept identical to the compositor's list.
fn cycle_date(year: i32, day_index: u64) -> (u8, u8) {
    let easter = easter(year);
    let dates = [(9, 26), (2, 14), (3, 17), easter, (6, 15), (7, 4), (10, 31), (12, 20), (12, 31)];
    dates[(day_index % dates.len() as u64) as usize]
}

/// Easter Sunday (Gregorian) as (month, day).
fn easter(y: i32) -> (u8, u8) {
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
    (((h + l - 7 * m + 114) / 31) as u8, ((h + l - 7 * m + 114) % 31 + 1) as u8)
}
const FACE: &str = "Urbanist";

/// `morning.toml`, as much of it as exists today.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// `messages_file = "path"`: the corpus; default `<data>/messages.md`,
    /// built-in lines when neither exists (see `messages.rs`).
    pub messages_file: Option<std::path::PathBuf>,
    /// `scene = "path/scene.toml"`: the wallpaper scene whose layer colours
    /// the type is set in, far layer first.
    pub scene: Option<std::path::PathBuf>,
}

impl Config {
    /// A missing file is a board with defaults, not an error: the glass
    /// should show *something* on first boot. A malformed one is reported.
    pub fn load(path: &Path) -> Config {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Config::default_board(),
        };
        let table: toml::Value = match text.parse() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("signage-app: {}: {e}", path.display());
                return Config::default_board();
            }
        };
        let num = |k: &str| table.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)));
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
        let path_of = |p: &str| match p.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => std::path::PathBuf::from(p),
        };
        let scene = table.get("scene").and_then(|v| v.as_str()).map(path_of);
        let messages_file = table.get("messages_file").and_then(|v| v.as_str()).map(path_of);
        Config {
            name: table.get("name").and_then(|v| v.as_str()).unwrap_or("there").to_string(),
            latitude: num("latitude"),
            longitude: num("longitude"),
            messages_file,
            scene,
        }
    }

    #[cfg(test)]
    fn default_board_named(name: &str) -> Config {
        Config { name: name.into(), ..Config::default_board() }
    }

    fn default_board() -> Config {
        Config {
            name: "there".into(),
            latitude: None,
            longitude: None,
            messages_file: None,
            scene: None,
        }
    }
}

pub struct Morning {
    config: Config,
    planner: std::sync::Mutex<Planner>,
    weather_path: std::path::PathBuf,
    /// The table the glass paints from (`<data>/sky.toml`, kept by main's
    /// sky keeper); the page measures its type against the same one.
    sky_path: std::path::PathBuf,
}

/// The fills of a scene's land layers, far to near: the first `fill="#…"`
/// in each SVG named by a `[[layer]]` that is not a night layer. Read on
/// each page build so a re-run of the key splitter recolours the type
/// with the wallpaper.
pub fn scene_palette(scene: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(scene) else { return Vec::new() };
    let Ok(root) = text.parse::<toml::Value>() else { return Vec::new() };
    let dir = scene.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let mut out = Vec::new();
    for layer in root.get("layer").and_then(|v| v.as_array()).into_iter().flatten() {
        let night = layer.get("night").and_then(|v| v.as_bool()).unwrap_or(false) || layer.get("on_below").is_some();
        if night {
            continue;
        }
        let Some(svg) = layer.get("svg").and_then(|v| v.as_str()) else { continue };
        let Ok(body) = std::fs::read_to_string(dir.join(svg)) else { continue };
        if let Some(i) = body.find("fill=\"#")
            && let Some(hex) = body.get(i + 6..i + 13)
            && hex.len() == 7
            && hex[1..].chars().all(|c| c.is_ascii_hexdigit())
        {
            out.push(hex.to_string());
        }
    }
    out
}

impl Morning {
    pub fn new(config: Config, data: &Path) -> Morning {
        let site = match (config.latitude, config.longitude) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };
        let planner = Planner::new(Some(data), config.messages_file.clone(), site);
        eprintln!("morning: {} lines in the message corpus", planner.count());
        Morning {
            config,
            planner: std::sync::Mutex::new(planner),
            weather_path: data.join("weather.json"),
            sky_path: data.join("sky.toml"),
        }
    }

    fn page(&self, now: i64) -> Result<Vec<u8>, Status> {
        let q = rill_doc::kdl_escape;
        let (now, live_ms) = demo_clock(now);
        let t = Local::at(now);
        // The line for this hour, from the planner (messages.rs).
        let message = self.planner.lock().map(|mut p| p.message_for(now)).unwrap_or_default();
        // Type in the wallpaper's own colours: the front (dark) layers on a
        // bright sky, the far (light) layers otherwise, all three slots
        // swapping in the same minute (type_colours). The sky is the one
        // the glass paints from, so the swap lands at dusk and dawn
        // wherever the season puts them. White when there is no scene.
        let palette = self.config.scene.as_deref().map(scene_palette).unwrap_or_default();
        let sky_rgb = self.sky_rgb(&t);
        let bright = luminance(sky_rgb) > DAY_LUMINANCE;
        let (c_clock, c_date, c_fun) = type_colours(&palette, sky_rgb);
        // The weather column: icon, the temperature both ways, a word, the
        // day's high and low. Day or night for the icon follows the sky the
        // glass is showing rather than the file, so the demo clock agrees.
        let weather_kdl = match Forecast::load(&self.weather_path) {
            Some(f) => {
                let icon = weather::icon(f.code, bright);
                let desc = weather::description(f.code, bright);
                let temp = format!("{}°F / {}°C", weather::c_to_f(f.temp_c).round(), f.temp_c.round());
                let hi_lo = match (f.high_c, f.low_c) {
                    (Some(h), Some(l)) => {
                        format!("H {}° L {}°", weather::c_to_f(h).round(), weather::c_to_f(l).round())
                    }
                    _ => String::new(),
                };
                let mut extra = vec![format!("Feels like {}°", weather::c_to_f(f.feels_c).round())];
                if let Some(h) = f.humidity {
                    extra.push(format!("{}% humidity", h.round()));
                }
                if let Some(w) = f.wind_kmh {
                    extra.push(format!("{} mph wind", (w * 0.621_371).round()));
                }
                let extra = extra.join("  ·  ");
                let stale = if f.age_s > 3 * 3600 { format!("{} h old", f.age_s / 3600) } else { String::new() };
                format!(
                    "\t\tcolumn style=\"wx\" {{\n\
                     \t\t\ticon \"{icon}\" style=\"wx-icon\" size=96\n\
                     \t\t\ttext {temp} style=\"wx-temp\"\n\
                     \t\t\ttext {desc} style=\"wx-desc\"\n\
                     \t\t\ttext {hilo} style=\"wx-hilo\"\n\
                     \t\t\ttext {extra} style=\"wx-stale\"\n\
                     \t\t\ttext {stale} style=\"wx-stale\"\n\
                     \t\t}}\n",
                    temp = q(&temp),
                    desc = q(desc),
                    hilo = q(&hi_lo),
                    extra = q(&extra),
                    stale = q(&stale),
                )
            }
            None => "\t\tcolumn style=\"wx\" { text \"weather: no data yet\" style=\"wx-stale\" }\n".to_string(),
        };
        let kdl = format!(
            "style \"root\" padding=0 gap=0 width=\"fill\" height=\"fill\"\n\
             style \"top\" padding-x=64 padding-y=48 gap=32 width=\"fill\"\n\
             style \"left\" gap=4 width=\"fill\"\n\
             style \"wx\" gap=2 width=620 align=\"right\"\n\
             style \"clock\" size=120 weight=500 color=\"{c_clock}\" font=\"{FACE}\" align=\"left\"\n\
             style \"date\" size=36 weight=400 color=\"{c_date}\" font=\"{FACE}\" align=\"left\"\n\
             style \"fun\" size=24 weight=400 color=\"{c_fun}\" font=\"{FACE}\" align=\"left\"\n\
             style \"wx-icon\" color=\"{c_clock}\" align=\"right\"\n\
             style \"wx-temp\" size=44 weight=500 color=\"{c_clock}\" font=\"{FACE}\" align=\"right\" wrap=#false\n\
             style \"wx-desc\" size=28 weight=400 color=\"{c_date}\" font=\"{FACE}\" align=\"right\" wrap=#false\n\
             style \"wx-hilo\" size=24 weight=400 color=\"{c_fun}\" font=\"{FACE}\" align=\"right\" wrap=#false\n\
             style \"wx-stale\" size=20 weight=400 color=\"{c_fun}\" font=\"{FACE}\" align=\"right\" wrap=#false\n\n\
             column style=\"root\" {{\n\
             \tpage background=\"#00000000\"\n\
             \trow style=\"top\" {{\n\
             \t\tcolumn style=\"left\" {{\n\
             \t\t\ttext {clock} style=\"clock\"\n\
             \t\t\ttext {date} style=\"date\"\n\
             \t\t\tspacer size=14\n\
             \t\t\ttext {fun} style=\"fun\"\n\
             \t\t}}\n\
             {weather_kdl}\
             \t}}\n\
             \tlive target=\"/morning\" every={live_ms}\n\
             }}\n",
            clock = q(&t.clock12()),
            date = q(&t.long_date()),
            fun = q(&message),
        );
        rill_doc::compile(&kdl).map(|c| c.bytes).map_err(|e| {
            eprintln!("morning: compile: {e}");
            Status::Internal
        })
    }
}

impl Morning {
    /// The sky behind the page at `now`: the table on disk — the one the
    /// glass is painting, today's, with the weather in it — sampled at the
    /// shown hour and minute; the day's computed table when the file is
    /// not there yet. Never a table for the shown date: under the demo's
    /// date cycle the glass keeps today's sky, and the type must follow
    /// what is painted, not what the calendar says. With no site, a plain
    /// day or night blue by the hour.
    fn sky_rgb(&self, t: &Local) -> (f32, f32, f32) {
        let (Some(lat), Some(lon)) = (self.config.latitude, self.config.longitude) else {
            return if (7..19).contains(&t.hour) { (0x4a as f32, 0x8f as f32, 0xe0 as f32) } else { (15.0, 23.0, 51.0) };
        };
        let map = sky::SkyMap::read(&self.sky_path).unwrap_or_else(|| sky::build(&sky::site_now(lat, lon, unix_now()), None));
        let h = t.hour as usize % 24;
        let frac = t.minute as f32 / 60.0;
        let (a, b) = (map.hours[h].sky, map.hours[(h + 1) % 24].sky);
        // The glass shades the zenith towards black (scene_layers paints
        // the band from a 14 % darker top); the type sits up there.
        let mixc = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * frac) * (1.0 - ZENITH_SHADE);
        (mixc(a.0, b.0), mixc(a.1, b.1), mixc(a.2, b.2))
    }

    #[cfg(test)]
    fn sky_is_bright(&self, t: &Local) -> bool {
        luminance(self.sky_rgb(t)) > DAY_LUMINANCE
    }
}

/// Linear luminance above which the sky counts as day (for the weather
/// icon's sun or moon). The noon blue sits near 0.26, dusk near 0.1.
const DAY_LUMINANCE: f32 = 0.18;

/// Relative luminance of an sRGB colour, 0..1 (WCAG's linearised form).
fn luminance((r, g, b): (f32, f32, f32)) -> f32 {
    let lin = |c: f32| {
        let c = c / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG contrast ratio between two colours, 1..21.
fn contrast(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (la, lb) = (luminance(a) + 0.05, luminance(b) + 0.05);
    if la > lb { la / lb } else { lb / la }
}

fn hex_rgb(h: &str) -> Option<(f32, f32, f32)> {
    let h = h.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|n| n as f32);
    Some((v(0)?, v(2)?, v(4)?))
}

fn rgb_hex((r, g, b): (f32, f32, f32)) -> String {
    format!("#{:02x}{:02x}{:02x}", r.round() as u8, g.round() as u8, b.round() as u8)
}

/// The floor for the big type, and the higher one for the smaller lines.
/// Neither is met by any layer colour in the ten minutes either side of
/// civil dusk, so there the nudge runs the type to near white or black;
/// the price of a board that reads at a glance across the room.
const CONTRAST_FLOOR: f32 = 4.5;
const CONTRAST_FLOOR_LINES: f32 = 5.5;

/// How far the compositor darkens the sky table's colour at the top of the
/// screen, where the type sits (scene_layers.rs, the sky band's zenith).
const ZENITH_SHADE: f32 = 0.14;

/// `c` if it clears `floor` against `sky`, else `c` mixed towards white
/// (`light`) or black by the smallest step that clears it. The direction
/// is the page's mode, not the colour's own: a mid-tone in light mode
/// nudged towards black would put one slot on the other side of the swap.
/// The hue survives as far as legibility allows.
fn legible(c: (f32, f32, f32), sky: (f32, f32, f32), floor: f32, light: bool) -> (f32, f32, f32) {
    if contrast(c, sky) >= floor {
        return c;
    }
    let target = if light { (255.0, 255.0, 255.0) } else { (0.0, 0.0, 0.0) };
    let mut best = target;
    for step in 1..=20 {
        let t = step as f32 / 20.0;
        let m = (c.0 + (target.0 - c.0) * t, c.1 + (target.1 - c.1) * t, c.2 + (target.2 - c.2) * t);
        if contrast(m, sky) >= floor {
            best = m;
            break;
        }
    }
    best
}

/// Whether the type reads dark (front layers) against `sky`: through
/// sunrise, the day and the sunset glow, until the sky is night by the same
/// luminance line that turns the weather icon to a moon. One answer for
/// every slot, so the page changes in a single minute. Below that line
/// white beats black as a nudge target, so the light set is also the one
/// the safety net can rescue.
fn dark_type(sky: (f32, f32, f32)) -> bool {
    luminance(sky) > DAY_LUMINANCE
}

/// Colours for the clock, the date and the line: the palette sorted dark to
/// light; the three darkest by day, the three lightest by night, the clock
/// taking the extreme. Each is then made
/// legible as a safety net for the minutes around the swap. Without a
/// palette, white made legible.
fn type_colours(palette: &[String], sky: (f32, f32, f32)) -> (String, String, String) {
    let mut cols: Vec<(f32, f32, f32)> = palette.iter().filter_map(|h| hex_rgb(h)).collect();
    if cols.is_empty() {
        cols.push((255.0, 255.0, 255.0));
    }
    cols.sort_by(|a, b| luminance(*a).partial_cmp(&luminance(*b)).unwrap_or(std::cmp::Ordering::Equal));
    let n = cols.len();
    let dark = dark_type(sky);
    let pick: [(f32, f32, f32); 3] = if dark {
        [cols[0], cols[1.min(n - 1)], cols[2.min(n - 1)]]
    } else {
        [cols[n - 1], cols[n.saturating_sub(2)], cols[n.saturating_sub(3)]]
    };
    let fix = |i: usize, floor: f32| rgb_hex(legible(pick[i], sky, floor, !dark));
    (fix(0, CONTRAST_FLOOR), fix(1, CONTRAST_FLOOR_LINES), fix(2, CONTRAST_FLOOR_LINES))
}

impl AppHandler for Morning {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        match path {
            "/morning" | "/morning/" => self.page(unix_now()).ok(),
            _ => None,
        }
    }

    fn revision(&self, path: &str, _identity: &Identity) -> Option<u64> {
        match path {
            // The shown minute, offset so it never reads as "zero, nothing".
            "/morning" | "/morning/" => Some(1 + demo_clock(unix_now()).0.div_euclid(60) as u64),
            _ => None,
        }
    }

    fn action(
        &self,
        _path: &str,
        _fields: &[(String, ActionValue)],
        _identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        Err(Status::NotFound)
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The building's clock, broken out: the page shows the day as the person
/// in front of it lives it, so this is `localtime_r`, not UTC arithmetic.
struct Local {
    hour: i64,
    minute: i64,
    second: i64,
    weekday: usize,
    month: usize,
    day: i64,
    year: i64,
    /// Seconds east of UTC for this instant's zone.
    gmtoff: i64,
}

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
    "November", "December",
];

impl Local {
    fn at(t: i64) -> Local {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let secs: libc::time_t = t as libc::time_t;
        if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
            let day = t.rem_euclid(86_400);
            return Local { hour: day / 3600, minute: (day % 3600) / 60, second: day % 60, weekday: 4, month: 0, day: 1, year: 1970, gmtoff: 0 };
        }
        Local {
            hour: tm.tm_hour as i64,
            minute: tm.tm_min as i64,
            second: tm.tm_sec as i64,
            weekday: (tm.tm_wday.rem_euclid(7)) as usize,
            month: (tm.tm_mon.rem_euclid(12)) as usize,
            day: tm.tm_mday as i64,
            year: tm.tm_year as i64 + 1900,
            gmtoff: tm.tm_gmtoff,
        }
    }

    /// "Thursday, September 25, 2026".
    fn long_date(&self) -> String {
        format!("{}, {} {}, {}", DAYS[self.weekday], MONTHS[self.month], self.day, self.year)
    }

    /// "7:42 AM".
    fn clock12(&self) -> String {
        let (h12, ap) = match self.hour {
            0 => (12, "AM"),
            1..=11 => (self.hour, "AM"),
            12 => (12, "PM"),
            _ => (self.hour - 12, "PM"),
        };
        format!("{h12}:{:02} {ap}", self.minute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> Morning {
        let d = std::env::temp_dir().join(format!("morning-board-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        Morning::new(Config::default_board_named("Evan"), &d)
    }

    #[test]
    fn the_page_compiles_transparent_and_live_at_any_hour() {
        let b = board();
        for hour in [3, 9, 14, 21] {
            let now = 1_790_000_000 + hour * 3600;
            let bytes = b.page(now).expect("compiles");
            let doc = rill_doc::decode(&bytes).expect("decodes");
            assert!(doc.strings.iter().any(|s| s.len() > 12 && !s.contains('/')), "a fun line is on the page");
            assert!(!doc.strings.iter().any(|s| s.contains("Evan")), "no greeting line");
            assert!(doc.strings.iter().any(|s| s == "/morning"), "live target present");
        }
    }

    #[test]
    fn the_greeting_follows_the_hour() {
        let kdl_at = |hour: i64| {
            // Build the same page text the handler compiles, by a local
            // instant whose hour is known: noon UTC on a day with a known
            // local offset is not portable, so drive `Local` directly.
            let t = Local { hour, minute: 5, second: 0, weekday: 4, month: 8, day: 25, year: 2026, gmtoff: -25_200 };
            (t.clock12(), t.long_date())
        };
        assert_eq!(kdl_at(7).0, "7:05 AM");
        assert_eq!(kdl_at(0).0, "12:05 AM");
        assert_eq!(kdl_at(12).0, "12:05 PM");
        assert_eq!(kdl_at(19).0, "7:05 PM");
        assert_eq!(kdl_at(7).1, "Thursday, September 25, 2026");
    }

    #[test]
    fn the_revision_is_the_minute() {
        let b = board();
        let id = Identity::Anonymous;
        let r1 = b.revision("/morning", &id).unwrap();
        assert!(r1 > 0);
        assert_eq!(b.revision("/elsewhere", &id), None);
    }

    #[test]
    fn the_palette_is_the_land_layers_fills_far_to_near() {
        let dir = std::env::temp_dir().join(format!("morning-palette-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.svg"), r##"<svg viewBox="0 0 1 1"><path d="M0 0" fill="#9a93b6"/></svg>"##).unwrap();
        std::fs::write(dir.join("b.svg"), r##"<svg viewBox="0 0 1 1"><path d="M0 0" fill="#5f5878"/></svg>"##).unwrap();
        std::fs::write(dir.join("l.svg"), r##"<svg viewBox="0 0 1 1"><path d="M0 0" fill="#ffd98a"/></svg>"##).unwrap();
        std::fs::write(
            dir.join("scene.toml"),
            "[[layer]]\nsvg = \"a.svg\"\n[[layer]]\nsvg = \"l.svg\"\non_below = 0\n[[layer]]\nsvg = \"b.svg\"\n",
        )
        .unwrap();
        assert_eq!(scene_palette(&dir.join("scene.toml")), vec!["#9a93b6", "#5f5878"]);
        assert!(scene_palette(std::path::Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn the_type_swaps_with_the_sky() {
        let no_site = board();
        let at = |hour| Local { hour, minute: 0, second: 0, weekday: 4, month: 8, day: 25, year: 2026, gmtoff: -25_200 };
        assert!(no_site.sky_is_bright(&at(12)));
        assert!(!no_site.sky_is_bright(&at(23)));
        let d = std::env::temp_dir().join(format!("morning-board-la-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let la = Morning::new(Config { latitude: Some(34.05), longitude: Some(-118.24), ..Config::default_board() }, &d);
        // No table on disk in this fresh dir, so today's computed one:
        // noon is bright and one in the morning is not, any day of the
        // year, at any offset the box's zone puts on it.
        assert!(la.sky_is_bright(&at(13)));
        assert!(!la.sky_is_bright(&at(1)));
    }

    #[test]
    fn type_stays_in_the_palette_when_it_can_and_clears_the_floor_when_it_cannot() {
        let palette: Vec<String> = ["#9a93b6", "#8982a4", "#787193", "#676081", "#564f6d", "#433d57", "#312c41", "#1e1a2b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let noon = (0x49 as f32, 0x8e as f32, 0xdf as f32);
        let (clock, ..) = type_colours(&palette, noon);
        assert_eq!(clock, "#1e1a2b", "noon: the front layer reads as-is");
        let night = (15.0, 23.0, 51.0);
        let (clock, ..) = type_colours(&palette, night);
        assert_eq!(clock, "#9a93b6", "night: the far layer reads as-is");
        // Golden hour: a warm mid-tone where neither end clears the floor
        // untouched; whatever comes back must clear it.
        let golden = (0xf0 as f32, 0xa8 as f32, 0x78 as f32);
        let (clock, date, fun) = type_colours(&palette, golden);
        for (c, floor) in [(clock, CONTRAST_FLOOR), (date, CONTRAST_FLOOR_LINES), (fun, CONTRAST_FLOOR_LINES)] {
            let rgb = hex_rgb(&c).unwrap();
            assert!(contrast(rgb, golden) >= floor, "{c} vs golden: {}", contrast(rgb, golden));
        }
        // No palette: white, made legible — white already clears the floor
        // against the noon blue (3.3:1, fine for a 168 px clock) and stays.
        assert_eq!(type_colours(&[], night).0, "#ffffff");
        let (c, ..) = type_colours(&[], noon);
        assert!(contrast(hex_rgb(&c).unwrap(), noon) >= CONTRAST_FLOOR);
    }

    #[test]
    fn the_three_slots_swap_together_when_the_sky_turns_night() {
        let palette: Vec<String> = ["#9a93b6", "#8982a4", "#787193", "#676081", "#564f6d", "#433d57", "#312c41", "#1e1a2b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mid = luminance(hex_rgb("#676081").unwrap());
        // A dusk, minute by minute: the sky curve's keys from golden hour to
        // night, each step mixed towards the next in tenths.
        let curve = [[0xf0, 0xa8, 0x78], [0xe8, 0x8c, 0x5a], [0x8a, 0x4e, 0x6e], [0x2b, 0x2f, 0x62], [0x16, 0x20, 0x44], [0x0f, 0x17, 0x33]];
        let mut modes = Vec::new();
        for w in curve.windows(2) {
            for k in 0..10 {
                let t = k as f32 / 10.0;
                let m = |i: usize| w[0][i] as f32 + (w[1][i] as f32 - w[0][i] as f32) * t;
                let sky = (m(0), m(1), m(2));
                let (a, b, c) = type_colours(&palette, sky);
                let side = |h: &str| luminance(hex_rgb(h).unwrap()) > mid;
                assert_eq!(side(&a), side(&b), "{sky:?}: clock and date on different sides");
                assert_eq!(side(&b), side(&c), "{sky:?}: date and line on different sides");
                modes.push(side(&a));
            }
        }
        // One swap, dark to light: dark through the sunset orange (0°),
        // light by the mauve (-3°) key.
        let swaps = modes.windows(2).filter(|p| p[0] != p[1]).count();
        assert_eq!(swaps, 1, "modes: {modes:?}");
        assert!(!modes[0] && modes[modes.len() - 1]);
        assert!(!modes[10], "dark type on the sunset orange");
        assert!(modes[20], "light type by the -3° mauve key");
        // The weather icon turns to a moon on the same line.
        assert_eq!(dark_type((0xe8 as f32, 0x8c as f32, 0x5a as f32)), luminance((0xe8 as f32, 0x8c as f32, 0x5a as f32)) > DAY_LUMINANCE);
    }

    #[test]
    fn a_missing_config_greets_somebody() {
        let c = Config::load(Path::new("/nonexistent/morning.toml"));
        assert_eq!(c.name, "there");
        assert_eq!(c.latitude, None);
    }
}
