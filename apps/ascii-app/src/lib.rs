//! An ASCII art widget: something to look at in the corner of a desktop.
//!
//! ```toml
//! [desktop.ascii]
//! art = "cube"        # cube | wave | plasma, or a directory of .txt frames
//! seconds = 0.08      # per frame
//! color = "accent"    # a theme token or #rrggbb
//! align = "center"    # left | center | right
//!
//! [[desktop.widgets]]
//! app = "rill://127.0.0.1:7420/ascii"
//! anchor = "bottom-left"
//! width = 380
//! height = 200
//! ```
//!
//! The frame is a pure function of the wall clock, so nothing here holds
//! animation state: two clients looking at the same widget see the same
//! frame, and a client that misses a tick simply arrives later in the
//! animation rather than out of step with it. The page carries `live`, and
//! the size arrives in the address the client substitutes — the same two
//! mechanisms the terminal uses, doing the same two jobs.

use std::path::{Path, PathBuf};

use rill_appkit::Metrics;
use rill_auth::Identity;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

mod art;
mod gif;

/// The bundled mono cut advances 632/1000 of an em and a line box is 1.4×
/// the type size — the same arithmetic the terminal does, for the same
/// reason: the server has to know how many characters fit.
pub(crate) const MONO_ADVANCE: f32 = 0.632;
pub(crate) const LINE_FACTOR: f32 = 1.4;

#[derive(Clone, PartialEq, Debug)]
enum Art {
    Cube,
    Wave,
    Plasma,
    /// A directory of `.txt` frames, shown in name order.
    Frames(PathBuf),
    /// A `.gif`, decoded once and played at its own timing.
    Gif(PathBuf),
}

#[derive(Clone, PartialEq, Debug)]
struct Config {
    art: Art,
    /// Seconds per frame. Generators want a smooth tick; a folder of
    /// drawings wants a slow one, so the default depends on which it is.
    seconds: f32,
    color: String,
    align: String,
}

impl Config {
    fn load(theme: &Path) -> Config {
        let table = std::fs::read_to_string(theme)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get("desktop")?.get("ascii")?.as_table().cloned())
            .unwrap_or_default();

        let named = table.get("art").and_then(|v| v.as_str()).unwrap_or("cube").to_string();
        let art = match named.as_str() {
            "cube" => Art::Cube,
            "wave" => Art::Wave,
            "plasma" => Art::Plasma,
            // Anything else is a path: a `.gif` to play, or a folder of
            // frames someone drew.
            other => {
                let path = expand_home(other);
                match path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gif")) {
                    true => Art::Gif(path),
                    false => Art::Frames(path),
                }
            }
        };
        let default_seconds = if matches!(art, Art::Frames(_)) { 1.5 } else { 0.08 };
        Config {
            seconds: table
                .get("seconds")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .map(|n| (n as f32).clamp(0.03, 60.0))
                .unwrap_or(default_seconds),
            color: table
                .get("color")
                .and_then(|v| v.as_str())
                .unwrap_or("accent")
                .to_string(),
            align: match table.get("align").and_then(|v| v.as_str()) {
                Some("left") => "left".into(),
                Some("right") => "right".into(),
                _ => "center".into(),
            },
            art,
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => PathBuf::from(path),
    }
}

/// The `.txt` frames in a directory, in name order — so `01.txt`, `02.txt`
/// is an animation and nobody has to write a manifest.
fn read_frames(dir: &Path) -> Vec<Vec<String>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|text| text.lines().map(|l| l.to_string()).collect())
        .collect()
}

/// Seconds since an arbitrary fixed point. Wall clock rather than uptime so
/// every client agrees, which is what makes the frame a pure function of
/// the time rather than of who asked.
fn now_seconds() -> f32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() % 86_400_000) as f32 / 1000.0)
        .unwrap_or(0.0)
}

pub struct Ascii {
    theme: PathBuf,
    /// Decoded GIFs and the grids rendered from them. Held on the app, not
    /// rebuilt per request — that is the whole difference between this and
    /// piping an image converter into a terminal.
    gifs: gif::Cache,
}

