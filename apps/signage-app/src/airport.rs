//! The departures board: live data at density, with churn, and honest
//! about a dropped feed.
//!
//! A schedule is simulated rather than fetched — the demo has to run on a
//! Pi in a room with no airline API — but it moves the way a real one
//! does: flights walk from *scheduled* through *boarding* and *final call*
//! to *departed* on the clock, gates change, delays arrive and grow, one
//! in forty is cancelled, and departed flights fall off the bottom while
//! new ones join the top of the queue. The simulation is deterministic
//! from its seed and the start time, so two glasses pointed at one server
//! agree.
//!
//! The document carries `live every=1000`, so the glass re-reads the board
//! once a second. `revision` moves only when the *rendered* board would
//! differ — a state change, or the minute hand — so almost every poll is
//! answered NOT_MODIFIED from a counter without rendering anything. That
//! is the point being made: a 1 s poll that costs nothing until there is
//! news.
//!
//! **The gate.** `/gate` is the same schedule seen from one gate: the
//! flight boarding there now (groups, standby list, the times), the one
//! after it, and a gate-change strip when a flight is moved away. The
//! simulation keeps a home gate (`HOME_GATE`) turning every ~40 minutes
//! so the display always has something on it. This is the demo target —
//! a gate screen is what a traveller stands in front of — and it shows a
//! seconds clock, so its revision moves every second; the terminal board
//! is the one that proves the NOT_MODIFIED poll.
//!
//! **Stale on drop.** A board that silently keeps showing yesterday's
//! rows is worse than a dark one. When the feed is down (the operator
//! creates `<data>/feed-down`; a real board would watch its upstream) the
//! simulation stops, the rows stay, and a banner says when the last update
//! was and how long ago — counting up, so a viewer can see it is still
//! counting.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rill_auth::Identity;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

/// The poll clock. One second is the fastest a departures board is ever
/// read; most of those polls come back NOT_MODIFIED.
const LIVE_MS: u16 = 1000;

/// How many flights the board shows. Fourteen rows at 1080p is a board
/// you can read from across a hall.
const ROWS: usize = 14;

/// The clock the schedule runs on: minutes before departure at which each
/// thing happens. `REAL` is what a traveller would recognise; `DEMO` is
/// the same sequence with one step a minute, looping — what a glass on a
/// desk shows while someone watches it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeline {
    /// Boarding opens this many minutes before departure.
    pub boarding_min: i64,
    /// Final call (every group) from here to departure.
    pub final_call_min: i64,
    /// Each boarding step — pre-boarding, then each group — lasts this long.
    pub group_min: i64,
    /// How long a departed flight stays on the board before it falls off.
    pub linger_min: i64,
    /// The home gate gets a new flight this often.
    pub home_turn_min: i64,
    /// Scripted: no random delays, gate changes or cancellations touch
    /// the home gate, and its flights are spaced exactly `home_turn_min`
    /// apart — the sequence is a loop, not a simulation.
    pub scripted: bool,
}

impl Timeline {
    pub const REAL: Timeline = Timeline {
        boarding_min: 30,
        final_call_min: 10,
        group_min: 4,
        linger_min: 4,
        home_turn_min: 38,
        scripted: false,
    };
    /// One state a minute, ten minutes a loop: boards in 2 · boards in 1 ·
    /// pre-boarding · groups 1–4 · final call · departed · (next flight).
    pub const DEMO: Timeline = Timeline {
        boarding_min: 6,
        final_call_min: 1,
        group_min: 1,
        linger_min: 1,
        home_turn_min: 8,
        scripted: true,
    };
}

/// The face the gate display is set in: Inter's static instances
/// (Light–ExtraBold), installed as user fonts under
/// `~/.local/share/fonts/`. The variable cut rendered with slivers through
/// the large glyphs on this renderer; the statics do not. A glass without
/// the family falls back to the bundled UI face and still reads.
const FACE: &str = "Inter";

/// The gate the `/gate` display is bolted to. Never assigned at random;
/// the queue keeps a flight here every `Timeline::home_turn_min`.
pub const HOME_GATE: &str = "B12";
/// Boarding groups after pre-boarding.
const GROUPS: usize = 4;
const STANDBY: usize = 14;
/// How many standby names the column shows at once; a longer list turns
/// pages every few seconds, the last page cut short rather than padded
/// with names already shown.
const STANDBY_ROWS: usize = 11;
const STANDBY_CYCLE_SECS: i64 = 6;

const SURNAMES: &[&str] = &[
    "MARTIN", "OKAFOR", "SATO", "LINDQVIST", "DUBOIS", "NAKAMURA", "ROSSI", "HAAS",
    "PEREIRA", "KOWALSKI", "NOVAK", "SINGH", "ANDERSEN", "MORALES", "IBRAHIM", "CHEN",
    "FISCHER", "BRENNAN", "TANAKA", "SCHMIDT", "GARCIA", "JANSEN", "POPOV", "REYES",
];

/// One carrier: the gate is Rill Air's, and the mark on it is theirs.
const AIRLINES: &[(&str, &str)] = &[("RL", "Rill Air")];

const DESTINATIONS: &[&str] = &[
    "Amsterdam", "Berlin", "Chicago", "Copenhagen", "Denver", "Dublin", "Helsinki",
    "Lisbon", "London Heathrow", "Madrid", "Montréal", "Oslo", "Paris CDG", "Prague",
    "Reykjavík", "Rome", "San Francisco", "Seattle", "Stockholm", "Tokyo Haneda",
    "Toronto", "Vancouver", "Vienna", "Warsaw", "Zürich",
];

/// xorshift64*: small, seedable, and the same on every machine — what a
/// simulation wants from a random source. Not for anything secret.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Flight {
    /// "RL 412" — carrier code and number.
    code: String,
    airline: &'static str,
    destination: &'static str,
    /// Scheduled departure, unix seconds. Always on a whole minute.
    scheduled: i64,
    /// Announced delay, minutes. Grows; never shrinks.
    delay_min: i64,
    gate: String,
    cancelled: bool,
    /// Set when a gate change moved this flight off a gate: the old gate
    /// shows the change until the flight departs.
    moved_from: Option<String>,
}

impl Flight {
    fn expected(&self) -> i64 {
        self.scheduled + self.delay_min * 60
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Scheduled,
    Delayed,
    Boarding,
    FinalCall,
    Departed,
    Cancelled,
}

impl Phase {
    fn of(f: &Flight, now: i64, tl: &Timeline) -> Phase {
        if f.cancelled {
            return Phase::Cancelled;
        }
        let minutes_left = (f.expected() - now).div_euclid(60);
        if minutes_left < 0 {
            Phase::Departed
        } else if minutes_left < tl.final_call_min {
            Phase::FinalCall
        } else if minutes_left < tl.boarding_min {
            Phase::Boarding
        } else if f.delay_min > 0 {
            Phase::Delayed
        } else {
            Phase::Scheduled
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Scheduled => "On time",
            Phase::Delayed => "Delayed",
            Phase::Boarding => "Boarding",
            Phase::FinalCall => "Final call",
            Phase::Departed => "Departed",
            Phase::Cancelled => "Cancelled",
        }
    }

    /// The style each phase wears. Colour carries meaning on a board;
    /// these are the conventions a traveller already knows.
    fn style(self) -> &'static str {
        match self {
            Phase::Scheduled => "st-ok",
            Phase::Delayed => "st-warn",
            Phase::Boarding => "st-go",
            Phase::FinalCall => "st-warn",
            Phase::Departed => "st-gone",
            Phase::Cancelled => "st-bad",
        }
    }
}

