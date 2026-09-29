//! The forecast on disk, read for the page and the sky.
//!
//! `deploy/pi/morning-fetch.sh` writes `<data>/weather.json` — Open-Meteo's
//! JSON, `current` + `hourly` + `daily`, `timezone=auto`. This module reads
//! it, nothing more: the server never fetches. A missing or unreadable file
//! is `None`, and the page says so; a stale one is served with its age.

use std::path::Path;

use crate::sky;

#[derive(Clone, Debug)]
pub struct Forecast {
    pub temp_c: f32,
    pub feels_c: f32,
    pub humidity: Option<f32>,
    pub wind_kmh: Option<f32>,
    pub code: u16,
    pub high_c: Option<f32>,
    pub low_c: Option<f32>,
    /// Hourly rows: unix seconds, temp °C, precipitation mm, WMO code,
    /// cloud cover 0..1, visibility m.
    pub hourly: Vec<(i64, f32, f32, u16, f32, Option<f32>)>,
    /// Seconds east of UTC the file's local times are in.
    pub utc_offset_s: i64,
    /// Age of the file when read.
    pub age_s: i64,
}

impl Forecast {
    pub fn load(path: &Path) -> Option<Forecast> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let age_s = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let c = &v["current"];
        let f = |x: &serde_json::Value| x.as_f64().map(|n| n as f32);
        let offset = v["utc_offset_seconds"].as_i64().unwrap_or(0);
        let h = &v["hourly"];
        let arr = |k: &str| h[k].as_array().cloned().unwrap_or_default();
        let (times, temp, precip, code, cloud, vis) = (
            arr("time"),
            arr("temperature_2m"),
            arr("precipitation"),
            arr("weather_code"),
            arr("cloud_cover"),
            arr("visibility"),
        );
        let mut hourly = Vec::new();
        for (i, t) in times.iter().enumerate() {
            let Some(ts) = t.as_str().and_then(|s| iso_local_to_unix(s, offset)) else { continue };
            hourly.push((
                ts,
                temp.get(i).and_then(f).unwrap_or(20.0),
                precip.get(i).and_then(f).unwrap_or(0.0),
                code.get(i).and_then(|x| x.as_f64()).unwrap_or(0.0) as u16,
                cloud.get(i).and_then(f).unwrap_or(0.0) / 100.0,
                vis.get(i).and_then(f),
            ));
        }
        Some(Forecast {
            temp_c: f(&c["temperature_2m"])?,
            feels_c: f(&c["apparent_temperature"]).unwrap_or_else(|| f(&c["temperature_2m"]).unwrap_or(0.0)),
            humidity: f(&c["relative_humidity_2m"]),
            wind_kmh: f(&c["wind_speed_10m"]),
            code: c["weather_code"].as_f64().unwrap_or(0.0) as u16,
            high_c: v["daily"]["temperature_2m_max"].get(0).and_then(f),
            low_c: v["daily"]["temperature_2m_min"].get(0).and_then(f),
            hourly,
            utc_offset_s: offset,
            age_s,
        })
    }

    /// The sky module's view of today's hours, indexed by local hour, for
    /// the day containing `now` in the file's own zone.
    pub fn sky_hours(&self, now: i64) -> Vec<sky::WeatherHour> {
        let local = now + self.utc_offset_s;
        let day_start_local = local - local.rem_euclid(86_400);
        let day_start = day_start_local - self.utc_offset_s;
        (0..24)
            .map(|h| {
                let ts = day_start + h * 3600;
                self.hourly
                    .iter()
                    .find(|r| r.0 == ts)
                    .map(|r| sky::WeatherHour { cloud: r.4, rain_mm: r.2, visibility_m: r.5 })
                    .unwrap_or_default()
            })
            .collect()
    }
}

pub fn c_to_f(c: f32) -> f32 {
    c * 9.0 / 5.0 + 32.0
}