impl Ascii {
    pub fn new(theme: PathBuf) -> Ascii {
        Ascii { theme, gifs: gif::Cache::default() }
    }

    /// The lines to draw, for a grid this size at this moment.
    fn frame(&self, config: &Config, cols: usize, rows: usize, now: f32) -> Vec<String> {
        let t = now / config.seconds.max(0.03);
        match &config.art {
            Art::Cube => art::cube(cols, rows, t * 0.08).lines(),
            Art::Wave => art::wave(cols, rows, t * 0.08).lines(),
            Art::Plasma => art::plasma(cols, rows, t * 0.08).lines(),
            Art::Frames(dir) => {
                let frames = read_frames(dir);
                if frames.is_empty() {
                    return vec![format!("no .txt frames in {}", dir.display())];
                }
                frames[(t as usize) % frames.len()].clone()
            }
            // The GIF keeps its own timing, so `seconds` is not applied: a
            // loop should run at the speed it was made at. Everything
            // expensive happened on the first frame; this is a lookup.
            Art::Gif(path) => match self.gifs.frame(path, cols, rows, now) {
                Ok(lines) => lines,
                Err(message) => vec![message],
            },
        }
    }

    fn page(&self, width: f32, height: f32) -> Result<Vec<u8>, Status> {
        let m = Metrics::from_theme_file(&self.theme);
        let config = Config::load(&self.theme);

        let cell_w = (m.font_size * MONO_ADVANCE).max(1.0);
        let cell_h = (m.font_size * LINE_FACTOR).max(1.0);
        let cols = (((width - 2.0 * m.padding) / cell_w).floor() as usize).clamp(4, 400);
        let rows = (((height - 2.0 * m.padding) / cell_h).floor() as usize).clamp(2, 200);
        let lines = self.frame(&config, cols, rows, now_seconds());

        // A token or a literal — the style system takes either, so the
        // colour is passed through as written rather than resolved here.
        let color = &config.color;
        let mut kdl = format!(
            "style \"art\" padding={p} gap=0 height=\"fill\"\n\
             style \"line\" color=\"{color}\" size={f} font=\"mono\" weight={weight} align=\"{align}\"\n\n\
             column style=\"art\" {{\n",
            p = m.padding,
            f = m.font_size,
            align = config.align,
            weight = m.mono_weight,
        );
        for line in lines {
            // A blank line still has to occupy one: an empty text node
            // measures to nothing and the art would concertina.
            let text = if line.trim().is_empty() { " ".to_string() } else { line };
            kdl.push_str(&format!(
                "\trow gap=0 padding=0 {{ text {} style=\"line\" }}\n",
                rill_doc::kdl_escape(&text)
            ));
        }
        // The clock, and the size: the client substitutes {w}/{h} with the
        // area it laid this page into, so the grid fits whatever the widget
        // was given rather than a number guessed here.
        kdl.push_str(&format!(
            "\tlive target=\"/ascii/fit/{{w}}x{{h}}\" every={ms}\n",
            ms = (config.seconds * 1000.0).round().clamp(16.0, 60_000.0) as u16
        ));
        kdl.push_str("}\n");

        rill_appkit::compile_page("ascii-app", &kdl)
    }
}

/// `WIDTHxHEIGHT` in pixels, as the client substituted it.
fn parse_fit(segment: &str) -> Option<(f32, f32)> {
    let (w, h) = segment.split_once('x')?;
    Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
}

impl AppHandler for Ascii {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        match path {
            "/ascii" | "/ascii/" => self.page(380.0, 200.0).ok(),
            p => {
                let fit = p.strip_prefix("/ascii/fit/")?;
                // Placeholders left unsubstituted are not a size; draw at the
                // default rather than refusing to draw.
                let (w, h) = parse_fit(fit).unwrap_or((380.0, 200.0));
                self.page(w, h).ok()
            }
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