struct State {
    tl: Timeline,
    rng: Rng,
    flights: Vec<Flight>,
    next_number: u32,
    /// Bumped on every change to `flights`. Part of the revision.
    changes: u64,
    /// The unix second the simulation last advanced to.
    ticked_at: i64,
    /// When the feed was last known good — what the stale banner reports.
    last_update: i64,
}

pub struct Board {
    state: Mutex<State>,
    /// The operator's switch: while this file exists the feed is "down".
    feed_flag: PathBuf,
}

impl Board {
    pub fn new(data: PathBuf, seed: u64, tl: Timeline) -> Board {
        let now = unix_now();
        let mut st = State {
            tl,
            rng: Rng(seed.max(1)),
            flights: Vec::new(),
            next_number: 100,
            changes: 0,
            ticked_at: now,
            last_update: now,
        };
        // Seed the board so the first viewer sees a full one, with the
        // earliest flights already boarding.
        let mut t = now - now.rem_euclid(60) + 5 * 60;
        for i in 0..ROWS {
            let mut f = new_flight(&mut st, t);
            // The home gate's first flight boards as soon as someone looks;
            // scripted, it opens exactly two minutes in ("Boards in 2").
            if i == 2 {
                f.gate = HOME_GATE.to_string();
                if tl.scripted {
                    f.scheduled = now - now.rem_euclid(60) + (tl.boarding_min + 2) * 60;
                }
            }
            st.flights.push(f);
            t += (2 + st.rng.below(5) as i64) * 60;
        }
        Board { state: Mutex::new(st), feed_flag: data.join("feed-down") }
    }

