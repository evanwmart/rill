//! The day's sky, as a table the glass can read without knowing why.
//!
//! The morning board's wallpaper is a layered vector scene whose sky
//! colour and atmospheric haze track the real day. All the knowledge that
//! takes — where the sun is for this place and date, what the weather is
//! doing — lives here, on the server that already has the location and the
//! forecast. What leaves is deliberately dumb:
//!
//! ```text
//! hour -> { sky_color_hex, desaturation_strength }
//! ```
//!
//! twenty-four rows in `sky.toml`, regenerated when the forecast changes.
//! The compositor interpolates between rows by the wall clock and applies
//! the two numbers to its scene layers; it never sees a sun, a cloud or a
//! coordinate. Hub decides, glass shows.
//!
//! Two sources feed the table. **Solar position** (a compact NOAA-style
//! approximation: declination and the equation of time from the fractional
//! year, then elevation from the hour angle) drives a base curve keyed on
//! elevation, so dawn and dusk are the same geometry the sky has and a
//! winter noon is lower and cooler than a summer one. **Weather** — cloud
//! cover, rain, visibility — sets a per-hour desaturation and nudges the
//! colour a little towards grey and dark; a small effect on purpose, so a
//! grey day reads as grey without pretending to be a lighting model.

use std::f64::consts::PI;
use std::fmt::Write as _;
use std::path::Path;

/// One hour's weather, as much of it as the sky cares about.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WeatherHour {
    /// Cloud cover, 0 (clear) to 1 (overcast).
    pub cloud: f32,
    /// Precipitation in the hour, mm.
    pub rain_mm: f32,
    /// Visibility in metres; `None` when the forecast has no figure.
    pub visibility_m: Option<f32>,
}

/// The place and day the table is for.
#[derive(Clone, Copy, Debug)]
pub struct Site {
    pub latitude: f64,
    pub longitude: f64,
    /// Local offset from UTC in seconds (east positive), for this date.
    pub utc_offset_s: i64,
    /// Day of the year, 1..=366.
    pub day_of_year: u32,
}

/// One row of the table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyHour {
    pub hour: u32,
    pub sky: Rgb,
    pub desat: f32,
    /// Solar elevation at the top of the hour, degrees; kept so a reader
    /// can tell twilight from night without recomputing anything.
    pub elevation: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// `#rrggbb` back to a colour; `None` for anything else.
    pub fn parse(hex: &str) -> Option<Rgb> {
        let h = hex.strip_prefix('#')?;
        if h.len() != 6 {
            return None;
        }
        let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some(Rgb(c(0)?, c(2)?, c(4)?))
    }

    fn mix(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    fn scale(self, k: f32) -> Rgb {
        let s = |a: u8| (a as f32 * k).round().clamp(0.0, 255.0) as u8;
        Rgb(s(self.0), s(self.1), s(self.2))
    }
}

/// Sunrise, sunset and the twenty-four rows.
#[derive(Clone, Debug)]
pub struct SkyMap {
    /// Minutes after local midnight; `None` when the sun does not rise or
    /// set that day (polar summer or winter).
    pub sunrise_min: Option<i32>,
    pub sunset_min: Option<i32>,
    pub hours: Vec<SkyHour>,
}

// ---------------------------------------------------------------- solar

/// Equation of time (minutes) and solar declination (radians) for the
/// fractional year `gamma` (radians). NOAA's low-order series.
fn eqtime_decl(gamma: f64) -> (f64, f64) {
    let eqtime = 229.18
        * (0.000075 + 0.001868 * gamma.cos() - 0.032077 * gamma.sin()
            - 0.014615 * (2.0 * gamma).cos()
            - 0.040849 * (2.0 * gamma).sin());
    let decl = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
        - 0.006758 * (2.0 * gamma).cos()
        + 0.000907 * (2.0 * gamma).sin()
        - 0.002697 * (3.0 * gamma).cos()
        + 0.00148 * (3.0 * gamma).sin();
    (eqtime, decl)
}

/// Solar elevation in degrees at `local_min` minutes after local midnight.
pub fn elevation_deg(site: &Site, local_min: f64) -> f64 {
    let hour = local_min / 60.0;
    let gamma = 2.0 * PI / 365.0 * (site.day_of_year as f64 - 1.0 + (hour - 12.0) / 24.0);
    let (eqtime, decl) = eqtime_decl(gamma);
    let tz_hours = site.utc_offset_s as f64 / 3600.0;
    let offset = eqtime + 4.0 * site.longitude - 60.0 * tz_hours;
    let tst = local_min + offset;
    let ha = (tst / 4.0 - 180.0).to_radians();
    let lat = site.latitude.to_radians();
    let sin_el = lat.sin() * decl.sin() + lat.cos() * decl.cos() * ha.cos();
    sin_el.clamp(-1.0, 1.0).asin().to_degrees()
}

