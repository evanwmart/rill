//! The store-window ad: a Rill Air campaign, one slide at a time.
//!
//! Each slide is a flat composition in the gate display's own system —
//! the palette, Inter, the mark, the dotted arc — so the window and the
//! gate read as one airline. The playlist plays on the server's clock:
//! slide *n* is whichever one `now / DWELL` lands on, so every glass
//! pointed at the same server shows the same slide at the same moment
//! with no coordination. The page carries `live every=1000`; `revision`
//! is the slide index, so the poll is answered NOT_MODIFIED until the
//! slide actually turns, and a wall of forty windows costs forty tiny
//! frames a second, not forty renders.
//!
//! No video, no per-frame animation: a slide is a document, and the
//! change between slides is instant. Motion, when the glass can afford
//! it, is the compositor's to add over these frames — not the page's.

use std::time::{SystemTime, UNIX_EPOCH};

use rill_auth::Identity;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

const LIVE_MS: u16 = 1000;

/// Seconds each slide holds. Ten is the store-window convention: long
/// enough to read, short enough that a passer-by sees two.
const DWELL: i64 = 10;

/// The face, shared with the gate.
const FACE: &str = "Inter";

/// A slide's ground: the campaign alternates the gate's navy with a light
/// slide and one full-colour slide, so the window has a rhythm from the
/// far side of the mall.
#[derive(Clone, Copy)]
enum Ground {
    Navy,
    Smoke,
    Sky,
}

/// The graphic on a slide's right-hand side.
#[derive(Clone, Copy)]
enum Art {
    /// The mark, large, in its own colours.
    Mark,
    /// The dotted arc between two codes with the block time under it.
    Route(&'static str, &'static str, i64),
    /// The boarding tracker, every step lit: the on-time story.
    Tracker,
    /// The side-on aircraft, large.
    Plane,
}

struct Slide {
    ground: Ground,
    kicker: &'static str,
    headline: &'static [&'static str],
    copy: &'static str,
    cta: &'static str,
    art: Art,
}

const PLAYLIST: &[Slide] = &[
    Slide {
        ground: Ground::Navy,
        kicker: "RILL AIR",
        headline: &["The short way", "to far away"],
        copy: "Nonstop from Rill International to twenty-five cities across North America, Europe and Japan.",
        cta: "rillair.example · Book by 30 September",
        art: Art::Mark,
    },
    Slide {
        ground: Ground::Smoke,
        kicker: "SEATTLE FROM",
        headline: &["$129"],
        copy: "One way, taxes included. Twelve departures a day, most of them on time.",
        cta: "Fares this low do not last",
        art: Art::Route("RIL", "SEA", 130),
    },
    Slide {
        ground: Ground::Navy,
        kicker: "NEW ROUTE",
        headline: &["Tokyo", "Haneda"],
        copy: "Daily nonstop from 1 October. Lie-flat seats up front, real coffee all the way back.",
        cta: "Opening fares from $649",
        art: Art::Route("RIL", "HND", 650),
    },
    Slide {
        ground: Ground::Sky,
        kicker: "RILLMILES",
        headline: &["Every seat", "earns"],
        copy: "Points on every fare, no blackout dates, and family pooling built in.",
        cta: "Join free in the app",
        art: Art::Plane,
    },
    Slide {
        ground: Ground::Navy,
        kicker: "ON TIME",
        headline: &["Boards on time.", "Lands on time."],
        copy: "Every gate at Rill International runs on one live board, updated the moment anything changes.",
        cta: "94% on time this year",
        art: Art::Tracker,
    },
];

pub struct Ad;

impl Ad {
    fn index(now: i64) -> usize {
        (now.div_euclid(DWELL) as usize) % PLAYLIST.len()
    }