    fn feed_down(&self) -> bool {
        self.feed_flag.exists()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Advance the simulation to `now`. Idempotent within a second; a no-op
    /// while the feed is down, which is what "stale" means.
    fn tick(&self, now: i64) {
        if self.feed_down() {
            return;
        }
        let mut st = self.lock();
        if now <= st.ticked_at {
            return;
        }
        // Each elapsed second is one roll of the dice, so a board that was
        // not looked at for a while catches up rather than jumping.
        let steps = (now - st.ticked_at).min(600);
        st.ticked_at = now;
        st.last_update = now;
        for i in 0..steps {
            let t = now - (steps - 1 - i);
            step(&mut st, t);
        }
    }

    /// What the board shows depends on the minute and on the flights; the
    /// stale banner adds the second while the feed is down.
    fn stamp(&self, now: i64) -> u64 {
        let st = self.lock();
        let minute = now.div_euclid(60) as u64;
        let stale = if self.feed_down() { (now - st.last_update) as u64 } else { 0 };
        (st.changes << 40) ^ (minute << 12) ^ stale
    }

    fn page(&self, now: i64) -> Result<Vec<u8>, Status> {
        let st = self.lock();
        let down = self.feed_down();
        let mut kdl = String::from(
            "style \"board\" padding=0 gap=0 width=\"fill\" height=\"fill\"\n\
             style \"head\" padding-x=56 padding-y=22 gap=24 valign=\"center\" width=\"fill\" background=\"#111a2e\"\n\
             style \"brand\" size=40 weight=700 color=\"#f2f5fa\"\n\
             style \"brand-sub\" size=22 weight=500 color=\"#8fa0bd\"\n\
             style \"title\" size=46 weight=800 color=\"#ffd166\" align=\"center\"\n\
             style \"clock\" size=56 weight=700 color=\"#f2f5fa\" font=\"mono\" align=\"right\"\n\
             style \"cols\" padding-x=56 padding-y=10 gap=0 width=\"fill\" background=\"#0e1526\"\n\
             style \"col\" size=22 weight=700 color=\"#6f7f9c\"\n\
             style \"row-a\" padding-x=56 padding-y=0 gap=0 width=\"fill\" height=60 valign=\"center\" background=\"#0a0f1c\"\n\
             style \"row-b\" padding-x=56 padding-y=0 gap=0 width=\"fill\" height=60 valign=\"center\" background=\"#0d1424\"\n\
             style \"c-time\" width=200 padding=0\n\
             style \"c-flight\" width=220 padding=0\n\
             style \"c-dest\" width=760 padding=0\n\
             style \"c-gate\" width=180 padding=0\n\
             style \"c-status\" width=448 padding=0\n\
             style \"time\" size=38 weight=600 color=\"#f2f5fa\" font=\"mono\"\n\
             style \"time-old\" size=38 weight=600 color=\"#6f7f9c\" font=\"mono\"\n\
             style \"flight\" size=34 weight=600 color=\"#cfd8e6\" font=\"mono\"\n\
             style \"dest\" size=38 weight=600 color=\"#f2f5fa\"\n\
             style \"dest-gone\" size=38 weight=600 color=\"#6f7f9c\"\n\
             style \"gate\" size=36 weight=700 color=\"#ffd166\" font=\"mono\"\n\
             style \"st-ok\" size=34 weight=600 color=\"#8fa0bd\"\n\
             style \"st-go\" size=34 weight=800 color=\"#3ddc97\"\n\
             style \"st-warn\" size=34 weight=800 color=\"#ffb454\"\n\
             style \"st-bad\" size=34 weight=800 color=\"#ff5c6c\"\n\
             style \"st-gone\" size=34 weight=600 color=\"#6f7f9c\"\n\
             style \"foot\" padding-x=56 padding-y=16 gap=24 valign=\"center\" width=\"fill\" background=\"#111a2e\"\n\
             style \"foot-text\" size=22 weight=500 color=\"#8fa0bd\"\n\
             style \"feed-ok\" size=22 weight=700 color=\"#3ddc97\"\n\
             style \"stale\" padding-x=56 padding-y=14 gap=24 valign=\"center\" width=\"fill\" background=\"#5a1a22\"\n\
             style \"stale-text\" size=28 weight=800 color=\"#ffd7db\"\n\
             style \"stale-sub\" size=24 weight=600 color=\"#ffb3ba\" font=\"mono\"\n\n\
             column style=\"board\" {\n\
             \tpage background=\"#0a0f1c\"\n",
        );

        let clock = hhmm(now);
        kdl.push_str(&format!(
            "\trow style=\"head\" {{\n\
             \t\tcolumn gap=0 {{ text \"Rill International\" style=\"brand\"; text \"Terminal B\" style=\"brand-sub\" }}\n\
             \t\tspacer\n\
             \t\ttext \"DEPARTURES\" style=\"title\"\n\
             \t\tspacer\n\
             \t\ttext \"{clock}\" style=\"clock\"\n\
             \t}}\n"
        ));

        if down {
            let ago = now - st.last_update;
            kdl.push_str(&format!(
                "\trow style=\"stale\" {{\n\
                 \t\ttext \"DATA FEED LOST — showing last known departures\" style=\"stale-text\"\n\
                 \t\tspacer\n\
                 \t\ttext \"last update {} · {} ago\" style=\"stale-sub\"\n\
                 \t}}\n",
                hhmmss(st.last_update),
                ago_text(ago)
            ));
        }

        kdl.push_str(
            "\trow style=\"cols\" {\n\
             \t\trow style=\"c-time\" { text \"TIME\" style=\"col\" }\n\
             \t\trow style=\"c-flight\" { text \"FLIGHT\" style=\"col\" }\n\
             \t\trow style=\"c-dest\" { text \"DESTINATION\" style=\"col\" }\n\
             \t\trow style=\"c-gate\" { text \"GATE\" style=\"col\" }\n\
             \t\trow style=\"c-status\" { text \"STATUS\" style=\"col\" }\n\
             \t}\n",
        );

        let mut shown: Vec<&Flight> = st.flights.iter().collect();
        shown.sort_by_key(|f| (f.scheduled, f.code.clone()));
        for (i, f) in shown.iter().take(ROWS).enumerate() {
            let phase = Phase::of(f, if down { st.last_update } else { now }, &st.tl);
            let gone = matches!(phase, Phase::Departed | Phase::Cancelled);
            let (time_style, dest_style) = if gone { ("time-old", "dest-gone") } else { ("time", "dest") };
            let status = match phase {
                Phase::Delayed => format!("Delayed · now {}", hhmm(f.expected())),
                Phase::Boarding if f.delay_min > 0 => format!("Boarding · {}", hhmm(f.expected())),
                _ => phase.label().to_string(),
            };
            let gate = if f.cancelled { "—" } else { f.gate.as_str() };
            kdl.push_str(&format!(
                "\trow style=\"{row}\" {{\n\
                 \t\trow style=\"c-time\" {{ text \"{time}\" style=\"{time_style}\" }}\n\
                 \t\trow style=\"c-flight\" {{ text {code} style=\"flight\" }}\n\
                 \t\trow style=\"c-dest\" {{ text {dest} style=\"{dest_style}\" }}\n\
                 \t\trow style=\"c-gate\" {{ text {gate} style=\"gate\" }}\n\
                 \t\trow style=\"c-status\" {{ text {status} style=\"{st}\" }}\n\
                 \t}}\n",
                row = if i % 2 == 0 { "row-a" } else { "row-b" },
                time = hhmm(f.scheduled),
                code = rill_doc::kdl_escape(&f.code),
                dest = rill_doc::kdl_escape(f.destination),
                gate = rill_doc::kdl_escape(gate),
                status = rill_doc::kdl_escape(&status),
                st = phase.style(),
            ));
        }

        kdl.push_str("\tspacer\n");
        let feed = if down {
            "text \"● feed down\" style=\"st-bad\"".to_string()
        } else {
            format!("text \"● live · updated {}\" style=\"feed-ok\"", hhmmss(st.last_update))
        };
        kdl.push_str(&format!(
            "\trow style=\"foot\" {{\n\
             \t\t{feed}\n\
             \t\tspacer\n\
             \t\ttext \"Gates close 10 minutes before departure · Please have your boarding pass ready\" style=\"foot-text\"\n\
             \t\tspacer\n\
             \t\ttext \"rill · one document, re-read every second\" style=\"foot-text\"\n\
             \t}}\n"
        ));
        kdl.push_str(&format!("\tlive target=\"/airport\" every={LIVE_MS}\n}}\n"));
        rill_appkit::compile_page("signage-app", &kdl)
    }

    /// The home gate's view: the flight there now (the earliest one that
    /// has not fallen off), the one after it, and any flight just moved
    /// away from it.
    fn gate_flights(st: &State) -> (Option<&Flight>, Option<&Flight>, Option<&Flight>) {
        let mut here: Vec<&Flight> = st.flights.iter().filter(|f| f.gate == HOME_GATE).collect();
        here.sort_by_key(|f| f.expected());
        let moved = st
            .flights
            .iter()
            .filter(|f| f.moved_from.as_deref() == Some(HOME_GATE) && !f.cancelled)
            .min_by_key(|f| f.expected());
        (here.first().copied(), here.get(1).copied(), moved)
    }

    fn gate_stamp(&self, now: i64) -> u64 {
        let st = self.lock();
        (st.changes << 40) ^ (now as u64)
    }

    fn gate_page(&self, now: i64) -> Result<Vec<u8>, Status> {
        let st = self.lock();
        let down = self.feed_down();
        let shown = if down { st.last_update } else { now };
        let (cur, next, moved) = Board::gate_flights(&st);
        let q = rill_doc::kdl_escape;
        // Everything sits on an 8 px grid: margins 64, band 224, footer 96,
        // gaps 16/24/32/48, tiles 320, card 544. One face (`FACE`, the
        // bundled one if it is not installed) and Evan's five: Black
        // #0A0908, Jet Black #22333B, White Smoke #F2F4F3, Fresh Sky
        // #00A7E1, Tomato Jam #CC2936 — tints are those five with alpha. Pills, chips and avatars are
        // rows around a text: a container is what honours padding, a
        // fixed size and a corner radius.
        let mut kdl = "\
style \"gate\" padding=0 gap=0 width=\"fill\" height=\"fill\" background=\"#00000000\"
style \"band\" padding-x=64 padding-y=0 gap=32 valign=\"center\" width=\"fill\" height=224 background=\"#F2F4F3\"
style \"logo\" color=\"#00A7E1\"
style \"title\" gap=4 width=1120 padding=0
style \"flight-row\" gap=20 valign=\"center\" padding=0
style \"airline\" size=28 weight=600 color=\"#0A0908\" font=\"@F\"
style \"code\" size=28 weight=400 color=\"#22333BB3\" font=\"@F\"
style \"dest\" size=112 weight=700 color=\"#0A0908\" font=\"@F\" wrap=#false
style \"status\" gap=12 valign=\"center\" padding=0
style \"dot-ok\" background=\"#00A7E1\" corner=7
style \"dot-warn\" background=\"#CC2936\" corner=7
style \"dot-quiet\" background=\"#22333B80\" corner=7
style \"status-ok\" size=30 weight=600 color=\"#00A7E1\" font=\"@F\" wrap=#false
style \"status-warn\" size=30 weight=600 color=\"#CC2936\" font=\"@F\" wrap=#false
style \"status-quiet\" size=30 weight=500 color=\"#22333B80\" font=\"@F\" wrap=#false
style \"gate-col\" gap=4 width=320 padding=0
style \"gate-line\" gap=20 valign=\"center\" padding=0
style \"gate-word\" size=28 weight=600 color=\"#22333BB3\" font=\"@F\"
style \"gate-id\" size=112 weight=700 color=\"#00A7E1\" font=\"@F\" wrap=#false
style \"field\" padding-x=64 padding-y=48 gap=64 width=\"fill\" height=\"fill\" background=\"#22333BCC\"
style \"main\" gap=16 width=\"fill\" height=\"fill\"
style \"head-row\" gap=20 valign=\"center\" padding=0
style \"head-icon\" color=\"#00A7E1\"
style \"headline\" size=96 weight=700 color=\"#F2F4F3\" font=\"@F\" wrap=#false
style \"subline\" size=32 weight=600 color=\"#F2F4F3CC\" font=\"@F\" wrap=#true
style \"tracker\" gap=16 valign=\"top\" padding=0 padding-y=16
style \"seg\" gap=12 width=176 padding=0
style \"seg-last\" gap=12 width=272 padding=0
style \"seg-done\" background=\"#00A7E1\" corner=4
style \"seg-now\" background=\"#F2F4F3\" corner=4
style \"seg-wait\" background=\"#F2F4F326\" corner=4
style \"seg-done-t\" size=22 weight=500 color=\"#F2F4F3A6\" font=\"@F\" wrap=#false
style \"seg-now-t\" size=22 weight=600 color=\"#F2F4F3\" font=\"@F\" wrap=#false
style \"seg-wait-t\" size=22 weight=400 color=\"#F2F4F359\" font=\"@F\" wrap=#false
style \"indent\" gap=0 padding=0
style \"route\" gap=0 valign=\"center\" padding=0 width=1232
style \"iata\" size=56 weight=500 color=\"#F2F4F3\" font=\"mono\"
style \"arc\" gap=0 valign=\"top\" padding=0
style \"arc-slot\" gap=0 width=15 align=\"center\" padding=0
style \"arc-dot\" background=\"#00A7E1\" corner=4
style \"arc-col\" gap=6 width=960 align=\"center\" padding=0
style \"duration\" size=24 weight=500 color=\"#F2F4F3A6\" font=\"@F\" align=\"center\" width=\"fill\"
style \"plane\" color=\"#F2F4F3\"
style \"times\" gap=8 width=1232 padding=0
style \"tiles\" gap=16 valign=\"top\" padding=0 width=\"fill\"
style \"tile\" width=360 padding=0 gap=8
style \"tile-r\" width=360 padding=0 gap=8
style \"tile-rule\" background=\"#F2F4F326\" corner=0
style \"tile-rule-warn\" background=\"#CC2936\" corner=0
style \"tile-head\" gap=12 valign=\"center\" padding=0 padding-y=8
style \"tile-icon\" color=\"#F2F4F3\"
style \"lbl\" size=24 weight=500 color=\"#F2F4F3A6\" font=\"@F\"
style \"big\" size=60 weight=600 color=\"#F2F4F3\" font=\"@F\" wrap=#false
style \"big-old\" size=60 weight=500 color=\"#F2F4F366\" font=\"@F\" wrap=#false
style \"big-late\" size=60 weight=600 color=\"#CC2936\" font=\"@F\" wrap=#false
style \"note\" size=22 weight=400 color=\"#F2F4F3A6\" font=\"@F\"
style \"note-warn\" size=22 weight=500 color=\"#CC2936\" font=\"@F\"
style \"card\" width=400 padding-x=0 padding-y=0 gap=12
style \"card-head\" gap=12 valign=\"center\" padding=0
style \"card-icon\" color=\"#00A7E1\"
style \"card-h\" size=32 weight=600 color=\"#F2F4F3\" font=\"@F\"
style \"count-t\" size=26 weight=400 color=\"#F2F4F3A6\" font=\"@F\"
style \"card-rule\" background=\"#F2F4F326\" corner=0
style \"col\" gap=4 width=\"fill\" padding=0
style \"sb\" gap=14 valign=\"center\" height=48 padding=0
style \"sb-ok\" background=\"#00A7E1\" corner=6
style \"sb-wait\" background=\"#F2F4F340\" corner=6
style \"sb-no\" background=\"#CC2936\" corner=6
style \"sb-name\" size=28 weight=500 color=\"#F2F4F3\" font=\"@F\"
style \"sb-name-dim\" size=28 weight=400 color=\"#F2F4F3A6\" font=\"@F\"
style \"sb-name-off\" size=28 weight=400 color=\"#F2F4F359\" font=\"@F\"
style \"foot\" padding-x=64 padding-y=0 gap=16 valign=\"center\" width=\"fill\" height=96 background=\"#F2F4F3\"
style \"foot-icon\" color=\"#00A7E1\"
style \"foot-icon-dim\" color=\"#22333B80\"
style \"foot-text\" size=26 weight=500 color=\"#0A0908\" font=\"@F\"
style \"foot-next\" size=26 weight=400 color=\"#22333BB3\" font=\"@F\"
style \"foot-clock\" size=28 weight=600 color=\"#0A0908\" font=\"@F\" align=\"right\"
style \"alert\" padding-x=64 padding-y=16 gap=20 valign=\"center\" width=\"fill\" background=\"#00A7E1\"
style \"alert-icon\" color=\"#0A0908\"
style \"alert-text\" size=30 weight=600 color=\"#0A0908\" font=\"@F\"
style \"stale\" padding-x=64 padding-y=14 gap=20 valign=\"center\" width=\"fill\" background=\"#CC2936\"
style \"stale-icon\" color=\"#F2F4F3\"
style \"stale-text\" size=28 weight=600 color=\"#F2F4F3\" font=\"@F\"
style \"stale-sub\" size=24 weight=400 color=\"#F2F4F3CC\" font=\"mono\"

column style=\"gate\" {
\tpage background=\"#00000000\"
"
        .replace("@F", FACE);

        let tl = st.tl;
        let phase = cur.map(|f| Phase::of(f, shown, &tl));
        let step = cur.and_then(|f| boarding_step(f, shown, &tl));

        // The band: mark, airline and flight, destination, status, gate.
        let (pill, tone) = match (phase, step) {
            (None, _) => ("Gate closed", "quiet"),
            (Some(Phase::Cancelled), _) => ("Cancelled", "warn"),
            (Some(Phase::Departed), _) => ("Departed", "quiet"),
            (_, Some(s)) if s > GROUPS => ("Final call", "warn"),
            (_, Some(_)) => ("Boarding", "ok"),
            (Some(Phase::Delayed), _) => ("Delayed", "warn"),
            _ => ("On time", "ok"),
        };
        let status = format!("row style=\"status\" {{ rect style=\"dot-{tone}\" width=14 height=14; text \"{pill}\" style=\"status-{tone}\" }}");
        let flight_row = match cur {
            Some(f) => format!(
                "row style=\"flight-row\" {{ text {} style=\"airline\"; text {} style=\"code\"; {status} }}",
                q(&f.airline.to_uppercase()),
                q(&format!("Flight {}", f.code))
            ),
            None => format!("row style=\"flight-row\" {{ text \"RILL AIR\" style=\"airline\"; {status} }}"),
        };
        let dest_line = cur.map(|f| f.destination.to_string()).unwrap_or_else(|| "Gate closed".to_string());
        kdl.push_str(&format!(
            "\trow style=\"band\" {{\n\
             \t\ticon \"rill-air\" style=\"logo\" size=128\n\
             \t\tcolumn style=\"title\" {{ {flight_row}; text {dest} style=\"dest\" }}\n\
             \t\tspacer\n\
             \t\tcolumn style=\"gate-col\" {{ row style=\"gate-line\" {{ spacer; text \"GATE\" style=\"gate-word\" }}; row style=\"gate-line\" {{ spacer; text \"{HOME_GATE}\" style=\"gate-id\" }} }}\n\
             \t}}\n",
            dest = q(&dest_line),
        ));

        if down {
            kdl.push_str(&format!(
                "\trow style=\"stale\" {{\n\
                 \t\ticon \"warning-fill\" style=\"stale-icon\" size=30\n\
                 \t\ttext \"Data feed lost — showing last known status\" style=\"stale-text\"\n\
                 \t\tspacer\n\
                 \t\ttext \"last update {} · {} ago\" style=\"stale-sub\"\n\
                 \t}}\n",
                clock12(st.last_update),
                ago_text(now - st.last_update)
            ));
        }
        if let Some(m) = moved {
            kdl.push_str(&format!(
                "\trow style=\"alert\" {{\n\
                 \t\ticon \"gate-fill\" style=\"alert-icon\" size=32\n\
                 \t\ttext \"GATE CHANGE\" style=\"alert-text\"\n\
                 \t\ttext {} style=\"alert-text\"\n\
                 \t\tspacer\n\
                 \t\ttext {} style=\"alert-text\"\n\
                 \t}}\n",
                q(&format!("{} to {} now departs from gate {}", m.code, m.destination, m.gate)),
                q(&hhmm12(m.expected()))
            ));
        }

        // The field: headline with its icon, subline, the tracker, the
        // route, the tiles; the standby card beside.
        kdl.push_str("\trow style=\"field\" {\n\t\tcolumn style=\"main\" {\n");
        match cur {
            None => {
                kdl.push_str(
                    "\t\t\trow style=\"head-row\" { icon \"gate-fill\" style=\"head-icon\" size=64; text \"Gate closed\" style=\"headline\" }\n\
                     \t\t\ttext \"No departures are scheduled from this gate\" style=\"subline\"\n",
                );
            }
            Some(f) => {
                let phase = phase.unwrap_or(Phase::Scheduled);
                let boards_at = f.expected() - tl.boarding_min * 60;
                let closes_at = f.expected() - tl.final_call_min * 60;
                let (icon, headline, subline) = match (phase, step) {
                    (Phase::Cancelled, _) => ("warning-fill", "Cancelled".to_string(), "Please see an agent for rebooking".to_string()),
                    (Phase::Departed, _) => ("takeoff-fill", "Departed".to_string(), format!("Left the gate at {}", hhmm12(f.expected()))),
                    (_, Some(0)) => ("people-fill", "Now boarding".to_string(), "Passengers needing assistance may board".to_string()),
                    (_, Some(g)) if g <= GROUPS => (
                        "people-fill",
                        "Now boarding".to_string(),
                        if g == 1 { "Group 1 may board".to_string() } else { format!("Groups 1–{g} may board") },
                    ),
                    (_, Some(_)) => ("warning-fill", "Final call".to_string(), "All remaining passengers may board".to_string()),
                    _ => {
                        let mins = (boards_at - shown).div_euclid(60).max(0);
                        let head = match mins {
                            0 => "Boarding shortly".to_string(),
                            1 => "Boards in 1 minute".to_string(),
                            n => format!("Boards in {n} minutes"),
                        };
                        let sub = if f.delay_min > 0 {
                            format!("Delayed {} minutes · boarding begins at {}", f.delay_min, hhmm12(boards_at))
                        } else {
                            format!("Boarding begins at {}", hhmm12(boards_at))
                        };
                        ("clock-fill", head, sub)
                    }
                };
                kdl.push_str(&format!(
                    "\t\t\trow style=\"head-row\" {{ icon \"{icon}\" style=\"head-icon\" size=64; text {} style=\"headline\" }}\n",
                    q(&headline)
                ));
                kdl.push_str(&format!("\t\t\trow style=\"indent\" {{ spacer 84; text {} style=\"subline\" }}\n", q(&subline)));

                // The tracker: one segment per boarding step, lit as it
                // goes. Shown from gate-open to departure.
                if !f.cancelled && phase != Phase::Departed {
                    kdl.push_str("\t\t\trow style=\"indent\" { spacer 84; row style=\"tracker\" {\n");
                    for g in 0..=GROUPS + 1 {
                        let state = match step {
                            Some(s) if s == g => "now",
                            Some(s) if s > g => "done",
                            _ => "wait",
                        };
                        let label = match g {
                            0 => "Pre-boarding".to_string(),
                            g if g <= GROUPS => format!("Group {g}"),
                            _ => format!("Final call · {}", hhmm12(closes_at)),
                        };
                        let (seg, w) = if g == GROUPS + 1 { ("seg-last", 272) } else { ("seg", 176) };
                        kdl.push_str(&format!(
                            "\t\t\t\tcolumn style=\"{seg}\" {{ rect style=\"seg-{state}\" width={w} height=12; text {} style=\"seg-{state}-t\" }}\n",
                            q(&label)
                        ));
                    }
                    kdl.push_str("\t\t\t} }\n");
                }
                kdl.push_str("\t\t\tspacer\n");

                // The route: the two codes, a dotted arc between them, the
                // block time under the arc. Each dot is a column whose top
                // spacer is the arc's height at that point — flow layout
                // drawing a curve. The arc is whole and blue throughout:
                // everything this screen shows happens before wheels-up.
                let minutes = flight_minutes(f.destination);
                const DOTS: usize = 64;
                const RISE: f32 = 64.0;
                let mut arc = String::from("\t\t\t\t\trow style=\"arc\" {\n");
                for i in 0..DOTS {
                    let u = 2.0 * (i as f32 / (DOTS - 1) as f32) - 1.0;
                    let y = RISE * (1.0 - u * u);
                    arc.push_str(&format!(
                        "\t\t\t\t\t\tcolumn style=\"arc-slot\" {{ spacer {}; rect style=\"arc-dot\" width=8 height=8 }}\n",
                        RISE - y
                    ));
                }
                arc.push_str("\t\t\t\t\t}\n");
                kdl.push_str(&format!(
                    "\t\t\trow style=\"indent\" {{ spacer 84; row style=\"route\" {{\n\
                     \t\t\t\ttext \"RIL\" style=\"iata\"\n\
                     \t\t\t\tspacer\n\
                     \t\t\t\tcolumn style=\"arc-col\" {{\n{arc}\
                     \t\t\t\t\ttext \"{}h {:02}m\" style=\"duration\"\n\
                     \t\t\t\t}}\n\
                     \t\t\t\tspacer\n\
                     \t\t\t\ttext \"{}\" style=\"iata\"\n\
                     \t\t\t}} }}\n",
                    minutes / 60,
                    minutes % 60,
                    iata(f.destination)
                ));
                kdl.push_str("\t\t\tspacer\n");

                // The times, under the route's two ends: departs under the
                // origin, arrives under the destination and right-aligned to
                // it. One rule spans them. Gate close lives on the tracker.
                let late = f.delay_min > 0 && phase != Phase::Departed;
                let dep_style = match phase {
                    Phase::Departed | Phase::Cancelled => "big-old",
                    _ if late => "big-late",
                    _ => "big",
                };
                let mut tiles = format!(
                    "\t\t\trow style=\"indent\" {{ spacer 84; column style=\"times\" {{\n\
                     \t\t\t\trect style=\"{}\" width=1232 height=2\n\
                     \t\t\t\trow style=\"tiles\" {{\n",
                    if late { "tile-rule-warn" } else { "tile-rule" }
                );
                let dep_label = if late { "NOW DEPARTS" } else { "DEPARTS" };
                tiles.push_str(&format!(
                    "\t\t\t\t\tcolumn style=\"tile\" {{ row style=\"tile-head\" {{ icon \"takeoff-fill\" style=\"tile-icon\" size=36; text \"{dep_label}\" style=\"lbl\" }}; text \"{}\" style=\"{dep_style}\"",
                    hhmm12(f.expected())
                ));
                if late {
                    tiles.push_str(&format!(
                        "; text {} style=\"note-warn\"",
                        q(&format!("Scheduled {} · +{} min", hhmm12(f.scheduled), f.delay_min))
                    ));
                }
                tiles.push_str(" }\n\t\t\t\t\tspacer\n");
                if !f.cancelled {
                    let arrives = f.expected() + minutes * 60;
                    tiles.push_str(&format!(
                        "\t\t\t\t\tcolumn style=\"tile-r\" {{ row style=\"tile-head\" {{ spacer; icon \"landing-fill\" style=\"tile-icon\" size=36; text \"ARRIVES\" style=\"lbl\" }}; row style=\"tile-head\" {{ spacer; text \"{}\" style=\"{dep_style}\" }} }}\n",
                        hhmm12(arrives)
                    ));
                }
                tiles.push_str("\t\t\t\t}\n\t\t\t} }\n");
                kdl.push_str(&tiles);
            }
        }
        kdl.push_str("\t\t}\n");

        // Standby: one column on the right. Names are stable per flight;
        // clearances arrive as boarding advances. A list longer than the
        // column cycles through it, one name further along every few
        // seconds, so nobody's name is ever off the screen for long.
        if let Some(f) = cur.filter(|f| !f.cancelled) {
            let phase = phase.unwrap_or(Phase::Scheduled);
            let h = code_hash(&f.code);
            let cleared = match step {
                None if phase == Phase::Departed => STANDBY,
                None => 0,
                Some(s) => (s * STANDBY / (GROUPS + 1)).min(STANDBY),
            };
            let closed = matches!(phase, Phase::Departed) || step == Some(GROUPS + 1);
            let start = (h % SURNAMES.len() as u64) as usize;
            let pages = STANDBY.div_ceil(STANDBY_ROWS).max(1);
            let offset = ((shown.div_euclid(STANDBY_CYCLE_SECS) as usize) % pages) * STANDBY_ROWS;
            let entry = |i: usize| -> String {
                let hi = code_hash(&format!("{}#{i}", f.code));
                let name = SURNAMES[(start + i * 5) % SURNAMES.len()];
                let initial = (b'A' + ((hi >> 8) % 26) as u8) as char;
                let (dot, name_style) = if i < cleared {
                    ("sb-ok", "sb-name")
                } else if closed {
                    ("sb-no", "sb-name-off")
                } else {
                    ("sb-wait", "sb-name-dim")
                };
                // Right-aligned: the name hugs the edge, the dot outside it.
                format!(
                    "\t\t\t\trow style=\"sb\" {{ spacer; text {} style=\"{name_style}\"; rect style=\"{dot}\" width=12 height=12 }}\n",
                    q(&format!("{}, {initial}", &name[..3.min(name.len())]))
                )
            };
            kdl.push_str(&format!(
                "\t\tcolumn style=\"card\" {{\n\
                 \t\t\trow style=\"card-head\" {{ spacer; icon \"ticket\" style=\"card-icon\" size=30; text \"Standby\" style=\"card-h\" }}\n\
                 \t\t\trow style=\"card-head\" {{ spacer; text \"{cleared} of {STANDBY} cleared\" style=\"count-t\" }}\n\
                 \t\t\trect style=\"card-rule\" width=400 height=2\n\
                 \t\t\tcolumn style=\"col\" {{\n"
            ));
            for i in offset..(offset + STANDBY_ROWS).min(STANDBY) {
                kdl.push_str(&entry(i));
            }
            kdl.push_str("\t\t\t}\n\t\t}\n");
        }
        kdl.push_str("\t}\n");

        // The footer: destination weather, what is next here, the clock.
        let (weather, weather_icon) = match cur {
            Some(f) if !f.cancelled => {
                let (cond, temp) = weather_at(f.destination, shown);
                (format!("{} · {cond} · {temp}°F", f.destination), weather_icon(cond))
            }
            _ => (String::new(), "cloud-fill"),
        };
        // The middle of the footer: the next flight here, when one is
        // queued; nothing at all — not even the glyph — when none is.
        let next_block = match next {
            Some(n) => format!(
                "icon \"plane\" style=\"foot-icon-dim\" size=26; text {} style=\"foot-next\"",
                q(&format!("Next · {} {} · {}", n.code, n.destination, hhmm12(n.expected())))
            ),
            None => "spacer 0".to_string(),
        };
        kdl.push_str(&format!(
            "\trow style=\"foot\" {{\n\
             \t\ticon \"{weather_icon}\" style=\"foot-icon\" size=28\n\
             \t\ttext {} style=\"foot-text\"\n\
             \t\tspacer\n\
             \t\t{next_block}\n\
             \t\tspacer\n\
             \t\ticon \"clock-fill\" style=\"foot-icon\" size=28\n\
             \t\ttext \"{}\" style=\"foot-clock\"\n\
             \t}}\n",
            q(&weather),
            clock12z(shown)
        ));
        kdl.push_str(&format!("\tlive target=\"/gate\" every={LIVE_MS}\n}}\n"));
        rill_appkit::compile_page("signage-app", &kdl)
    }

    /// The same facts without the drawing: one line per flight.
    fn data(&self, now: i64) -> String {
        let st = self.lock();
        let mut out = format!("# departures at {}\n", hhmmss(now));
        let mut shown: Vec<&Flight> = st.flights.iter().collect();
        shown.sort_by_key(|f| f.scheduled);
        for f in shown {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                hhmm(f.scheduled),
                f.code,
                f.destination,
                f.gate,
                Phase::of(f, now, &st.tl).label()
            ));
        }
        out
    }
}