/// Sunrise and sunset as minutes after local midnight, by the standard
/// −0.833° horizon (refraction plus the disc's radius).
pub fn sunrise_sunset(site: &Site) -> (Option<i32>, Option<i32>) {
    let gamma = 2.0 * PI / 365.0 * (site.day_of_year as f64 - 1.0);
    let (eqtime, decl) = eqtime_decl(gamma);
    let lat = site.latitude.to_radians();
    let zenith = 90.833f64.to_radians();
    let cos_ha = zenith.cos() / (lat.cos() * decl.cos()) - lat.tan() * decl.tan();
    if !(-1.0..=1.0).contains(&cos_ha) {
        return (None, None);
    }
    let ha = cos_ha.acos().to_degrees();
    let tz_min = site.utc_offset_s as f64 / 60.0;
    let rise = 720.0 - 4.0 * (site.longitude + ha) - eqtime + tz_min;
    let set = 720.0 - 4.0 * (site.longitude - ha) - eqtime + tz_min;
    (Some(rise.round() as i32), Some(set.round() as i32))
}

// ---------------------------------------------------------------- colour

/// The base sky by solar elevation, in degrees. Night is a deep navy, the
/// twilights climb through violet to the warm band at the horizon, and the
/// day settles into a blue that deepens as the sun climbs. Between keys the
/// colour is a straight mix.
const CURVE: &[(f32, Rgb)] = &[
    (-90.0, Rgb(0x0f, 0x17, 0x33)),
    (-18.0, Rgb(0x0f, 0x17, 0x33)),
    (-12.0, Rgb(0x16, 0x20, 0x44)),
    (-6.0, Rgb(0x2b, 0x2f, 0x62)),
    (-3.0, Rgb(0x8a, 0x4e, 0x6e)),
    (0.0, Rgb(0xe8, 0x8c, 0x5a)),
    (4.0, Rgb(0xf0, 0xa8, 0x78)),
    (10.0, Rgb(0x8c, 0xb4, 0xe6)),
    (25.0, Rgb(0x6c, 0xa8, 0xe6)),
    (50.0, Rgb(0x4a, 0x8f, 0xe0)),
    (90.0, Rgb(0x3f, 0x86, 0xdc)),
];

/// A dusk is warmer than a dawn: the same elevation after solar noon leans
/// this far towards ember.
const DUSK_EMBER: Rgb = Rgb(0xff, 0x6e, 0x3c);

/// Where clouds pull the colour: a flat grey-blue.
const OVERCAST: Rgb = Rgb(0x8c, 0x96, 0xa8);

fn base_sky(elevation: f32, after_noon: bool) -> Rgb {
    let mut i = 0;
    while i + 1 < CURVE.len() && CURVE[i + 1].0 < elevation {
        i += 1;
    }
    let (e0, c0) = CURVE[i];
    let (e1, c1) = CURVE[(i + 1).min(CURVE.len() - 1)];
    let t = if e1 > e0 { (elevation - e0) / (e1 - e0) } else { 0.0 };
    let base = c0.mix(c1, t);
    // The ember only shows near the horizon: strongest at 0°, gone by ±8°.
    if after_noon {
        let near = (1.0 - (elevation.abs() / 8.0)).clamp(0.0, 1.0);
        base.mix(DUSK_EMBER, 0.18 * near)
    } else {
        base
    }
}

/// Weather to atmosphere. Cloud cover is most of it; rain and poor
/// visibility add a little. The colour shift is kept small: a tenth of the
/// way to grey per full overcast, and a slight darkening under rain.
fn weather_effect(w: Option<&WeatherHour>) -> (f32, f32, f32) {
    let Some(w) = w else { return (0.0, 0.0, 1.0) };
    let cloud = w.cloud.clamp(0.0, 1.0);
    let rain = (w.rain_mm / 2.0).clamp(0.0, 1.0);
    let haze = w.visibility_m.map(|v| (1.0 - (v / 10_000.0)).clamp(0.0, 1.0)).unwrap_or(0.0);
    let desat = (0.6 * cloud + 0.25 * rain + 0.25 * haze).clamp(0.0, 0.9);
    let grey_shift = 0.10 * cloud + 0.08 * haze;
    let darken = 1.0 - 0.18 * rain - 0.05 * cloud;
    (desat, grey_shift, darken)
}

