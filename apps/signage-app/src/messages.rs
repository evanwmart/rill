//! The board's one-liners: a corpus in markdown, and a planner that picks
//! one per hour for the next two days.
//!
//! The corpus (`<data>/messages.md`, `apps/signage-app/content/messages.md`
//! in the repo) is headings and numbered lines:
//!
//! ```text
//! # ALL — eligible any time, any day
//! ### Picture-frame consciousness
//! 1. Here is the fucking weather <3
//! # MORNING — only morning
//! 301. Good morning. …
//! ```
//!
//! A `#` heading names a category, the words before the dash: ALL, MORNING,
//! AFTERNOON, EVENING, NIGHT, GOOD WEATHER, HOT, COLD, RAIN, the seven
//! weekdays, SUNSET. A `###` heading is a group inside it (used only to
//! avoid two lines from one group back to back). Edit the file in place;
//! the server re-reads it when it changes.
//!
//! **Planning.** One message per local hour, chosen when the plan is
//! extended (it always reaches 48 h ahead) and kept, so a restart or a
//! forecast refresh does not reshuffle what is already on the glass. Per
//! slot the eligible categories are ALL, the time band (morning 5–11,
//! afternoon 12–16, evening 17–21, night otherwise), the weekday, SUNSET
//! when the hour touches the window from an hour before sunset to half an
//! hour after (sunset from the sky map for the site), and one weather
//! category when a forecast is on disk: RAIN if ≥ 0.3 mm or a rain code,
//! else HOT at ≥ 32 °C, else COLD at ≤ 15 °C, else GOOD WEATHER for a clear
//! 17–29 °C hour. The category is drawn by weight — sunset 100, weather 4,
//! time band 3, ALL 3, weekday 2 — with a generator seeded by the slot, so
//! the same slot always draws the same way; within it the least recently
//! shown line wins, ties by the same generator, and a line from the
//! previous slot's group is passed over once. Shown ids and times persist
//! in `<data>/messages-history.tsv`, so nothing repeats until its category
//! is exhausted, across restarts. Future slots are replanned only when the
//! forecast file changes.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::sky;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    All,
    Morning,
    Afternoon,
    Evening,
    Night,
    GoodWeather,
    Hot,
    Cold,
    Rain,
    /// 0 = Sunday … 6 = Saturday, as `tm_wday`.
    Weekday(u8),
    Sunset,
}