/// One second of the world.
fn step(st: &mut State, now: i64) {
    let mut changed = false;
    // Departed flights linger, then fall off.
    let before = st.flights.len();
    st.flights.retain(|f| {
        let gone_at = if f.cancelled { f.scheduled } else { f.expected() };
        now < gone_at + st.tl.linger_min * 60
    });
    changed |= st.flights.len() != before;

    // Scripted: the home gate always has exactly one flight queued behind
    // the one boarding, exactly a turn later — the loop does not wait for
    // the rest of the queue to make room.
    if st.tl.scripted {
        let future_home = st
            .flights
            .iter()
            .any(|f| f.gate == HOME_GATE && !f.cancelled && f.expected() > now + st.tl.boarding_min * 60);
        if !future_home {
            let last_home = st.flights.iter().filter(|f| f.gate == HOME_GATE).map(|f| f.expected()).max();
            let at = match last_home {
                Some(h) => h + st.tl.home_turn_min * 60,
                None => now - now.rem_euclid(60) + (st.tl.boarding_min + 2) * 60,
            };
            let mut f = new_flight(st, at);
            f.gate = HOME_GATE.to_string();
            st.flights.push(f);
            changed = true;
        }
    }

    // Keep the queue full: new flights join at the far end.
    while st.flights.len() < ROWS + 2 {
        let last = st.flights.iter().map(|f| f.scheduled).max().unwrap_or(now - now.rem_euclid(60));
        let t = last + (2 + st.rng.below(6) as i64) * 60;
        let mut f = new_flight(st, t);
        let last_home = st
            .flights
            .iter()
            .filter(|f| f.gate == HOME_GATE && !f.cancelled)
            .map(|f| f.expected())
            .max();
        if !st.tl.scripted && last_home.is_none_or(|h| t - h >= st.tl.home_turn_min * 60) {
            f.gate = HOME_GATE.to_string();
        }
        st.flights.push(f);
        changed = true;
    }

    // News. Roughly one event a minute across the board — enough to
    // watch, not enough to look broken.
    if st.rng.chance(20) {
        let n = st.flights.len() as u64;
        let i = st.rng.below(n) as usize;
        let roll = st.rng.below(100);
        let tl = st.tl;
        let f = &mut st.flights[i];
        let phase = Phase::of(f, now, &tl);
        // The scripted home gate takes no news: its sequence is the point.
        if matches!(phase, Phase::Departed | Phase::Cancelled) || (tl.scripted && f.gate == HOME_GATE) {
            return;
        }
        if roll < 55 {
            // A delay, or more of one. Only before boarding starts.
            if !matches!(phase, Phase::Boarding | Phase::FinalCall) {
                f.delay_min += 5 + st.rng.below(6) as i64 * 5;
                changed = true;
            }
        } else if roll < 90 {
            // The home gate is sticky: one change in eight moves a flight
            // off it — the gate display's "gate change" moment — and the
            // gate is re-staffed at once by the earliest flight that has
            // not started boarding, moved *in* from wherever it was.
            let at_home = st.flights[i].gate == HOME_GATE;
            if !at_home || st.rng.chance(8) {
                let gate = gate_name(&mut st.rng);
                if st.flights[i].gate != gate {
                    let old = std::mem::replace(&mut st.flights[i].gate, gate);
                    st.flights[i].moved_from = Some(old);
                    changed = true;
                }
            }
            if at_home && changed {
                let incoming = st
                    .flights
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.gate != HOME_GATE && !f.cancelled && f.moved_from.is_none())
                    .filter(|(_, f)| f.expected() - now >= (tl.boarding_min + 5) * 60)
                    .min_by_key(|(_, f)| f.expected())
                    .map(|(j, _)| j);
                if let Some(j) = incoming {
                    let old = std::mem::replace(&mut st.flights[j].gate, HOME_GATE.to_string());
                    st.flights[j].moved_from = Some(old);
                }
            }
        } else if roll < 93 && !matches!(phase, Phase::FinalCall) {
            st.flights[i].cancelled = true;
            changed = true;
        }
    }
    if changed {
        st.changes += 1;
    }
}