/// The day's table. `weather` is indexed by hour, 0..24, and may be absent.
pub fn build(site: &Site, weather: Option<&[WeatherHour]>) -> SkyMap {
    let (sunrise_min, sunset_min) = sunrise_sunset(site);
    // Solar noon splits dawn from dusk; without a sunrise/sunset pair
    // (polar day or night) fall back to clock noon.
    let noon_min = match (sunrise_min, sunset_min) {
        (Some(r), Some(s)) => (r + s) as f64 / 2.0,
        _ => 720.0,
    };
    let hours = (0..24)
        .map(|hour| {
            let local_min = hour as f64 * 60.0;
            let elevation = elevation_deg(site, local_min) as f32;
            let base = base_sky(elevation, local_min > noon_min);
            let (desat, grey_shift, darken) = weather_effect(weather.and_then(|w| w.get(hour as usize)));
            let sky = base.mix(OVERCAST, grey_shift).scale(darken);
            SkyHour { hour, sky, desat, elevation }
        })
        .collect();
    SkyMap { sunrise_min, sunset_min, hours }
}

fn hhmm(min: i32) -> String {
    let m = min.rem_euclid(24 * 60);
    format!("{:02}:{:02}", m / 60, m % 60)
}

impl SkyMap {
    /// The file the compositor reads. Hand-formatted: twenty-four rows of
    /// three numbers do not need a serializer, and the shape is part of the
    /// contract (specs/theming.md, `[desktop] sky`).
    pub fn to_toml(&self, label: &str) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# sky map: {label}");
        let _ = writeln!(out, "# hour -> sky colour, desaturation 0..1; the glass interpolates by the minute");
        match (self.sunrise_min, self.sunset_min) {
            (Some(r), Some(s)) => {
                let _ = writeln!(out, "sunrise = \"{}\"", hhmm(r));
                let _ = writeln!(out, "sunset = \"{}\"", hhmm(s));
            }
            _ => {
                let _ = writeln!(out, "# no sunrise or sunset today");
            }
        }
        for h in &self.hours {
            let _ = writeln!(out);
            let _ = writeln!(out, "[[hour]]");
            let _ = writeln!(out, "h = {}", h.hour);
            let _ = writeln!(out, "sky = \"{}\"", h.sky.hex());
            let _ = writeln!(out, "desat = {:.2}", h.desat);
            let _ = writeln!(out, "elevation = {:.1}", h.elevation);
        }
        out
    }

    /// Write atomically: the compositor hot-reloads on mtime and must never
    /// read a half-written table.
    pub fn write(&self, path: &Path, label: &str) -> std::io::Result<()> {
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_toml(label))?;
        std::fs::rename(&tmp, path)
    }

    /// The table back from the file, so a page can measure against the sky
    /// the glass is actually painting — weather and all — rather than a
    /// second computation of its own. `None` when the file is missing,
    /// malformed, or short of its twenty-four rows.
    pub fn read(path: &Path) -> Option<SkyMap> {
        let text = std::fs::read_to_string(path).ok()?;
        let doc: toml::Value = text.parse().ok()?;
        let hhmm_min = |key: &str| -> Option<i32> {
            let v = doc.get(key)?.as_str()?;
            let (h, m) = v.split_once(':')?;
            Some(h.parse::<i32>().ok()? * 60 + m.parse::<i32>().ok()?)
        };
        let rows = doc.get("hour")?.as_array()?;
        let mut hours = Vec::with_capacity(24);
        for row in rows {
            let hex = row.get("sky")?.as_str()?;
            let sky = Rgb::parse(hex)?;
            hours.push(SkyHour {
                hour: row.get("h")?.as_integer()? as u32,
                sky,
                desat: row.get("desat")?.as_float()? as f32,
                elevation: row.get("elevation")?.as_float()? as f32,
            });
        }
        if hours.len() != 24 || hours.iter().enumerate().any(|(i, h)| h.hour as usize != i) {
            return None;
        }
        Some(SkyMap { sunrise_min: hhmm_min("sunrise"), sunset_min: hhmm_min("sunset"), hours })
    }
}