/// A short word for a WMO weather code, by day or night.
pub fn description(code: u16, is_day: bool) -> &'static str {
    match code {
        0 => {
            if is_day {
                "Sunny"
            } else {
                "Clear"
            }
        }
        1 => {
            if is_day {
                "Mostly sunny"
            } else {
                "Mostly clear"
            }
        }
        2 => "Partly cloudy",
        3 => "Cloudy",
        45 | 48 => "Foggy",
        51..=57 => "Drizzle",
        61..=67 => "Rain",
        71..=77 => "Snow",
        80..=82 => "Showers",
        85 | 86 => "Snow showers",
        95..=99 => "Thunderstorm",
        _ => "Unsettled",
    }
}

/// The icon name for a WMO weather code, by day or night.
pub fn icon(code: u16, is_day: bool) -> &'static str {
    match code {
        0 | 1 => {
            if is_day {
                "sun-fill"
            } else {
                "moon-fill"
            }
        }
        2 => {
            if is_day {
                "cloud-sun-fill"
            } else {
                "cloud-moon-fill"
            }
        }
        3 => "cloud-fill",
        45 | 48 => "cloud-fog",
        51..=67 | 80..=82 => "cloud-rain-fill",
        71..=77 | 85 | 86 => "cloud-snow",
        95..=99 => "cloud-lightning",
        _ => "cloud",
    }
}

/// "2026-09-26T14:00" in a zone `offset` seconds east of UTC → unix seconds.
pub fn iso_local_to_unix(s: &str, offset: i64) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut hm = time.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi) = (hm.next()??, hm.next().flatten().unwrap_or(0));
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"utc_offset_seconds":-25200,
      "current":{"temperature_2m":25.1,"apparent_temperature":24.0,"relative_humidity_2m":40,"weather_code":3,"is_day":0,"wind_speed_10m":8.2},
      "hourly":{"time":["2026-09-26T00:00","2026-09-26T01:00"],"temperature_2m":[20.0,19.5],"precipitation":[0.0,0.5],"weather_code":[0,61],"cloud_cover":[10,90],"visibility":[24000,8000]},
      "daily":{"temperature_2m_max":[34.4],"temperature_2m_min":[17.5]}}"#;

    #[test]
    fn a_forecast_reads_the_fields_the_page_and_the_sky_need() {
        let d = std::env::temp_dir().join(format!("weather-{}.json", std::process::id()));
        std::fs::write(&d, SAMPLE).unwrap();
        let f = Forecast::load(&d).expect("parses");
        assert_eq!((f.temp_c, f.code), (25.1, 3));
        assert_eq!((f.feels_c, f.humidity, f.wind_kmh), (24.0, Some(40.0), Some(8.2)));
        assert_eq!((f.high_c, f.low_c), (Some(34.4), Some(17.5)));
        assert_eq!(f.hourly.len(), 2);
        // 00:00 local at -7 h is 07:00Z on the 26th.
        let midnight_local = f.hourly[0].0;
        assert_eq!(midnight_local % 86_400, 7 * 3600);
        let hours = f.sky_hours(midnight_local + 600);
        assert_eq!(hours.len(), 24);
        assert!((hours[1].cloud - 0.9).abs() < 1e-6);
        assert_eq!(hours[1].rain_mm, 0.5);
        assert_eq!(hours[5].cloud, 0.0, "missing hours read as clear");
    }

    #[test]
    fn descriptions_and_icons_follow_the_code_and_the_light() {
        assert_eq!(description(0, true), "Sunny");
        assert_eq!(description(0, false), "Clear");
        assert_eq!(description(3, true), "Cloudy");
        assert_eq!(description(63, true), "Rain");
        assert_eq!(icon(0, false), "moon-fill");
        assert_eq!(icon(2, true), "cloud-sun-fill");
        assert_eq!(icon(95, true), "cloud-lightning");
        for code in [0, 1, 2, 3, 45, 51, 61, 71, 80, 85, 95] {
            for day in [true, false] {
                assert!(rill_ui::icons::icon(icon(code, day)).is_some(), "icon for {code}/{day}");
            }
        }
        assert_eq!(c_to_f(25.0).round(), 77.0);
    }
}