fn new_flight(st: &mut State, scheduled: i64) -> Flight {
    let (code, airline) = AIRLINES[st.rng.below(AIRLINES.len() as u64) as usize];
    let destination = DESTINATIONS[st.rng.below(DESTINATIONS.len() as u64) as usize];
    let number = st.next_number;
    st.next_number = if number >= 999 { 100 } else { number + 1 + st.rng.below(7) as u32 };
    let gate = gate_name(&mut st.rng);
    Flight {
        code: format!("{code} {number}"),
        airline,
        destination,
        scheduled: scheduled - scheduled.rem_euclid(60),
        delay_min: 0,
        gate,
        cancelled: false,
        moved_from: None,
    }
}

fn gate_name(rng: &mut Rng) -> String {
    loop {
        let g = format!("B{}", 1 + rng.below(24));
        if g != HOME_GATE {
            return g;
        }
    }
}

/// A stable small hash of a flight code: the seed for its standby list.
fn code_hash(code: &str) -> u64 {
    code.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Where boarding stands for a flight at `now`: `None` before the gate
/// opens or after departure; otherwise the step index (0 = pre-boarding,
/// 1..=GROUPS = that group, GROUPS+1 = final call, all groups).
fn boarding_step(f: &Flight, now: i64, tl: &Timeline) -> Option<usize> {
    if f.cancelled {
        return None;
    }
    let minutes_left = (f.expected() - now).div_euclid(60);
    if !(0..tl.boarding_min).contains(&minutes_left) {
        return None;
    }
    if minutes_left < tl.final_call_min {
        return Some(GROUPS + 1);
    }
    Some((((tl.boarding_min - 1 - minutes_left) / tl.group_min) as usize).min(GROUPS))
}

/// Block time to each destination, minutes — what "Arrives" and "Flight
/// time" are built from. Round numbers; a schedule, not a flight plan.
fn flight_minutes(destination: &str) -> i64 {
    match destination {
        "Amsterdam" | "Berlin" | "Copenhagen" | "Oslo" | "Paris CDG" | "Prague" | "Stockholm"
        | "Vienna" | "Warsaw" | "Zürich" | "Rome" | "Madrid" | "Lisbon" | "Dublin"
        | "London Heathrow" | "Helsinki" => 620 + (destination.len() as i64 % 5) * 15,
        "Reykjavík" => 470,
        "Montréal" | "Toronto" => 305,
        "Chicago" | "Denver" => 245,
        "Seattle" | "Vancouver" | "San Francisco" => 130,
        "Tokyo Haneda" => 650,
        _ => 180,
    }
}

/// The airport code for a destination — the three letters a traveller
/// reads off a tag.
fn iata(destination: &str) -> &'static str {
    match destination {
        "Amsterdam" => "AMS", "Berlin" => "BER", "Chicago" => "ORD", "Copenhagen" => "CPH",
        "Denver" => "DEN", "Dublin" => "DUB", "Helsinki" => "HEL", "Lisbon" => "LIS",
        "London Heathrow" => "LHR", "Madrid" => "MAD", "Montréal" => "YUL", "Oslo" => "OSL",
        "Paris CDG" => "CDG", "Prague" => "PRG", "Reykjavík" => "KEF", "Rome" => "FCO",
        "San Francisco" => "SFO", "Seattle" => "SEA", "Stockholm" => "ARN", "Tokyo Haneda" => "HND",
        "Toronto" => "YYZ", "Vancouver" => "YVR", "Vienna" => "VIE", "Warsaw" => "WAW",
        "Zürich" => "ZRH",
        _ => "---",
    }
}