/// The site for `now` at a location: the local offset and the day of the
/// year come from the C library's view of the local zone, the same clock
/// every other page on this server keeps.
pub fn site_now(latitude: f64, longitude: f64, now: i64) -> Site {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let secs: libc::time_t = now as libc::time_t;
    let (utc_offset_s, day_of_year) = if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        (0, ((now.rem_euclid(365 * 86_400)) / 86_400 + 1) as u32)
    } else {
        (tm.tm_gmtoff, tm.tm_yday as u32 + 1)
    };
    Site { latitude, longitude, utc_offset_s, day_of_year }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Los Angeles, Pacific Daylight Time, the September equinox (day 266).
    fn la_equinox() -> Site {
        Site { latitude: 34.05, longitude: -118.24, utc_offset_s: -7 * 3600, day_of_year: 266 }
    }

    #[test]
    fn the_table_reads_back_as_written() {
        let site = Site { latitude: 34.15, longitude: -118.45, utc_offset_s: -7 * 3600, day_of_year: 270 };
        let map = build(&site, None);
        let dir = std::env::temp_dir().join(format!("rill-sky-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sky.toml");
        map.write(&path, "test").unwrap();
        let back = SkyMap::read(&path).expect("reads back");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(back.sunrise_min, map.sunrise_min);
        assert_eq!(back.sunset_min, map.sunset_min);
        assert_eq!(back.hours.len(), 24);
        for (a, b) in back.hours.iter().zip(&map.hours) {
            assert_eq!(a.hour, b.hour);
            assert_eq!(a.sky, b.sky);
            assert!((a.desat - b.desat).abs() < 0.006 && (a.elevation - b.elevation).abs() < 0.06);
        }
        assert!(SkyMap::read(&dir.join("missing.toml")).is_none());
    }

    #[test]
    fn sunrise_and_sunset_land_where_the_almanac_puts_them() {
        // Published for Los Angeles, 23 Sep: sunrise 06:41, sunset 18:50 PDT.
        let (r, s) = sunrise_sunset(&la_equinox());
        let (r, s) = (r.unwrap(), s.unwrap());
        assert!((r - (6 * 60 + 41)).abs() <= 6, "sunrise {} min", r);
        assert!((s - (18 * 60 + 50)).abs() <= 6, "sunset {} min", s);
    }

    #[test]
    fn elevation_is_below_the_horizon_at_midnight_and_high_at_noon() {
        let site = la_equinox();
        assert!(elevation_deg(&site, 0.0) < -40.0);
        let noon = elevation_deg(&site, 12.0 * 60.0 + 47.0);
        // At the equinox the noon sun stands at 90° minus the latitude.
        assert!((noon - (90.0 - 34.05)).abs() < 2.0, "noon elevation {noon}");
    }

    #[test]
    fn the_curve_is_dark_at_night_warm_at_the_horizon_and_blue_by_day() {
        let night = base_sky(-30.0, false);
        let dawn = base_sky(0.0, false);
        let day = base_sky(40.0, false);
        assert!(night.0 < 0x18 && night.2 < 0x40 && night.2 > night.0, "night {night:?}");
        assert!(dawn.0 > dawn.2, "dawn should be warm: {dawn:?}");
        assert!(day.2 > day.0, "day should be blue: {day:?}");
        // Dusk leans to ember at the same elevation.
        let dusk = base_sky(0.0, true);
        assert!(dusk.0 >= dawn.0 && dusk.2 <= dawn.2, "dusk {dusk:?} vs dawn {dawn:?}");
    }

    #[test]
    fn clouds_raise_desaturation_and_only_nudge_the_colour() {
        let site = la_equinox();
        let clear = build(&site, None);
        let overcast: Vec<WeatherHour> =
            (0..24).map(|_| WeatherHour { cloud: 1.0, rain_mm: 0.0, visibility_m: None }).collect();
        let grey = build(&site, Some(&overcast));
        let noon = 13;
        assert_eq!(clear.hours[noon].desat, 0.0);
        assert!((grey.hours[noon].desat - 0.6).abs() < 1e-5);
        let (a, b) = (clear.hours[noon].sky, grey.hours[noon].sky);
        let dist = (a.0 as i32 - b.0 as i32).abs() + (a.1 as i32 - b.1 as i32).abs() + (a.2 as i32 - b.2 as i32).abs();
        assert!(dist > 0 && dist < 90, "shift should be small but present: {a:?} -> {b:?} ({dist})");
        // Rain darkens.
        let wet = vec![WeatherHour { cloud: 1.0, rain_mm: 4.0, visibility_m: Some(2_000.0) }; 24];
        let rainy = build(&site, Some(&wet));
        assert!(rainy.hours[noon].desat > grey.hours[noon].desat);
        assert!(rainy.hours[noon].sky.1 < grey.hours[noon].sky.1);
    }

    #[test]
    fn the_table_has_twenty_four_rows_and_round_trips_as_toml() {
        let map = build(&la_equinox(), None);
        assert_eq!(map.hours.len(), 24);
        let text = map.to_toml("test");
        let parsed: toml::Value = text.parse().expect("valid toml");
        let rows = parsed.get("hour").and_then(|v| v.as_array()).expect("hour array");
        assert_eq!(rows.len(), 24);
        assert_eq!(rows[0].get("h").and_then(|v| v.as_integer()), Some(0));
        assert!(rows[13].get("sky").and_then(|v| v.as_str()).unwrap().starts_with('#'));
        assert!(parsed.get("sunrise").is_some());
    }

    #[test]
    fn polar_night_has_no_sunrise_and_stays_dark() {
        let svalbard = Site { latitude: 78.2, longitude: 15.6, utc_offset_s: 3600, day_of_year: 355 };
        let map = build(&svalbard, None);
        assert_eq!(map.sunrise_min, None);
        assert!(map.hours.iter().all(|h| h.elevation < 0.0));
    }
}