impl Category {
    fn parse(name: &str) -> Option<Category> {
        Some(match name.trim().to_ascii_uppercase().as_str() {
            "ALL" => Category::All,
            "MORNING" => Category::Morning,
            "AFTERNOON" => Category::Afternoon,
            "EVENING" => Category::Evening,
            "NIGHT" => Category::Night,
            "GOOD WEATHER" => Category::GoodWeather,
            "HOT" => Category::Hot,
            "COLD" => Category::Cold,
            "RAIN" => Category::Rain,
            "SUNDAY" => Category::Weekday(0),
            "MONDAY" => Category::Weekday(1),
            "TUESDAY" => Category::Weekday(2),
            "WEDNESDAY" => Category::Weekday(3),
            "THURSDAY" => Category::Weekday(4),
            "FRIDAY" => Category::Weekday(5),
            "SATURDAY" => Category::Weekday(6),
            "SUNSET" => Category::Sunset,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: u32,
    pub text: String,
    pub category: Category,
    pub group: String,
}

/// Parse the corpus. Unknown headings are skipped with their lines.
pub fn parse(text: &str) -> Vec<Message> {
    let mut out = Vec::new();
    let mut category: Option<Category> = None;
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(h) = line.strip_prefix("### ") {
            group = h.trim().to_string();
        } else if let Some(h) = line.strip_prefix("# ") {
            let name = h.split(['—', '-']).next().unwrap_or(h);
            category = Category::parse(name);
            group = name.trim().to_string();
        } else if let Some(cat) = category
            && let Some((num, rest)) = line.split_once(". ")
            && let Ok(id) = num.trim().parse::<u32>()
        {
            let text = rest.trim();
            if !text.is_empty() {
                out.push(Message { id, text: text.to_string(), category: cat, group: group.clone() });
            }
        }
    }
    out
}

/// A built-in corpus for a board with no file yet: the lines above, but
/// only the ones that need no site or forecast.
pub fn builtin() -> Vec<Message> {
    parse(include_str!("../content/messages.md"))
        .into_iter()
        .filter(|m| matches!(m.category, Category::All | Category::Morning | Category::Afternoon | Category::Evening | Category::Night | Category::Weekday(_)))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WeatherKind {
    Rain,
    Hot,
    Cold,
    Good,
}

/// One forecast hour, the three numbers the rules read.
#[derive(Clone, Copy, Debug)]
pub struct WeatherHour {
    pub temp_c: f32,
    pub precip_mm: f32,
    pub code: u16,
}

impl WeatherHour {
    pub fn kind(&self) -> Option<WeatherKind> {
        if self.precip_mm >= 0.3 || self.code >= 51 {
            Some(WeatherKind::Rain)
        } else if self.temp_c >= 32.0 {
            Some(WeatherKind::Hot)
        } else if self.temp_c <= 15.0 {
            Some(WeatherKind::Cold)
        } else if self.code <= 2 && (17.0..=29.0).contains(&self.temp_c) {
            Some(WeatherKind::Good)
        } else {
            None
        }
    }
}

/// What a slot is: the inputs to eligibility.
#[derive(Clone, Copy, Debug)]
pub struct SlotContext {
    pub hour: u8,
    pub weekday: u8,
    pub sunset_window: bool,
    pub weather: Option<WeatherKind>,
}

impl SlotContext {
    fn band(&self) -> Category {
        match self.hour {
            5..=11 => Category::Morning,
            12..=16 => Category::Afternoon,
            17..=21 => Category::Evening,
            _ => Category::Night,
        }
    }

    /// Eligible categories with their weights.
    pub fn eligible(&self) -> Vec<(Category, u32)> {
        let mut v = vec![(Category::All, 3), (self.band(), 3), (Category::Weekday(self.weekday), 2)];
        if let Some(w) = self.weather {
            let c = match w {
                WeatherKind::Rain => Category::Rain,
                WeatherKind::Hot => Category::Hot,
                WeatherKind::Cold => Category::Cold,
                WeatherKind::Good => Category::GoodWeather,
            };
            v.push((c, 4));
        }
        if self.sunset_window {
            v.push((Category::Sunset, 100));
        }
        v
    }
}

/// SplitMix64: a slot's own generator, so a slot draws the same way twice.
fn seeded(mut z: u64) -> impl FnMut() -> u64 {
    move || {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = z;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }
}

const HORIZON_S: i64 = 48 * 3600;

pub struct Planner {
    messages: Vec<Message>,
    corpus_path: Option<PathBuf>,
    corpus_stamp: Option<SystemTime>,
    history_path: Option<PathBuf>,
    /// id → last shown, unix seconds.
    history: HashMap<u32, i64>,
    /// slot start (unix seconds) → index into `messages`.
    plan: BTreeMap<i64, usize>,
    weather_path: Option<PathBuf>,
    weather_stamp: Option<SystemTime>,
    weather: Vec<(i64, WeatherHour)>,
    site: Option<(f64, f64)>,
}

impl Planner {
    /// `data` holds `messages.md`, `weather.json` and the history; absent
    /// files degrade gracefully (built-in corpus, no weather categories).
    pub fn new(data: Option<&Path>, corpus: Option<PathBuf>, site: Option<(f64, f64)>) -> Planner {
        let corpus_path = corpus.or_else(|| data.map(|d| d.join("messages.md")));
        let mut p = Planner {
            messages: Vec::new(),
            corpus_path,
            corpus_stamp: None,
            history_path: data.map(|d| d.join("messages-history.tsv")),
            history: HashMap::new(),
            plan: BTreeMap::new(),
            weather_path: data.map(|d| d.join("weather.json")),
            weather_stamp: None,
            weather: Vec::new(),
            site,
        };
        p.reload_corpus();
        p.load_history();
        p.reload_weather();
        p
    }

    fn mtime(p: &Path) -> Option<SystemTime> {
        std::fs::metadata(p).ok()?.modified().ok()
    }

    fn reload_corpus(&mut self) {
        let loaded = self
            .corpus_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| parse(&t))
            .filter(|m| !m.is_empty());
        self.corpus_stamp = self.corpus_path.as_deref().and_then(Self::mtime);
        self.messages = loaded.unwrap_or_else(builtin);
        self.plan.clear();
    }

    fn load_history(&mut self) {
        let Some(p) = &self.history_path else { return };
        let Ok(text) = std::fs::read_to_string(p) else { return };
        for line in text.lines() {
            if let Some((id, t)) = line.split_once('\t')
                && let (Ok(id), Ok(t)) = (id.parse::<u32>(), t.parse::<i64>())
            {
                self.history.insert(id, t);
            }
        }
    }

    fn save_history(&self) {
        let Some(p) = &self.history_path else { return };
        let mut lines: Vec<String> = self.history.iter().map(|(id, t)| format!("{id}\t{t}")).collect();
        lines.sort();
        let tmp = p.with_extension("tsv.tmp");
        if std::fs::write(&tmp, lines.join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }

    /// Open-Meteo hourly JSON: `hourly.time[]` (ISO local), `temperature_2m`,
    /// `precipitation`, `weather_code`. Anything else is ignored; a file that
    /// does not parse leaves the weather categories off.
    fn reload_weather(&mut self) {
        let Some(p) = &self.weather_path else { return };
        let stamp = Self::mtime(p);
        if stamp == self.weather_stamp && !self.weather.is_empty() {
            return;
        }
        self.weather_stamp = stamp;
        self.weather.clear();
        let Ok(text) = std::fs::read_to_string(p) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        let h = &v["hourly"];
        let times = h["time"].as_array().cloned().unwrap_or_default();
        let nums = |k: &str| -> Vec<f64> {
            h[k].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect()).unwrap_or_default()
        };
        let (temp, precip, code) = (nums("temperature_2m"), nums("precipitation"), nums("weather_code"));
        let offset = v["utc_offset_seconds"].as_i64().unwrap_or(0);
        for (i, t) in times.iter().enumerate() {
            let Some(ts) = t.as_str().and_then(|s| iso_local_to_unix(s, offset)) else { continue };
            self.weather.push((
                ts,
                WeatherHour {
                    temp_c: temp.get(i).copied().unwrap_or(20.0) as f32,
                    precip_mm: precip.get(i).copied().unwrap_or(0.0) as f32,
                    code: code.get(i).copied().unwrap_or(0.0) as u16,
                },
            ));
        }
        // A new forecast replans what has not been shown yet.
        let now = unix_now();
        self.plan.retain(|start, _| *start <= now);
    }

    fn weather_at(&self, slot: i64) -> Option<WeatherKind> {
        self.weather.iter().find(|(t, _)| *t == slot).and_then(|(_, w)| w.kind())
    }

    fn context(&self, slot: i64) -> SlotContext {
        let (hour, weekday) = local_hour_weekday(slot);
        let sunset_window = self
            .site
            .map(|(lat, lon)| {
                let s = sky::site_now(lat, lon, slot);
                match sky::sunrise_sunset(&s).1 {
                    Some(sunset_min) => {
                        let slot_min = (hour as i32) * 60;
                        let (lo, hi) = (sunset_min - 60, sunset_min + 30);
                        slot_min < hi && slot_min + 60 > lo
                    }
                    None => false,
                }
            })
            .unwrap_or(false);
        SlotContext { hour, weekday, sunset_window, weather: self.weather_at(slot) }
    }

    fn choose(&self, slot: i64, prev_group: Option<&str>) -> Option<usize> {
        let ctx = self.context(slot);
        let mut rng = seeded(slot as u64 ^ 0x5EED_0F7E_D4A7_0001);
        let eligible: Vec<(Category, u32)> =
            ctx.eligible().into_iter().filter(|(c, _)| self.messages.iter().any(|m| m.category == *c)).collect();
        if eligible.is_empty() {
            return None;
        }
        let total: u32 = eligible.iter().map(|(_, w)| w).sum();
        let mut pick = (rng() % total as u64) as u32;
        let mut category = eligible[0].0;
        for (c, w) in &eligible {
            if pick < *w {
                category = *c;
                break;
            }
            pick -= w;
        }
        // Least recently shown within the category; never-shown first.
        let mut candidates: Vec<(i64, u64, usize)> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.category == category)
            .map(|(i, m)| (self.history.get(&m.id).copied().unwrap_or(0), rng(), i))
            .collect();
        candidates.sort();
        let first = candidates.first()?.2;
        if let Some(g) = prev_group
            && self.messages[first].group == g
            && let Some(alt) = candidates.iter().find(|(_, _, i)| self.messages[*i].group != g)
        {
            return Some(alt.2);
        }
        Some(first)
    }

    /// Make sure every hour from `now` to 48 h ahead has a line.
    fn extend(&mut self, now: i64) {
        let first = now - now.rem_euclid(3600);
        let mut prev_group: Option<String> =
            self.plan.range(..first).next_back().map(|(_, i)| self.messages[*i].group.clone());
        let mut changed = false;
        let mut slot = first;
        while slot <= first + HORIZON_S {
            if let Some(i) = self.plan.get(&slot) {
                prev_group = Some(self.messages[*i].group.clone());
            } else if let Some(i) = self.choose(slot, prev_group.as_deref()) {
                self.plan.insert(slot, i);
                self.history.insert(self.messages[i].id, slot);
                prev_group = Some(self.messages[i].group.clone());
                changed = true;
            }
            slot += 3600;
        }
        self.plan.retain(|start, _| *start >= first - 24 * 3600);
        if changed {
            self.save_history();
        }
    }

    /// The line for `now`, planning ahead as needed. Re-reads the corpus or
    /// the forecast when either file changed.
    pub fn message_for(&mut self, now: i64) -> String {
        if self.corpus_path.as_deref().and_then(Self::mtime) != self.corpus_stamp {
            self.reload_corpus();
        }
        self.reload_weather();
        self.extend(now);
        let slot = now - now.rem_euclid(3600);
        self.plan.get(&slot).map(|i| self.messages[*i].text.clone()).unwrap_or_default()
    }

    /// The plan as (slot start, id, text).
    #[cfg(test)]
    pub fn plan(&self) -> Vec<(i64, u32, String)> {
        self.plan.iter().map(|(s, i)| (*s, self.messages[*i].id, self.messages[*i].text.clone())).collect()
    }

    /// How many lines the corpus holds.
    pub fn count(&self) -> usize {
        self.messages.len()
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Local hour and weekday of a unix instant, by the C library's zone.
fn local_hour_weekday(t: i64) -> (u8, u8) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let secs: libc::time_t = t as libc::time_t;
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return (((t.rem_euclid(86_400)) / 3600) as u8, (((t / 86_400) + 4).rem_euclid(7)) as u8);
    }
    (tm.tm_hour as u8, tm.tm_wday as u8)
}

/// "2026-09-26T14:00" in a zone `offset` seconds east of UTC → unix seconds.
fn iso_local_to_unix(s: &str, offset: i64) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut hm = time.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi) = (hm.next()??, hm.next().flatten().unwrap_or(0));
    // Days from civil (Howard Hinnant's algorithm).
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

    #[test]
    fn the_corpus_parses_into_its_categories() {
        let m = parse(include_str!("../content/messages.md"));
        assert_eq!(m.len(), 730);
        let count = |c: Category| m.iter().filter(|x| x.category == c).count();
        assert_eq!(count(Category::All), 300);
        assert_eq!(count(Category::Morning), 60);
        assert_eq!(count(Category::Sunset), 20);
        assert_eq!(count(Category::Weekday(2)), 15, "Tuesday");
        assert_eq!(count(Category::Rain), 25);
        assert_eq!(m[0].group, "Picture-frame consciousness");
        assert_eq!(m.iter().find(|x| x.id == 121).unwrap().group, "Couple / household");
        assert_eq!(m.iter().find(|x| x.id == 301).unwrap().group, "MORNING");
    }

    #[test]
    fn weather_rules_in_order() {
        let w = |t, p, c| WeatherHour { temp_c: t, precip_mm: p, code: c }.kind();
        assert_eq!(w(20.0, 1.0, 0), Some(WeatherKind::Rain));
        assert_eq!(w(35.0, 0.0, 61), Some(WeatherKind::Rain), "rain beats heat");
        assert_eq!(w(34.0, 0.0, 0), Some(WeatherKind::Hot));
        assert_eq!(w(10.0, 0.0, 3), Some(WeatherKind::Cold));
        assert_eq!(w(22.0, 0.0, 1), Some(WeatherKind::Good));
        assert_eq!(w(22.0, 0.0, 3), None, "overcast and mild: nothing to say");
        assert_eq!(w(30.5, 0.0, 0), None);
    }

    #[test]
    fn bands_and_eligibility() {
        let ctx = |hour, sunset, weather| SlotContext { hour, weekday: 2, sunset_window: sunset, weather };
        assert_eq!(ctx(7, false, None).band(), Category::Morning);
        assert_eq!(ctx(13, false, None).band(), Category::Afternoon);
        assert_eq!(ctx(19, false, None).band(), Category::Evening);
        assert_eq!(ctx(23, false, None).band(), Category::Night);
        assert_eq!(ctx(2, false, None).band(), Category::Night);
        let e = ctx(18, true, Some(WeatherKind::Hot)).eligible();
        assert!(e.contains(&(Category::Sunset, 100)));
        assert!(e.contains(&(Category::Hot, 4)));
        assert!(e.contains(&(Category::Weekday(2), 2)));
        assert_eq!(ctx(18, false, None).eligible().len(), 3);
    }

    fn planner(dir: &str) -> Planner {
        let d = std::env::temp_dir().join(format!("morning-msgs-{}-{dir}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("messages.md"), include_str!("../content/messages.md")).unwrap();
        Planner::new(Some(&d), None, Some((34.15, -118.45)))
    }

    #[test]
    fn a_plan_covers_two_days_without_repeats_and_is_deterministic() {
        let mut p = planner("plan");
        let now = 1_790_400_000;
        let first = p.message_for(now);
        assert!(!first.is_empty());
        let plan = p.plan();
        assert!(plan.len() >= 48, "{}", plan.len());
        let mut ids: Vec<u32> = plan.iter().map(|(_, id, _)| *id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "no line twice in one plan");
        // Same data, same slot, same draw.
        let mut q = planner("plan2");
        assert_eq!(q.message_for(now), first);
        // The chosen line is still the same an hour later (plans are kept).
        assert_eq!(p.message_for(now + 1800), first);
        assert_ne!(p.message_for(now + 3600), first);
    }

    #[test]
    fn history_survives_a_restart_and_pushes_shown_lines_back() {
        let d = std::env::temp_dir().join(format!("morning-msgs-{}-hist", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("messages.md"), "# ALL — x\n1. one\n2. two\n3. three\n").unwrap();
        let mut a = Planner::new(Some(&d), None, None);
        let now = 1_790_400_000;
        let mut seen = std::collections::HashSet::new();
        for k in 0..3 {
            seen.insert(a.message_for(now + k * 3600));
        }
        assert_eq!(seen.len(), 3, "three lines, three slots, all different");
        assert!(d.join("messages-history.tsv").exists());
        let b = Planner::new(Some(&d), None, None);
        assert_eq!(b.history.len(), 3);
    }

    #[test]
    fn the_sunset_hour_gets_a_sunset_line() {
        let mut p = planner("sunset");
        // Walk a day and find the slot the planner marks as the sunset
        // window; its line must come from SUNSET.
        let day0 = 1_790_400_000 - 1_790_400_000_i64.rem_euclid(86_400);
        let mut found = false;
        for h in 0..24 {
            let slot = day0 + h * 3600;
            let ctx = p.context(slot);
            if ctx.sunset_window {
                let text = p.message_for(slot);
                let m = p.messages.iter().find(|m| m.text == text).unwrap();
                assert_eq!(m.category, Category::Sunset, "{text}");
                found = true;
            }
        }
        assert!(found, "some hour touched the sunset window");
    }

    #[test]
    fn iso_times_convert() {
        // 2026-09-26T14:00 at UTC-7 = 21:00Z.
        let t = iso_local_to_unix("2026-09-26T14:00", -7 * 3600).unwrap();
        assert_eq!(t % 3600, 0);
        let (h, _) = ((t / 3600) % 24, 0);
        assert_eq!(h, 21);
    }
}