/// Destination weather: a plausible, stable reading that drifts by the
/// hour. Simulated, like everything else on the board.
fn weather_at(destination: &str, now: i64) -> (&'static str, i64) {
    const CONDITIONS: &[&str] = &["Sunny", "Partly cloudy", "Cloudy", "Light rain", "Clear", "Overcast"];
    let h = code_hash(destination) ^ (now.div_euclid(3600) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let cond = CONDITIONS[(h % CONDITIONS.len() as u64) as usize];
    let base = 46 + (code_hash(destination) % 30) as i64;
    let drift = ((h >> 20) % 7) as i64 - 3;
    (cond, base + drift)
}

/// The glyph for a weather condition.
fn weather_icon(condition: &str) -> &'static str {
    match condition {
        "Sunny" | "Clear" => "sun-fill",
        "Partly cloudy" => "cloud-sun-fill",
        "Light rain" => "cloud-rain-fill",
        _ => "cloud-fill",
    }
}

/// Hours, minutes, seconds and the zone's short name, in the process's
/// local time.
fn local_parts(t: i64) -> (i64, i64, i64, String) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let secs: libc::time_t = t as libc::time_t;
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        let day = t.rem_euclid(86_400);
        return (day / 3600, (day % 3600) / 60, day % 60, "UTC".into());
    }
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned()
    };
    (tm.tm_hour as i64, tm.tm_min as i64, tm.tm_sec as i64, zone)
}