    fn with_theme(body: &str, name: &str) -> (PathBuf, Ascii) {
        let dir = std::env::temp_dir().join(format!("ascii-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let theme = dir.join("theme.toml");
        std::fs::write(&theme, body).unwrap();
        (dir, Ascii::new(theme))
    }

    /// The defaults are what someone gets for writing nothing at all.
    /// A GIF plays as ASCII, keeps its own timing, and — the point of the
    /// whole thing — decodes once. A widget ticks several times a second
    /// forever; if each tick re-decoded the file it would be exactly the
    /// terminal-plus-image-converter problem this exists to avoid.
    #[test]
    fn a_gif_plays_as_ascii_and_is_only_decoded_once() {
        let dir = std::env::temp_dir().join(format!("rill-gif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blink.gif");
        // Two 2x2 frames, 100ms each: one black, one white.
        std::fs::write(&path, tiny_gif()).unwrap();

        let cache = gif::Cache::default();
        let dark = cache.frame(&path, 8, 4, 0.00).expect("frame 0");
        let light = cache.frame(&path, 8, 4, 0.15).expect("frame 1");
        assert_eq!(dark.len(), 4, "as many lines as rows asked for");
        assert!(dark.iter().all(|l| l.chars().count() == 8), "every line is cols wide");
        assert_ne!(dark, light, "the two frames must not render the same");
        // Darkest maps to the low end of the ramp, brightest to the high end.
        assert!(dark.iter().any(|l| l.contains(' ')));
        assert!(light.iter().any(|l| l.contains('@')));

        // It loops, and on its own clock: one full period later is frame 0
        // again, without anyone resetting anything.
        assert_eq!(cache.frame(&path, 8, 4, 0.20).unwrap(), dark, "loops after 200ms");

        // Deleting the file mid-loop proves nothing is re-read per frame:
        // a cached decode keeps playing, which is also the behaviour you
        // want when a file is being rewritten underneath a live widget.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cache.frame(&path, 8, 4, 0.00).unwrap(), dark, "served from cache");

        // A file that never existed says so rather than blanking.
        assert!(cache.frame(&dir.join("nope.gif"), 8, 4, 0.0).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The smallest real GIF89a that animates: 2x2, two frames, 100ms each,
    /// a two-colour global table (black, white). Written by hand so the test
    /// needs no encoder and no fixture file.
    fn tiny_gif() -> Vec<u8> {
        let mut g = Vec::new();
        g.extend(b"GIF89a");
        g.extend([2, 0, 2, 0]);           // 2x2
        g.extend([0x80, 0, 0]);           // global table, 2 entries
        g.extend([0, 0, 0, 255, 255, 255]); // black, white
        g.extend(b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00"); // loop forever
        for index in [0u8, 1] {
            // Graphic control: 100ms delay.
            g.extend([0x21, 0xf9, 0x04, 0x00, 10, 0, 0, 0]);
            g.extend([0x2c, 0, 0, 0, 0, 2, 0, 2, 0, 0]); // image at 0,0 2x2
            // LZW at a 2-bit minimum code size, so codes are 3 bits wide.
            // A CLEAR before every pixel keeps the dictionary from ever
            // growing to 8 entries, which is where the code width would
            // step up to 4 bits — legal, and much easier to be sure of than
            // hand-rolling the width transitions.
            g.push(2);
            let (clear, end) = (4u16, 5u16);
            let mut bits: Vec<u8> = Vec::new();
            let mut acc: u32 = 0;
            let mut nbits: u32 = 0;
            let push = |code: u16, acc: &mut u32, nbits: &mut u32, out: &mut Vec<u8>| {
                *acc |= (code as u32) << *nbits;
                *nbits += 3;
                while *nbits >= 8 {
                    out.push((*acc & 0xff) as u8);
                    *acc >>= 8;
                    *nbits -= 8;
                }
            };
            for _ in 0..4 {
                push(clear, &mut acc, &mut nbits, &mut bits);
                push(index as u16, &mut acc, &mut nbits, &mut bits);
            }
            push(end, &mut acc, &mut nbits, &mut bits);
            if nbits > 0 {
                bits.push((acc & 0xff) as u8);
            }
            g.push(bits.len() as u8);
            g.extend(&bits);
            g.push(0); // block terminator
        }
        g.push(0x3b); // trailer
        g
    }

    #[test]
    fn an_unconfigured_widget_still_draws() {
        let (dir, a) = with_theme("[colors]\ntext = \"#ffffff\"\n", "bare");
        let config = Config::load(&a.theme);
        assert_eq!(config.art, Art::Cube);
        assert_eq!(config.align, "center");
        assert_eq!(config.color, "accent");
        let bytes = a.get("/ascii", &Identity::Anonymous).expect("a page");
        assert!(!bytes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every knob in the table lands.
    #[test]
    fn the_table_configures_it() {
        let (dir, a) = with_theme(
            "[desktop.ascii]\nart = \"wave\"\nseconds = 0.5\ncolor = \"#ff8800\"\nalign = \"left\"\n",
            "conf",
        );
        let config = Config::load(&a.theme);
        assert_eq!(config.art, Art::Wave);
        assert_eq!(config.seconds, 0.5);
        assert_eq!(config.color, "#ff8800");
        assert_eq!(config.align, "left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder of drawings is an animation in name order, and an empty one
    /// says so instead of drawing nothing.
    #[test]
    fn a_folder_of_frames_is_an_animation() {
        let (dir, a) = with_theme("[colors]\n", "frames");
        let art_dir = dir.join("art");
        std::fs::create_dir_all(&art_dir).unwrap();
        std::fs::write(art_dir.join("01.txt"), "one\n").unwrap();
        std::fs::write(art_dir.join("02.txt"), "two\n").unwrap();
        std::fs::write(art_dir.join("notes.md"), "ignored\n").unwrap();

        let frames = read_frames(&art_dir);
        assert_eq!(frames.len(), 2, "only .txt, and both of them");
        assert_eq!(frames[0], vec!["one".to_string()]);

        let config = Config {
            art: Art::Frames(art_dir.clone()),
            seconds: 1.0,
            color: "accent".into(),
            align: "center".into(),
        };
        // Time picks the frame, and it wraps.
        assert_eq!(a.frame(&config, 10, 4, 0.0), vec!["one".to_string()]);
        assert_eq!(a.frame(&config, 10, 4, 1.0), vec!["two".to_string()]);
        assert_eq!(a.frame(&config, 10, 4, 2.0), vec!["one".to_string()]);

        let empty = Config { art: Art::Frames(dir.join("nothing")), ..config };
        assert!(a.frame(&empty, 10, 4, 0.0)[0].contains("no .txt frames"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grid follows the area the client reported, and the page says how
    /// often it wants to be re-read.
    #[test]
    fn the_grid_fits_the_widget_and_carries_a_clock() {
        assert_eq!(parse_fit("380x200"), Some((380.0, 200.0)));
        assert_eq!(parse_fit("{w}x{h}"), None);

        let (dir, a) = with_theme("[desktop.ascii]\nart = \"plasma\"\nseconds = 0.25\n", "fit");
        let bytes = a.get("/ascii/fit/600x300", &Identity::Anonymous).expect("a page");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let live = doc.nodes.iter().find_map(|n| match n {
            rill_doc::Node::Live { target, interval } => {
                Some((doc.string(*target).to_string(), *interval))
            }
            _ => None,
        });
        assert_eq!(live, Some(("/ascii/fit/{w}x{h}".to_string(), 250)));

        // A taller widget is more rows of art.
        let small = rill_doc::decode(&a.get("/ascii/fit/600x120", &Identity::Anonymous).unwrap())
            .unwrap()
            .nodes
            .iter()
            .filter(|n| matches!(n, rill_doc::Node::Row { .. }))
            .count();
        let tall = rill_doc::decode(&a.get("/ascii/fit/600x400", &Identity::Anonymous).unwrap())
            .unwrap()
            .nodes
            .iter()
            .filter(|n| matches!(n, rill_doc::Node::Row { .. }))
            .count();
        assert!(tall > small, "{tall} rows at 400px vs {small} at 120px");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