    fn page(&self, now: i64) -> Result<Vec<u8>, Status> {
        let i = Self::index(now);
        let s = &PLAYLIST[i];
        let q = rill_doc::kdl_escape;
        // The palette per ground: ink, muted ink, the accent, the floor.
        // Navy slides are 80% so the glass's watermark ghosts through,
        // like the gate; the light and the sky slides are solid.
        let (ground, ink, dim, accent, dot_off) = match s.ground {
            Ground::Navy => ("#22333BCC", "#F2F4F3", "#F2F4F3A6", "#00A7E1", "#F2F4F340"),
            Ground::Smoke => ("#F2F4F3", "#0A0908", "#22333BB3", "#00A7E1", "#22333B33"),
            Ground::Sky => ("#00A7E1", "#F2F4F3", "#F2F4F3CC", "#0A0908", "#F2F4F359"),
        };
        let mut kdl = format!(
            "style \"slide\" padding-x=96 padding-y=80 gap=0 width=\"fill\" height=\"fill\" background=\"{ground}\"
style \"body\" gap=64 valign=\"center\" width=\"fill\" height=\"fill\" padding=0
style \"words\" gap=24 width=1000 padding=0
style \"lines\" gap=0 padding=0
style \"kicker\" size=32 weight=600 color=\"{accent}\" font=\"{FACE}\"
style \"headline\" size=136 weight=700 color=\"{ink}\" font=\"{FACE}\" wrap=#false
style \"copy\" size=36 weight=400 color=\"{dim}\" font=\"{FACE}\" wrap=#true
style \"cta-row\" gap=16 valign=\"center\" padding=0 padding-y=16
style \"cta-bar\" background=\"{accent}\" corner=3
style \"cta\" size=32 weight=600 color=\"{ink}\" font=\"{FACE}\" wrap=#false
style \"art\" gap=0 width=640 align=\"center\" padding=0
style \"mark\" color=\"{ink}\"
style \"plane\" color=\"{ink}\"
style \"route\" gap=0 valign=\"center\" padding=0 width=640
style \"iata\" size=48 weight=500 color=\"{ink}\" font=\"mono\"
style \"arc\" gap=0 valign=\"top\" padding=0
style \"arc-slot\" gap=0 width=14 align=\"center\" padding=0
style \"arc-dot\" background=\"{accent}\" corner=4
style \"arc-col\" gap=6 width=420 align=\"center\" padding=0
style \"duration\" size=24 weight=500 color=\"{dim}\" font=\"{FACE}\" align=\"center\" width=\"fill\"
style \"tracker\" gap=12 padding=0
style \"seg\" gap=12 width=96 padding=0
style \"seg-on\" background=\"{accent}\" corner=4
style \"seg-t\" size=18 weight=500 color=\"{dim}\" font=\"{FACE}\" wrap=#false
style \"foot\" gap=16 valign=\"center\" padding=0 width=\"fill\"
style \"brand\" size=26 weight=600 color=\"{ink}\" font=\"{FACE}\"
style \"brand-dim\" size=26 weight=400 color=\"{dim}\" font=\"{FACE}\"
style \"dots\" gap=10 valign=\"center\" padding=0 width=120
style \"dot\" background=\"{accent}\" corner=5
style \"dot-off\" background=\"{dot_off}\" corner=5

column style=\"slide\" {{
\tpage background=\"#00000000\"
\trow style=\"body\" {{
\t\tcolumn style=\"words\" {{
"
        );
        kdl.push_str(&format!("\t\t\ttext {} style=\"kicker\"\n", q(s.kicker)));
        kdl.push_str("\t\t\tcolumn style=\"lines\" {\n");
        for line in s.headline {
            kdl.push_str(&format!("\t\t\t\ttext {} style=\"headline\"\n", q(line)));
        }
        kdl.push_str("\t\t\t}\n");
        kdl.push_str(&format!("\t\t\ttext {} style=\"copy\"\n", q(s.copy)));
        kdl.push_str(&format!(
            "\t\t\trow style=\"cta-row\" {{ rect style=\"cta-bar\" width=48 height=6; text {} style=\"cta\" }}\n",
            q(s.cta)
        ));
        kdl.push_str("\t\t}\n\t\tspacer\n\t\tcolumn style=\"art\" {\n");
        match s.art {
            Art::Mark => kdl.push_str("\t\t\ticon \"rill-air\" style=\"mark\" size=560\n"),
            Art::Plane => kdl.push_str("\t\t\ticon \"plane-side\" style=\"plane\" size=600\n"),
            Art::Route(from, to, minutes) => {
                const DOTS: usize = 30;
                const RISE: f32 = 72.0;
                let mut arc = String::from("\t\t\t\t\trow style=\"arc\" {\n");
                for k in 0..DOTS {
                    let u = 2.0 * (k as f32 / (DOTS - 1) as f32) - 1.0;
                    let y = RISE * (1.0 - u * u);
                    arc.push_str(&format!(
                        "\t\t\t\t\t\tcolumn style=\"arc-slot\" {{ spacer {}; rect style=\"arc-dot\" width=8 height=8 }}\n",
                        RISE - y
                    ));
                }
                arc.push_str("\t\t\t\t\t}\n");
                kdl.push_str(&format!(
                    "\t\t\trow style=\"route\" {{\n\
                     \t\t\t\ttext \"{from}\" style=\"iata\"\n\
                     \t\t\t\tspacer\n\
                     \t\t\t\tcolumn style=\"arc-col\" {{\n{arc}\
                     \t\t\t\t\ttext \"{}h {:02}m\" style=\"duration\"\n\
                     \t\t\t\t}}\n\
                     \t\t\t\tspacer\n\
                     \t\t\t\ttext \"{to}\" style=\"iata\"\n\
                     \t\t\t}}\n",
                    minutes / 60,
                    minutes % 60,
                ));
            }
            Art::Tracker => {
                kdl.push_str("\t\t\trow style=\"tracker\" {\n");
                for label in ["Pre-boarding", "Group 1", "Group 2", "Group 3", "Group 4", "Final call"] {
                    kdl.push_str(&format!(
                        "\t\t\t\tcolumn style=\"seg\" {{ rect style=\"seg-on\" width=96 height=10; text \"{label}\" style=\"seg-t\" }}\n"
                    ));
                }
                kdl.push_str("\t\t\t}\n");
            }
        }
        kdl.push_str("\t\t}\n\t}\n");

        // The foot: brand left, the slide dots right.
        kdl.push_str("\trow style=\"foot\" {\n\t\ttext \"RILL AIR\" style=\"brand\"\n\t\ttext \"rillair.example\" style=\"brand-dim\"\n\t\tspacer\n\t\trow style=\"dots\" {\n");
        for j in 0..PLAYLIST.len() {
            let (w, style) = if j == i { (40, "dot") } else { (10, "dot-off") };
            kdl.push_str(&format!("\t\t\trect style=\"{style}\" width={w} height=10\n"));
        }
        kdl.push_str("\t\t}\n\t}\n");
        kdl.push_str(&format!("\tlive target=\"/ad\" every={LIVE_MS}\n}}\n"));
        rill_appkit::compile_page("signage-app", &kdl)
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl AppHandler for Ad {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        match path {
            "/ad" | "/ad/" => self.page(unix_now()).ok(),
            _ => None,
        }
    }

    fn revision(&self, path: &str, _identity: &Identity) -> Option<u64> {
        match path {
            // The slide index, offset so it never reads as "zero, nothing".
            "/ad" | "/ad/" => Some(1 + Self::index(unix_now()) as u64),
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

    #[test]
    fn every_slide_compiles() {
        for (i, slide) in PLAYLIST.iter().enumerate() {
            let now = (i as i64) * DWELL;
            let bytes = Ad.page(now).unwrap_or_else(|e| panic!("slide {i}: {e:?}"));
            let doc = rill_doc::decode(&bytes).expect("decodes");
            assert!(doc.strings.iter().any(|s| s == slide.headline[0]));
        }
    }

    #[test]
    fn the_rotation_is_a_function_of_the_clock() {
        assert_eq!(Ad::index(0), 0);
        assert_eq!(Ad::index(DWELL - 1), 0);
        assert_eq!(Ad::index(DWELL), 1);
        assert_eq!(Ad::index(DWELL * PLAYLIST.len() as i64), 0);
    }
}