/// "11:36 AM" — the twelve-hour clock a gate shows.
fn hhmm12(t: i64) -> String {
    let (h, m, _, _) = local_parts(t);
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{h12}:{m:02} {ap}")
}

/// "11:36 AM PDT".
fn clock12z(t: i64) -> String {
    let (_, _, _, zone) = local_parts(t);
    format!("{} {zone}", hhmm12(t)).trim_end().to_string()
}

/// "11:36:07 AM".
fn clock12(t: i64) -> String {
    let (h, m, s, _) = local_parts(t);
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{h12}:{m:02}:{s:02} {ap}")
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn local_hms(t: i64) -> (i64, i64, i64) {
    let (h, m, s, _) = local_parts(t);
    (h, m, s)
}

fn hhmm(t: i64) -> String {
    let (h, m, _) = local_hms(t);
    format!("{h:02}:{m:02}")
}

fn hhmmss(t: i64) -> String {
    let (h, m, s) = local_hms(t);
    format!("{h:02}:{m:02}:{s:02}")
}

fn ago_text(secs: i64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{} min {:02} s", secs / 60, secs % 60)
    }
}

impl AppHandler for Board {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        let now = unix_now();
        match path {
            "/airport" | "/airport/" => {
                self.tick(now);
                self.page(now).ok()
            }
            "/airport/data" => {
                self.tick(now);
                Some(self.data(now).into_bytes())
            }
            "/gate" | "/gate/" => {
                self.tick(now);
                self.gate_page(now).ok()
            }
            _ => None,
        }
    }

    fn revision(&self, path: &str, _identity: &Identity) -> Option<u64> {
        match path {
            "/airport" | "/airport/" => {
                let now = unix_now();
                self.tick(now);
                Some(self.stamp(now))
            }
            "/gate" | "/gate/" => {
                let now = unix_now();
                self.tick(now);
                Some(self.gate_stamp(now))
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One directory per board: the feed switch is a file, and tests run
    /// in parallel.
    fn board() -> Board {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "signage-airport-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Board::new(dir, 7, Timeline::REAL)
    }

    #[test]
    fn the_board_compiles_and_is_live() {
        let b = board();
        let now = unix_now();
        let bytes = b.page(now).expect("compiles");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let live = doc.nodes.iter().find_map(|n| match n {
            rill_doc::Node::Live { target, interval } => {
                Some((doc.strings[*target as usize].clone(), *interval))
            }
            _ => None,
        });
        assert_eq!(live, Some(("/airport".to_string(), LIVE_MS)));
    }

    #[test]
    fn the_revision_holds_within_a_minute_and_moves_with_the_schedule() {
        let b = board();
        let t = 1_800_000_000 - 1_800_000_000 % 60 + 5;
        let a = b.stamp(t);
        assert_eq!(a, b.stamp(t + 30), "same minute, same flights, same stamp");
        assert_ne!(a, b.stamp(t + 60), "the minute hand is part of the board");
    }

    #[test]
    fn flights_walk_through_their_phases_and_fall_off() {
        let b = board();
        let (first, sched) = {
            let st = b.lock();
            let f = st.flights.iter().min_by_key(|f| f.scheduled).unwrap();
            (f.code.clone(), f.scheduled)
        };
        let at = |b: &Board, t: i64| {
            let st = b.lock();
            st.flights.iter().find(|f| f.code == first).map(|f| Phase::of(f, t, &Timeline::REAL))
        };
        assert!(matches!(at(&b, sched - 40 * 60), Some(Phase::Scheduled | Phase::Delayed)));
        assert!(matches!(at(&b, sched - 20 * 60), Some(Phase::Boarding | Phase::Delayed | Phase::Scheduled)));
        // Run the clock past the linger window and the flight is gone
        // (unless a delay pushed it — then it is at least not departed).
        let mut st = b.lock();
        for t in (sched + 1)..(sched + 90 * 60) {
            step(&mut st, t);
        }
        let now = sched + 90 * 60;
        let still = st.flights.iter().find(|f| f.code == first);
        assert!(still.is_none_or(|f| Phase::of(f, now, &Timeline::REAL) != Phase::Departed || now < f.expected() + Timeline::REAL.linger_min * 60));
        assert!(st.flights.len() >= ROWS, "the queue is kept full");
    }

    #[test]
    fn the_gate_compiles_through_a_whole_turn_and_stays_staffed() {
        let b = board();
        let start = unix_now();
        // Walk an hour and a half in 30 s steps: every rendered moment
        // compiles, and the home gate never runs out of flights.
        let mut t = start;
        while t < start + 90 * 60 {
            {
                let mut st = b.lock();
                for s in (t - 30)..t {
                    step(&mut st, s);
                }
                let (cur, next, _) = Board::gate_flights(&st);
                assert!(cur.is_some() || next.is_some(), "the home gate has a flight queued at +{}s", t - start);
            }
            let bytes = b.gate_page(t).expect("gate compiles");
            let doc = rill_doc::decode(&bytes).expect("decodes");
            assert!(doc.strings.iter().any(|s| s == HOME_GATE));
            t += 30;
        }
    }

    #[test]
    fn boarding_walks_pre_then_groups_then_final_call() {
        let f = Flight {
            code: "RL 1".into(),
            airline: "Rill Air",
            destination: "Oslo",
            scheduled: 10_000 * 60,
            delay_min: 0,
            gate: HOME_GATE.into(),
            cancelled: false,
            moved_from: None,
        };
        let at = |min_left: i64| boarding_step(&f, f.scheduled - min_left * 60, &Timeline::REAL);
        assert_eq!(at(31), None);
        assert_eq!(at(29), Some(0));
        assert_eq!(at(25), Some(1));
        assert_eq!(at(13), Some(4));
        assert_eq!(at(9), Some(GROUPS + 1));
        assert_eq!(at(-1), None);
    }

    /// The demo timeline: one state a minute, and the home gate turns
    /// every eight, forever. Walk half an hour and read the state each
    /// minute.
    #[test]
    fn the_demo_loop_changes_state_every_minute_and_repeats() {
        let dir = std::env::temp_dir().join(format!("signage-demo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let b = Board::new(dir, 7, Timeline::DEMO);
        let start = unix_now();
        let start = start - start.rem_euclid(60) + 30; // mid-minute, like a viewer
        let mut states = Vec::new();
        for m in 0..30 {
            let t = start + m * 60;
            {
                let mut st = b.lock();
                for s in (t - 60)..t {
                    step(&mut st, s);
                }
            }
            let st = b.lock();
            let (cur, _, _) = Board::gate_flights(&st);
            let f = cur.unwrap_or_else(|| {
                let homes: Vec<String> = st.flights.iter().map(|f| format!("{} {} {}", f.code, f.gate, (f.expected() - t) / 60)).collect();
                panic!("the home gate always has a flight: minute {m}, states {states:?}, flights {homes:?}")
            });
            let label = match (Phase::of(f, t, &st.tl), boarding_step(f, t, &st.tl)) {
                (Phase::Departed, _) => "departed".to_string(),
                (_, Some(s)) => format!("step{s}"),
                (p, None) => format!("{}-{}", p.label(), (f.expected() - st.tl.boarding_min * 60 - t).div_euclid(60) + 1),
            };
            states.push(label);
        }
        // Every minute is a new state...
        for w in states.windows(2) {
            assert_ne!(w[0], w[1], "no minute repeats its neighbour: {states:?}");
        }
        // ...and the sequence comes back around after one turn.
        let turn = Timeline::DEMO.home_turn_min as usize;
        assert_eq!(states[2..turn + 2], states[turn + 2..2 * turn + 2], "loops every {turn} min: {states:?}");
        assert!(states.iter().any(|s| s == "step5"), "final call is in the loop: {states:?}");
        assert!(states.iter().any(|s| s == "departed"), "departed is in the loop: {states:?}");
    }

    #[test]
    fn a_dropped_feed_freezes_the_simulation_and_says_so() {
        let b = board();
        std::fs::write(&b.feed_flag, "").unwrap();
        let before = b.lock().flights.clone();
        b.tick(unix_now() + 3600);
        assert_eq!(before, b.lock().flights, "no ticks while down");
        let bytes = b.page(unix_now() + 90).unwrap();
        let doc = rill_doc::decode(&bytes).unwrap();
        assert!(doc.strings.iter().any(|s| s.contains("DATA FEED LOST")));
        std::fs::remove_file(&b.feed_flag).unwrap();
        let bytes = b.page(unix_now()).unwrap();
        let doc = rill_doc::decode(&bytes).unwrap();
        assert!(!doc.strings.iter().any(|s| s.contains("DATA FEED LOST")));
    }
}
