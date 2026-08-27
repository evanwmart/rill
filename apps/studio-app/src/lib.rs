//! Theme Studio — the desktop's look, adjustable from inside it.
//!
//! Built on `rill_appkit`, like the file explorer: the same shell, the same
//! metrics, the same style table — which is the point. A settings app that
//! invented its own chrome would be the first evidence the kit does not
//! work. Categories live in the sidebar (each is a served page, so every
//! section is addressable and the back button means something); controls sit
//! in an aligned grid, labels sharing a measure group so every column lines
//! up by construction rather than by hand.
//!
//! Everything here edits `theme.toml`, the one file the whole desktop
//! watches:
//!
//! * **Density** — `[metrics]`; every kit app re-densifies on its next page.
//! * **Palette / Colors** — `[colors]`; token surfaces re-skin live.
//! * **Window** — `[window]`; chrome opacity, radius, and how the compositor
//!   dresses a window: the focus ring around the active one, the shadow
//!   under every one.
//! * **Showroom** — `[desktop.showroom]`; the 3D scene's lights, spin,
//!   camera, and colours, read by the background shader and the model pass.
//! * **Desktop** — `[desktop]`; glass, boids, stats, override, effect
//!   shaders, and shader wallpapers.
//!
//! Honest limits: no drag primitive yet, so "sliders" are steppers and the
//! colour picker is a grid; and editing `theme.toml` assumes the server and
//! the desktop share a machine.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rill_auth::Identity;
use rill_protocol::{ActionValue, Status};
use rill_appkit::kdl_escape;
use rill_server::AppHandler;

/// The eight tokens worth editing by hand, with the built-in dark values as
/// the display fallback when `theme.toml` doesn't name them.
const TOKENS: &[(&str, &str)] = &[
    ("page", "#121219"),
    ("surface", "#1b1b28"),
    ("surface-raised", "#242438"),
    ("text", "#e8e8f0"),
    ("text-muted", "#9a9ab0"),
    ("accent", "#7c5cff"),
    ("accent-text", "#ffffff"),
    ("border", "#33334a"),
];

/// The shell's preset palettes, duplicated as data from
/// `rill_viewport::theme::palettes()` — the studio is a *server* app and the
/// viewer crate is the client side; sharing the list would mean depending on
/// the whole client stack for eight color pairs. If these drift, the switcher
/// and the studio disagree visibly, which is the failure mode that gets fixed.
const PALETTES: &[(&str, &[(&str, &str)])] = &[
    ("Mono", &[
        ("page", "#131313"), ("surface", "#1d1d1d"), ("surface-raised", "#282828"),
        ("text", "#ececec"), ("text-muted", "#969696"), ("accent", "#e0e0e0"),
        ("accent-text", "#131313"), ("border", "#3a3a3a"),
    ]),
    ("Midnight", &[
        ("page", "#0e1020"), ("surface", "#161a2e"), ("surface-raised", "#212747"),
        ("text", "#e9ecff"), ("text-muted", "#8b90b8"), ("accent", "#6ea8ff"),
        ("accent-text", "#0b1024"), ("border", "#2b315a"),
    ]),
    ("Dusk", &[
        ("page", "#1a1020"), ("surface", "#241528"), ("surface-raised", "#3a2142"),
        ("text", "#f6e9ff"), ("text-muted", "#b892c8"), ("accent", "#ff7ac6"),
        ("accent-text", "#1a1020"), ("border", "#4a2e52"),
    ]),
    ("Forest", &[
        ("page", "#0c1613"), ("surface", "#111f1a"), ("surface-raised", "#1a2f26"),
        ("text", "#e6f4ec"), ("text-muted", "#84a795"), ("accent", "#5fd39a"),
        ("accent-text", "#08130d"), ("border", "#274539"),
    ]),
    ("Paper", &[
        ("page", "#f4f1ea"), ("surface", "#ffffff"), ("surface-raised", "#ece7db"),
        ("text", "#2a2620"), ("text-muted", "#6b6558"), ("accent", "#b4552d"),
        ("accent-text", "#ffffff"), ("border", "#d8d2c4"),
    ]),
    ("Synthwave", &[
        ("page", "#160b2e"), ("surface", "#1f1140"), ("surface-raised", "#33195e"),
        ("text", "#f2e7ff"), ("text-muted", "#a48ad4"), ("accent", "#00e5ff"),
        ("accent-text", "#12082a"), ("border", "#ff2ea6"),
    ]),
    ("Ember", &[
        ("page", "#171310"), ("surface", "#211a15"), ("surface-raised", "#33271d"),
        ("text", "#f7ede2"), ("text-muted", "#b39a84"), ("accent", "#ff9d45"),
        ("accent-text", "#1c130b"), ("border", "#4a382a"),
    ]),
];

/// The showroom's numeric knobs: key, default, min, max, step. Written to
/// `[desktop.showroom]`, read by the background shader and the model pass
/// alike — one table, one scene.
const SHOWROOM_KNOBS: &[(&str, f64, f64, f64, f64)] = &[
    ("spin", 0.08, -0.6, 0.6, 0.02),
    ("distance", 3.88, 1.2, 14.0, 0.25),
    ("exposure", 1.0, 0.15, 3.0, 0.1),
    ("key_azimuth", -42.0, -180.0, 180.0, 15.0),
    ("key_elevation", 55.0, -10.0, 89.0, 5.0),
    ("key_intensity", 7.2, 0.0, 20.0, 0.8),
    ("fill_azimuth", 60.0, -180.0, 180.0, 15.0),
    ("fill_elevation", 18.0, -10.0, 89.0, 5.0),
    ("fill_intensity", 1.8, 0.0, 12.0, 0.4),
    ("reflection", 0.30, 0.0, 1.0, 0.05),
    ("reflection_fade", 0.42, 0.05, 2.0, 0.08),
    ("backdrop_glow", 0.45, 0.0, 2.0, 0.1),
    ("rings", 1.0, 0.0, 3.0, 0.25),
    ("vignette", 0.55, 0.0, 1.5, 0.1),
];

/// The showroom's colours, with the built-in defaults as display fallback.
/// These live in `[desktop.showroom]`, not `[colors]` — a scene's lights
/// are not the desktop's palette.
/// `[window]` colours — how the compositor dresses a window.
const WINDOW_COLORS: &[(&str, &str)] = &[
    ("focus_glow", "#6ea8ff"),
    ("shadow_color", "#000000"),
];

/// The colours that live outside `[colors]`, as picker targets with the
/// labels the Colour page shows. One room for everything the desktop
/// wears: the tokens above, then these.
const SURFACE_COLORS: &[(&str, &str)] = &[
    ("win:focus_glow", "focus glow"),
    ("win:shadow_color", "window shadow"),
    ("cur:color", "cursor"),
    ("cur:outline", "cursor outline"),
    ("desk:background", "desktop floor"),
];

const SHOWROOM_COLORS: &[(&str, &str)] = &[
    ("key_color", "#ffb98a"),
    ("fill_color", "#8fa8ff"),
    ("rim_color", "#a3bcff"),
    ("body_color", "#7a7a80"),
    ("ground_color", "#2e2f38"),
    ("backdrop_color", "#3c3e4a"),
];

/// F/P bounds (mirrored by `Metrics::from_theme_file`): below these the type
/// is unreadable, above them the demo stops saying anything.
const F_RANGE: (f32, f32) = (10.0, 24.0);
const P_RANGE: (f32, f32) = (2.0, 16.0);

/// The `[window]` glass knobs: key, built-in default, max, stepper step.
/// (Defaults mirror `rill_viewport::theme::WindowStyle` — same duplication
/// argument as PALETTES.)
const WINDOW_KNOBS: &[(&str, f64, f64, f64)] = &[
    ("focus_glow_blur", 18.0, 90.0, 2.0),
    ("focus_glow_alpha", 230.0, 255.0, 16.0),
    ("shadow_blur", 26.0, 120.0, 2.0),
    ("shadow_alpha", 140.0, 255.0, 16.0),
    ("chrome_alpha", 74.0, 255.0, 16.0),
    ("chrome_raised_alpha", 180.0, 255.0, 16.0),
    ("glass_body_alpha", 60.0, 255.0, 16.0),
    ("blur", 28.0, 128.0, 4.0),
    ("radius", 14.0, 64.0, 2.0),
    ("titlebar", 34.0, 120.0, 2.0),
    ("titlebar_tall", 44.0, 120.0, 2.0),
];

/// The desktop effect-shader choices (label, file in `assets/shaders/`,
/// input-warp barrel for CRT) and the generative shader wallpapers —
/// moved here from the dock: the studio is where the desktop is shaped,
/// the dock only launches.
/// The one thing an effect cannot work out for itself: how much the
/// compositor should bend *input* to match the picture. A CRT's curve moves
/// what is under the pointer, so the hit test has to curve with it.
const EFFECT_BARRELS: &[(&str, f64)] = &[("crt", 0.07)];

/// Wallpapers are found, not declared: every `.wgsl` in the shader
/// directories that paints the desktop rather than filtering it. Dropping a
/// file in is the whole of adding a background — the list used to be a const
/// here, which meant a new shader was invisible until someone remembered to
/// name it in two places.
///
/// The two kinds separate by what they read: a post-process effect samples
/// `scene`, a wallpaper never does, since there is nothing under it. That is
/// a property of the shader itself, so it cannot drift out of step the way a
/// registry does.
fn wall_choices() -> Vec<(String, PathBuf)> {
    shader_files().into_iter().filter(|(_, _, effect)| !*effect).map(|(a, b, _)| (a, b)).collect()
}

/// How many image chips the background page offers before it stops — a
/// Pictures folder can be enormous, and the page says when it capped.
const IMAGE_CHOICE_CAP: usize = 24;

/// Image wallpapers on disk as (stem, path): `~/.config/rill/wallpapers`
/// first (a curated copy there shadows a Pictures original of the same
/// name), then the top level of `~/Pictures`. The same shape as the shader
/// lists: dropping a file in a folder is the whole of adding a choice.
fn image_choices() -> (Vec<(String, PathBuf)>, bool) {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for dir in [home.join(".config/rill/wallpapers"), home.join("Pictures")] {
        let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    ["jpg", "jpeg", "png"].iter().any(|x| x.eq_ignore_ascii_case(e))
                })
            })
            .filter_map(|p| Some((p.file_stem()?.to_string_lossy().into_owned(), p)))
            .collect();
        found.sort();
        found.retain(|(stem, _)| !out.iter().any(|(s, _)| s == stem));
        out.extend(found);
    }
    out.sort();
    let capped = out.len() > IMAGE_CHOICE_CAP;
    out.truncate(IMAGE_CHOICE_CAP);
    (out, capped)
}

/// Every usable shader on disk as (stem, path, is_effect), sorted by name.
fn shader_files() -> Vec<(String, PathBuf, bool)> {
    let mut out: Vec<(String, PathBuf, bool)> = Vec::new();
    for dir in [user_shader_dir(), shader_dir()] {
        let mut found: Vec<(String, PathBuf, bool)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "wgsl"))
            .filter_map(|p| {
                let src = std::fs::read_to_string(&p).ok()?;
                // A file someone is midway through pasting is not a choice.
                if src.trim().is_empty() {
                    return None;
                }
                let stem = p.file_stem()?.to_string_lossy().into_owned();
                // Only shaders written against the *fx* preamble belong in
                // the wallpaper and screen-effect lists. A particle pass or
                // a per-window effect is a different contract entirely, and
                // offering one here compiles it against the wrong preamble
                // and rejects it — which is exactly what happened the first
                // time particle shaders landed in this directory.
                if role_of_shader(&stem) != ShaderRole::Fx {
                    return None;
                }
                let effect = samples_the_scene(&src);
                Some((stem, p, effect))
            })
            .collect();
        found.sort();
        // First directory wins a name: a shader dropped in the config dir
        // shadows a bundled one of the same name.
        found.retain(|(stem, ..)| !out.iter().any(|(s, ..)| s == stem));
        out.extend(found);
    }
    out.sort();
    out
}

/// The screen-effect chips, found the same way as the wallpapers and by the
/// same test read the other way round: a grader is a shader that samples the
/// composited desktop, because filtering it is the whole job.
fn effect_choices() -> Vec<(String, PathBuf)> {
    shader_files().into_iter().filter(|(_, _, effect)| *effect).map(|(a, b, _)| (a, b)).collect()
}

/// What contract a shader file is written against, decided by its name.
///
/// The name is the declaration because it has to be readable *before* the
/// file is compiled — the studio lists choices without compiling them, and
/// compiling a compute pass against the fullscreen-fx preamble does not
/// fail gracefully, it fails with a wall of errors about a missing `params`.
///
/// ```text
/// <stem>_update.wgsl    particle simulation step   (per agent)
/// <stem>_diffuse.wgsl   particle field pass        (per pixel)
/// <stem>_draw.wgsl      particle drawing
/// window_<name>.wgsl    per-window effect
/// anything else         fullscreen fx: a wallpaper or a grader
/// ```
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum ShaderRole {
    Fx,
    ParticleUpdate,
    ParticleDiffuse,
    ParticleDraw,
    WindowFx,
}

fn role_of_shader(stem: &str) -> ShaderRole {
    if stem.ends_with("_update") {
        ShaderRole::ParticleUpdate
    } else if stem.ends_with("_diffuse") {
        ShaderRole::ParticleDiffuse
    } else if stem.ends_with("_draw") {
        ShaderRole::ParticleDraw
    } else if stem.starts_with("window_") {
        ShaderRole::WindowFx
    } else {
        ShaderRole::Fx
    }
}

/// Every `.wgsl` on disk with its stem, whatever its role — the raw listing
/// the role-specific pickers filter.
fn all_shader_files() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for dir in [user_shader_dir(), shader_dir()] {
        let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "wgsl"))
            .filter(|p| std::fs::read_to_string(p).is_ok_and(|s| !s.trim().is_empty()))
            .filter_map(|p| {
                let stem = p.file_stem()?.to_string_lossy().into_owned();
                Some((stem, p))
            })
            .collect();
        found.sort();
        found.retain(|(stem, _)| !out.iter().any(|(s, _)| s == stem));
        out.extend(found);
    }
    out.sort();
    out
}

/// The per-window effects: `window_<name>.wgsl`.
fn window_fx_choices() -> Vec<(String, PathBuf)> {
    all_shader_files()
        .into_iter()
        .filter(|(stem, _)| role_of_shader(stem) == ShaderRole::WindowFx)
        .collect()
}

/// A particle set is a family sharing a stem: `<name>_update` is required,
/// `<name>_diffuse` and `<name>_draw` are optional. Dropping the files in is
/// the whole of adding one — the same rule the wallpapers already follow.
fn particle_sets() -> Vec<ParticleSet> {
    let files = all_shader_files();
    let find = |name: &str| files.iter().find(|(s, _)| s == name).map(|(_, p)| p.clone());
    files
        .iter()
        .filter(|(stem, _)| role_of_shader(stem) == ShaderRole::ParticleUpdate)
        .map(|(stem, path)| {
            let base = stem.trim_end_matches("_update").to_string();
            ParticleSet {
                count: declared_count(path).unwrap_or(DEFAULT_PARTICLES),
                diffuse: find(&format!("{base}_diffuse")),
                draw: find(&format!("{base}_draw")),
                update: path.clone(),
                name: base,
            }
        })
        .collect()
}

/// One installable particle simulation.
struct ParticleSet {
    name: String,
    update: PathBuf,
    diffuse: Option<PathBuf>,
    draw: Option<PathBuf>,
    /// How many agents this simulation wants.
    count: i64,
}

/// The flock's worth, and what a set that declines to say gets.
///
/// Small on purpose. The flock is *scenery* — a few birds crossing a
/// wallpaper — and it reads better sparse than dense: at a couple of
/// thousand the separation rule keeps them apart and the screen turns into
/// static. A field simulation like the slime is the opposite case, and says
/// so in its own shader.
const DEFAULT_PARTICLES: i64 = 200;

/// A set's agent count, declared in its own update shader:
///
/// ```text
/// // @particles 200000
/// ```
///
/// It belongs in the file because it is a property of the simulation, not a
/// preference: a flock of two hundred thousand boids is a smear, and a slime
/// mould of two thousand agents never forms a network at all. Reading it
/// here keeps a set to one file to add — the same rule the wallpapers follow,
/// and for the same reason the wallpaper list stopped being a const.
fn declared_count(path: &Path) -> Option<i64> {
    let src = std::fs::read_to_string(path).ok()?;
    src.lines()
        .filter_map(|l| l.trim().strip_prefix("//"))
        .filter_map(|l| l.trim().strip_prefix("@particles"))
        .find_map(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map(|n| n.min(rill_gpu_max_particles()))
}

/// Mirrors `rill_gpu::MAX_PARTICLES` without taking a dependency on the
/// renderer from an app; the compositor clamps to the same number anyway, so
/// this only keeps the studio from *offering* an impossible one.
fn rill_gpu_max_particles() -> i64 {
    1_000_000
}

/// Whether a shader reads the composited desktop — i.e. is an effect layered
/// over everything rather than a background painted beneath it. Whitespace is
/// stripped first so the test survives whatever line breaking the shader uses.
fn samples_the_scene(src: &str) -> bool {
    let dense: String = src.chars().filter(|c| !c.is_whitespace()).collect();
    dense.contains("(scene,")
}

/// A wallpaper's chip label: the file stem, underscores opened up, so
/// `window_aura.wgsl` reads as "window aura" without renaming the file.
fn wall_label(stem: &str) -> String {
    stem.replace('_', " ")
}

/// Where a person drops models: `~/.config/rill/models/*.obj`. The scene's
/// own shader may sit beside a mesh as `<stem>.wgsl`; otherwise the bundled
/// cinematic one is used.
fn models_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/rill/models")
}

/// The shader a mesh wears when it brings none of its own: the generic
/// auto-fitting toon one, which asks nothing of the model.
fn default_model_shader() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/models/figure_toon.wgsl"))
}

fn shader_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../shaders"))
}

/// Where a person drops shaders of their own: `~/.config/rill/shaders`,
/// beside the models directory and read the same way.
fn user_shader_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/rill/shaders")
}

/// The picker grid: 12 hues × these lightness rows (sat 0.65), plus a grey
/// ramp. Small enough to read, wide enough to land close; the hex field
/// remains for the exact value.
const GRID_LIGHTNESS: &[f32] = &[0.85, 0.70, 0.55, 0.40, 0.28, 0.18];
const GRID_HUES: usize = 12;

/// The pointer's own knobs — `[cursor]` in theme.toml. The compositor
/// draws the cursor as geometry rather than borrowing the host's bitmap,
/// so it themes like everything else.
const CURSOR_KNOBS: &[(&str, f64, f64, f64, f64)] = &[
    ("size", 22.0, 8.0, 96.0, 2.0),
    ("shadow", 90.0, 0.0, 255.0, 16.0),
];

const CURSOR_COLORS: &[(&str, &str)] = &[("color", "#121218"), ("outline", "#f2f2f6")];

/// The strip's shape: key, default, min, max, step. Defaults match
/// `DockStyle`'s — a dock nobody has configured follows the density, and
/// these are what it lands on at F14/P6.
const DOCK_KNOBS: &[(&str, f64, f64, f64, f64)] = &[
    ("height", 44.0, 20.0, 200.0, 2.0),
    ("padding", 6.0, 0.0, 40.0, 1.0),
    ("gap", 6.0, 0.0, 40.0, 1.0),
    ("corner", 0.0, 0.0, 40.0, 2.0),
    ("icon", 26.0, 12.0, 96.0, 2.0),
];

/// What the strip is made of. Same three the dock understands.
const DOCK_BACKGROUNDS: &[(&str, &str)] =
    &[("glass", "Glass"), ("solid", "Solid"), ("none", "None")];

/// What the dock can hold. Order within a slot is the priority, so a slot
/// is simply a list — which is why placing an item is moving a name.
const DOCK_ITEMS: &[(&str, &str)] = &[
    ("menu", "Mark + apps"),
    ("clock", "Clock"),
    ("apps", "App links"),
];

const DOCK_SLOTS: &[(&str, &str)] = &[("left", "Left"), ("center", "Centre"), ("right", "Right")];

/// The sidebar's categories: (slug, label, icon). Each is a served page.
/// The anchors a widget can hug, spelled as the compositor parses them.
const ANCHORS: &[&str] = &["top-left", "top-right", "bottom-left", "bottom-right", "center"];

/// The sidebar, in the order someone actually reaches for these.
///
/// Coarse to fine: a whole saved look first, then the two things that decide
/// how a desktop *reads* (its palette and its type), then the surfaces —
/// windows, the desktop itself, the dock, the widgets on it — and finally
/// the two specialised corners, the pointer and the 3D scene.
///
/// It grew a page at a time and the order was whatever came next; this is
/// the order the settings themselves suggest. The slugs are unchanged, so
/// existing links and the tests that use them still work — only the labels,
/// icons and order moved.
const SECTIONS: &[(&str, &str, &str)] = &[
    ("appearance", "Appearance", "star-fill"),
    ("rices", "Looks", "folder-fill"),
    ("colors", "Colours", "grid"),
    ("background", "Background", "image-fill"),
    ("density", "Type", "list"),
    ("window", "Windows", "file-fill"),
    ("dock", "Dock", "clock-fill"),
    ("widgets", "Widgets", "film-fill"),
    ("effects", "Effects", "play"),
    ("cursor", "Pointer", "pencil"),
    ("showroom", "Scene", "world-fill"),
];

pub struct Studio {
    theme_path: PathBuf,
    /// Which token the picker edits. View state, like a selection.
    target: Mutex<String>,
    /// The last few colours picked, newest first — the working palette a
    /// person builds up while theming, offered back as one-click swatches.
    recent: Mutex<Vec<String>>,
    /// Which category the sidebar has open — a served page, so it is
    /// addressable and the back button works.
    section: Mutex<String>,
    /// The saved look mid-rename, if any. View state like `target`: the
    /// Looks page grows a rename row while this is set.
    rename: Mutex<Option<String>>,
}

impl Studio {
    /// `theme_path` is the desktop's `theme.toml` — the studio edits the
    /// same file the compositor and every window already watch.
    pub fn new(theme_path: PathBuf) -> Studio {
        Studio {
            theme_path,
            target: Mutex::new("accent".to_string()),
            recent: Mutex::new(Vec::new()),
            section: Mutex::new("appearance".to_string()),
            rename: Mutex::new(None),
        }
    }

    /// Remember a picked rgb for the recent row. Newest first, no
    /// duplicates, eight deep — a working palette, not a history.
    fn remember(&self, rgb: &str) {
        let mut recent = self.recent.lock().unwrap();
        recent.retain(|c| c != rgb);
        recent.insert(0, rgb.to_string());
        recent.truncate(8);
    }

    /// A named table from `theme.toml`, as written.
    /// The ANSI colours this theme currently names, in palette order.
    fn ansi_now(&self) -> Vec<(String, String)> {
        let colors = self.table("colors");
        ANSI_ORDER
            .iter()
            .filter_map(|name| {
                let value = colors.get(*name)?.as_str()?.to_string();
                Some((name.to_string(), value))
            })
            .collect()
    }

    fn table(&self, name: &str) -> toml::Table {
        std::fs::read_to_string(&self.theme_path)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get(name)?.as_table().cloned())
            .unwrap_or_default()
    }

    /// Edit `[desktop.ascii]`, the ASCII widget's own table.
    fn update_ascii(&self, f: impl FnOnce(&mut toml::Table)) -> Result<(), Status> {
        self.update_table("desktop", |desktop| {
            let ascii = desktop
                .entry("ascii")
                .or_insert_with(|| toml::Value::Table(Default::default()));
            if let Some(table) = ascii.as_table_mut() {
                f(table);
            }
        })
    }

    /// Edit one table of `theme.toml` in place, preserving the rest of the
    /// file — the same discipline as the dock's `[desktop]` edits. An edit
    /// that empties the table removes it, so a fully-reset file is clean.
    fn update_table(&self, name: &str, f: impl FnOnce(&mut toml::Table)) -> Result<(), Status> {
        if let Some(dir) = self.theme_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut root: toml::Table = std::fs::read_to_string(&self.theme_path)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        let table = root
            .entry(name)
            .or_insert(toml::Value::Table(Default::default()));
        let table = table.as_table_mut().ok_or(Status::Internal)?;
        f(table);
        if table.is_empty() {
            root.remove(name);
        }
        let text = toml::to_string_pretty(&root).map_err(|_| Status::Internal)?;
        std::fs::write(&self.theme_path, text).map_err(|_| Status::Internal)
    }

    /// A token's current value — always a validated hex string, because it
    /// is embedded in KDL and painted as a literal, and `theme.toml` is
    /// user-editable text.
    /// Where a picker target's value lives: `sr:name` in the showroom
    /// table, anything else among the theme's colour tokens.
    fn color_home(token: &str) -> (&'static str, String, &'static str) {
        let look = |table: &'static str, name: &str, set: &[(&'static str, &'static str)]| {
            let fallback =
                set.iter().find(|(k, _)| *k == name).map(|(_, v)| *v).unwrap_or("#808080");
            (table, name.to_string(), fallback)
        };
        if let Some(name) = token.strip_prefix("sr:") {
            return look("showroom", name, SHOWROOM_COLORS);
        }
        if let Some(name) = token.strip_prefix("win:") {
            return look("window", name, WINDOW_COLORS);
        }
        if let Some(name) = token.strip_prefix("cur:") {
            return look("cursor", name, CURSOR_COLORS);
        }
        if token == "desk:background" {
            return ("desktop", "background_color".to_string(), "#104c2e");
        }
        look("colors", token, TOKENS)
    }

    /// The models a person can choose: everything in `~/.config/rill/models`,
    /// plus whatever the scene currently points at (so a model configured by
    /// hand still shows as the active choice).
    fn model_choices(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = std::fs::read_dir(models_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            // A model is a mesh file *or* a directory of parts (a body
            // ships as body/stand/tail) — the chooser listed only files, so
            // a part-directory model was invisible.
            .filter(|p| {
                let mesh_ext = |p: &PathBuf| {
                    p.extension().is_some_and(|e| {
                        let e = e.to_ascii_lowercase();
                        e == "obj" || e == "stl"
                    })
                };
                if mesh_ext(p) {
                    return true;
                }
                p.is_dir()
                    && std::fs::read_dir(p)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .any(|e| mesh_ext(&e.path()))
            })
            .map(|p| {
                (
                    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                    p.display().to_string(),
                )
            })
            .collect();
        out.sort();
        if let Some(current) = self.showroom().get("model").and_then(|v| v.as_str())
            && !out.iter().any(|(_, p)| p == current)
        {
            let label = PathBuf::from(current)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "model".into());
            out.insert(0, (label, current.to_string()));
        }
        out
    }

    /// The shader that dresses a mesh: one sitting beside it as
    /// `<stem>.wgsl`, else the generic figure shader. A directory of parts
    /// looks for `<dir>.wgsl` beside the directory.
    fn shader_for_model(model: &str) -> String {
        let sibling = PathBuf::from(model).with_extension("wgsl");
        if sibling.exists() {
            return sibling.display().to_string();
        }
        default_model_shader().display().to_string()
    }

    /// A model may ship scene hints beside it as `<stem>.toml` — which way
    /// is up, how big it wants to be, where to start its turn. Without them
    /// every new mesh needs the same three knobs hunted down by hand; with
    /// them, choosing a model just works.
    fn apply_model_hints(&self, model: &str) -> Result<(), Status> {
        let hints = PathBuf::from(model).with_extension("toml");
        let Some(table) = std::fs::read_to_string(&hints)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
        else {
            return Ok(());
        };
        self.update_showroom(move |t| {
            for key in ["model_up", "model_scale", "model_lift", "spin_phase", "spin"] {
                if let Some(v) = table.get(key) {
                    t.insert(key.to_string(), v.clone());
                }
            }
        })
    }

    /// The model belongs to the *showroom scene*, not to the desktop: it
    /// renders only while the showroom is the active wallpaper. This mirrors
    /// the scene's choice into the live `[desktop]` slot the compositor
    /// reads — or clears it when some other wallpaper is up.
    /// Make the background a plain colour: set the floor, and turn off the
    /// two layers that would hide it. What you picked is what you see.
    fn set_background_color(&self, hex: String) -> Result<(), Status> {
        self.update_table("desktop", move |t| {
            t.insert("background_color".into(), toml::Value::String(hex));
            t.remove("wallpaper");
            t.remove("background_shader");
        })?;
        self.sync_model()
    }

    /// Make the background an image: set the wallpaper, and turn off the
    /// shader that would cover it. The colour stays — it is the floor the
    /// image sits on, and what a transparent PNG shows through.
    fn set_background_image(&self, path: PathBuf) -> Result<(), Status> {
        self.update_table("desktop", move |t| {
            t.insert("wallpaper".into(), toml::Value::String(path.display().to_string()));
            t.remove("background_shader");
        })?;
        self.sync_model()
    }

    fn sync_model(&self) -> Result<(), Status> {
        let desktop = self.table("desktop");
        let showroom_active = desktop
            .get("background_shader")
            .and_then(|v| v.as_str())
            .is_some_and(|p| p.ends_with("showroom.wgsl"));
        let scene_model = self.showroom().get("model").and_then(|v| v.as_str()).map(str::to_string);
        self.update_table("desktop", move |t| match (showroom_active, scene_model) {
            (true, Some(model)) => {
                let shader = Self::shader_for_model(&model);
                t.insert("model".into(), toml::Value::String(model));
                t.insert("model_shader".into(), toml::Value::String(shader));
            }
            _ => {
                t.remove("model");
                t.remove("model_shader");
            }
        })
    }

    /// A colour's current value wherever it lives — always validated hex,
    /// because it is embedded in KDL and painted as a literal.
    fn color_value(&self, home: &str, name: &str, fallback: &str) -> String {
        let table = match home {
            "showroom" => self.showroom(),
            other => self.table(other),
        };
        table
            .get(name)
            .and_then(|v| v.as_str())
            .filter(|s| is_hex_color(s))
            .unwrap_or(fallback)
            .to_string()
    }

    /// Write a colour to whichever table owns it.
    fn set_color(&self, home: &str, name: String, value: String) -> Result<(), Status> {
        match home {
            "desktop" if name == "background_color" => self.set_background_color(value),
            "showroom" => self.update_showroom(move |t| {
                t.insert(name, toml::Value::String(value));
            }),
            other => self.update_table(other, move |t| {
                t.insert(name, toml::Value::String(value));
            }),
        }
    }

    /// The dock sub-table (`[desktop.dock]`), as written.
    fn dock(&self) -> toml::Table {
        std::fs::read_to_string(&self.theme_path)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get("desktop")?.get("dock")?.as_table().cloned())
            .unwrap_or_default()
    }

    /// Edit `[desktop.dock]` in place.
    fn update_dock(&self, f: impl FnOnce(&mut toml::Table)) -> Result<(), Status> {
        self.update_table("desktop", |desktop| {
            let d = desktop.entry("dock").or_insert(toml::Value::Table(Default::default()));
            if let Some(t) = d.as_table_mut() {
                f(t);
                if t.is_empty() {
                    desktop.remove("dock");
                }
            }
        })
    }

    /// One slot's items, defaulting to the dock's own arrangement.
    fn dock_slot(&self, slot: &str) -> Vec<String> {
        let table = self.dock();
        match table.get(slot).and_then(|v| v.as_array()) {
            Some(arr) => {
                arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
            }
            None => match slot {
                "left" => vec!["menu".to_string()],
                "center" => vec!["clock".to_string()],
                _ => Vec::new(),
            },
        }
    }

    /// Move an item to a slot (or nowhere), keeping every slot exclusive —
    /// an item lives in one place, which is what makes clicking a slot mean
    /// "put it here" rather than "add another one".
    fn place_dock_item(&self, item: &str, slot: &str) -> Result<(), Status> {
        let mut slots: Vec<(String, Vec<String>)> = DOCK_SLOTS
            .iter()
            .map(|(name, _)| (name.to_string(), self.dock_slot(name)))
            .collect();
        for (_, items) in &mut slots {
            items.retain(|i| i != item);
        }
        if slot != "off"
            && let Some((_, items)) = slots.iter_mut().find(|(name, _)| name == slot)
        {
            items.push(item.to_string());
        }
        self.update_dock(move |t| {
            for (name, items) in slots {
                t.insert(
                    name,
                    toml::Value::Array(items.into_iter().map(toml::Value::String).collect()),
                );
            }
        })
    }

    /// Reorder within a slot: priority is position, so this is a swap.
    fn nudge_dock_item(&self, item: &str, delta: i32) -> Result<(), Status> {
        for (slot, _) in DOCK_SLOTS {
            let mut items = self.dock_slot(slot);
            if let Some(i) = items.iter().position(|x| x == item) {
                let j = (i as i32 + delta).clamp(0, items.len() as i32 - 1) as usize;
                if i == j {
                    return Ok(());
                }
                items.swap(i, j);
                let slot = slot.to_string();
                return self.update_dock(move |t| {
                    t.insert(
                        slot,
                        toml::Value::Array(items.into_iter().map(toml::Value::String).collect()),
                    );
                });
            }
        }
        Ok(())
    }

    /// The showroom sub-table (`[desktop.showroom]`), as written.
    fn showroom(&self) -> toml::Table {
        std::fs::read_to_string(&self.theme_path)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get("desktop")?.get("showroom")?.as_table().cloned())
            .unwrap_or_default()
    }

    /// Edit `[desktop.showroom]` in place — a table inside a table, so it
    /// cannot use `update_table`.
    fn update_showroom(&self, f: impl FnOnce(&mut toml::Table)) -> Result<(), Status> {
        self.update_table("desktop", |desktop| {
            let sr = desktop
                .entry("showroom")
                .or_insert(toml::Value::Table(Default::default()));
            if let Some(t) = sr.as_table_mut() {
                f(t);
                if t.is_empty() {
                    desktop.remove("showroom");
                }
            }
        })
    }


    fn token_value(&self, colors: &toml::Table, token: &str) -> String {
        let fallback = TOKENS
            .iter()
            .find(|(t, _)| *t == token)
            .map(|(_, v)| *v)
            .unwrap_or("#808080");
        colors
            .get(token)
            .and_then(|v| v.as_str())
            .filter(|s| is_hex_color(s))
            .unwrap_or(fallback)
            .to_string()
    }

    /// One section as a kit-shell page: sidebar of categories, the section's
    /// controls in the pane.
    fn page(&self) -> Result<Vec<u8>, Status> {
        let section = self.section.lock().unwrap().clone();
        let m = rill_appkit::Metrics::from_theme_file(&self.theme_path);
        let (f, p) = (m.font_size, m.padding);
        let ib = m.icon_button();
        let cell = (ib * 0.7).round();
        let colors = self.table("colors");
        let target = self.target.lock().unwrap().clone();
        let (t_home, t_name, t_fallback) = Self::color_home(&target);
        let target_value = self.color_value(t_home, &t_name, t_fallback);
        let (_, target_alpha) = split_alpha(&target_value);

        // Styles: the kit's table, plus this app's controls. Swatches paint
        // literal colours because the colours *are* the data on show.
        let mut styles = String::new();
        for (name, pairs) in PALETTES {
            let find = |t: &str, d: &'static str| -> &'static str {
                pairs.iter().find(|(k, _)| *k == t).map(|(_, v)| *v).unwrap_or(d)
            };
            styles.push_str(&format!(
                "style \"chip-{name}\" color=\"{accent}\" background=\"{surface}\" size={f} corner=0 padding={p} underline=#false\n",
                accent = find("accent", "#808080"),
                surface = find("surface-raised", "#282828"),
            ));
        }
        styles.push_str(&format!(
            "style \"chip\" color=\"text\" background=\"surface-raised\" size={f} corner=0 padding={p} underline=#false hover=\"chip--hover\"\n\
             style \"chip--hover\" color=\"text\" background=\"elevation-lg\" size={f} corner=0 padding={p} underline=#false\n\
             style \"chip--on\" color=\"accent-text\" background=\"accent\" size={f} corner=0 padding={p} underline=#false\n\
             style \"stepper\" color=\"text\" background=\"surface-raised\" size={f} corner=0 padding={p} width={ib} hover=\"chip--hover\"\n\
             style \"readout\" font=\"mono\" color=\"text\" size={f} group=\"knob-value\" align=\"right\"\n\
             style \"knob-label\" color=\"text-muted\" size={meta} group=\"knob-label\"\n\
             style \"row\" padding=0 gap={p} valign=\"center\"\n\
             style \"wrap\" wrap=#true padding=0 gap={p} valign=\"center\"\n\
             style \"grid\" wrap=#true padding=0 gap={p}\n\
             style \"knob\" background=\"surface\" padding={p} gap={p} corner=0 valign=\"center\" width={knob}\n\
             style \"param\" color=\"accent\" background=\"surface-raised\" size={f} width={param_w}\n\
             style \"swatchrow\" wrap=#true padding=0 gap=2\n\
             style \"pickrow\" padding=0 gap=0\n\
             style \"hexfield\" color=\"text\" background=\"surface-raised\" font=\"mono\" size={meta} corner=0 padding=6 width={hexw}\n\
             style \"note\" color=\"text-muted\" size={quiet}\n",
            meta = f - 2.0,
            quiet = f - 3.0,
            knob = (m.control_height() * 7.5).round(),
            hexw = (9.0 * (f - 2.0) * 0.65 + 24.0).round(),
            param_w = (m.control_height() * 5.5).round(),
        ));
        for (token, _) in TOKENS {
            styles.push_str(&format!(
                "style \"sw-{token}\" background=\"{}\"\n",
                self.token_value(&colors, token),
            ));
        }
        for (name, value) in self.ansi_now() {
            styles.push_str(&format!("style \"sw-{name}\" background=\"{value}\"\n"));
        }
        for (name, fallback) in SHOWROOM_COLORS {
            styles.push_str(&format!(
                "style \"sw-sr-{name}\" background=\"{}\"\n",
                self.color_value("showroom", name, fallback),
            ));
        }
        for (name, fallback) in WINDOW_COLORS {
            styles.push_str(&format!(
                "style \"sw-win-{name}\" background=\"{}\"\n",
                self.color_value("window", name, fallback),
            ));
        }
        for (name, fallback) in CURSOR_COLORS {
            styles.push_str(&format!(
                "style \"sw-cur-{name}\" background=\"{}\"\n",
                self.color_value("cursor", name, fallback),
            ));
        }
        let mut cells: Vec<String> = Vec::new();
        for &l in GRID_LIGHTNESS {
            for h in 0..GRID_HUES {
                cells.push(hsl_hex(h as f32 * 360.0 / GRID_HUES as f32, 0.65, l));
            }
        }
        for i in 0..GRID_HUES {
            cells.push(hsl_hex(0.0, 0.0, 0.04 + 0.92 * (i as f32) / (GRID_HUES - 1) as f32));
        }
        for c in &cells {
            styles.push_str(&format!(
                "style \"cx-{c}\" background=\"#{c}\" size={f} corner=0 padding=0 width={cell} height={cell}\n"
            ));
        }
        // The saved-look cards: thumbnail, name, and the right-click menu's
        // home. Per-look colour styles are generated beside the cards.
        styles.push_str(&format!(
            "style \"look-card\" background=\"surface\" corner=4 padding={p} gap=4\n\
             style \"look-card--current\" background=\"surface\" corner=4 padding={p} gap=4 border=1 border-color=\"accent\"\n\
             style \"look-name\" color=\"text\" background=\"#00000000\" size={f} corner=0 padding=2 underline=#false ellipsis=#true\n\
             style \"lk-row\" padding=0 gap=6 valign=\"top\"\n\
             style \"lk-winbox\" padding=0 gap=0\n",
        ));
        // The desktop floor's own swatch — `[desktop] background_color`,
        // the compositor's default near-black when unset.
        styles.push_str(&format!(
            "style \"sw-bg\" background=\"{}\"\n",
            self.table("desktop")
                .get("background_color")
                .and_then(|v| v.as_str())
                .filter(|s| is_hex_color(s))
                .unwrap_or("#0e1020"),
        ));

        // --- the section's body ------------------------------------------
        let mut body = String::new();
        let head = |body: &mut String, text: &str| {
            body.push_str(&format!("\t\t\ttext \"{text}\" style=\"hd\"\n"));
        };
        let note = |body: &mut String, text: &str| {
            body.push_str(&format!("\t\t\ttext {} style=\"note\"\n", kdl_escape(text)));
        };
        // Every page opens the same way: one line saying what it controls
        // and which table in theme.toml it writes. The studio is not the
        // only way to set these — the file is — so a page that does not say
        // where it is writing leaves you guessing.
        let intro = |body: &mut String, what: &str, table: &str| {
            body.push_str(&format!(
                "\t\t\ttext {} style=\"note\"\n",
                kdl_escape(&format!("{what}  ·  {table}")),
            ));
        };
        // State slots the current section's controls bind (sliders); folded
        // into the shell's state table below.
        let mut extra_states = String::new();
        match section.as_str() {
            "density" => {
                intro(&mut body, "How big text is, and how much air sits around it", "[metrics]");
                head(&mut body, "TYPE AND SPACING");
                body.push_str("\t\t\trow style=\"grid\" {\n");
                body.push_str(&knob("Type F", &format!("{f}"), "f"));
                body.push_str(&knob("Padding P", &format!("{p}"), "p"));
                body.push_str(&knob("Mono weight", &format!("{}", m.mono_weight), "mono"));
                body.push_str("\t\t\t}\n");
                body.push_str(&format!(
                    "\t\t\ttext \"line {lh:.1}   control {ch:.1}   region {rh:.1}\" style=\"readout\"\n",
                    lh = m.line_height(),
                    ch = m.control_height(),
                    rh = m.region_height(),
                ));
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (label, pf, pp) in [("Compact", 14.0, 6.0), ("Normal", 16.0, 8.0), ("Spacious", 18.0, 10.0)] {
                    let on = (f - pf).abs() < 0.01 && (p - pp).abs() < 0.01;
                    body.push_str(&chip(label, on, &format!("/studio/actions/density/{}", label.to_lowercase())));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(&mut body, "writes [metrics] — every app re-densifies on its next page");
            }
            "appearance" => {
                // The landing page, shaped the way GNOME shapes its
                // Appearance panel: the few decisions that make a desktop
                // yours, each one preview-first and one click deep, with
                // the full rooms a sidebar entry away. The live desktop is
                // the preview for everything here — instant apply, cheap
                // undo, no mockups.
                intro(&mut body, "The desktop at a glance — the deep rooms are in the sidebar", "theme.toml");

                head(&mut body, "LOOK");
                let config = self.theme_path.parent().unwrap_or(Path::new("."));
                let saved = rill_appkit::rices::list(config);
                let current = rill_appkit::rices::current(config, &self.theme_path);
                // The lifecycle line, the piece every preset system needs
                // both directions of: which look this desktop is based on,
                // and — when a knob has turned since — the way to keep the
                // divergence. Windows forks you into "Custom" with a Save
                // button; this is that, in our grammar.
                if current.is_none()
                    && let Some(base) = rill_appkit::rices::last(config)
                {
                    body.push_str(&format!(
                        "\t\t\trow style=\"row\" {{ text {} style=\"note\"; \
                         button {} style=\"text-button\" {{ submit \"/studio/actions/rice/update\" }}\n\
                         link \"Save as new…\" target=\"/studio/rices\"; spacer }}\n",
                        kdl_escape(&format!("Based on {base} — modified.")),
                        kdl_escape(&format!("Update {base}")),
                    ));
                }
                if saved.is_empty() {
                    note(&mut body, "No saved looks yet — set the desktop up, then save it on the Looks page.");
                } else {
                    body.push_str("\t\t\trow style=\"wrap\" {\n");
                    for (i, name) in saved.iter().enumerate().take(8) {
                        if rill_appkit::rices::sanitize(name).as_deref() != Some(name.as_str()) {
                            continue;
                        }
                        let Some(rice_path) = rill_appkit::rices::path(config, name) else {
                            continue;
                        };
                        let (bg, chrome_c, surface_c, accent_c) = look_swatches(&rice_path);
                        styles.push_str(&format!(
                            "style \"ov-bg-{i}\" background=\"{bg}\" width=104 height=64 corner=4 padding=8 gap=0\n\
                             style \"ov-chrome-{i}\" background=\"{chrome_c}\" corner=0\n\
                             style \"ov-surface-{i}\" background=\"{surface_c}\" corner=0\n\
                             style \"ov-accent-{i}\" background=\"{accent_c}\" corner=2\n",
                        ));
                        let label_style = if current.as_deref() == Some(name.as_str()) {
                            "sidebar-label--active"
                        } else {
                            "sidebar-label"
                        };
                        body.push_str(&format!(
                            "\t\t\t\tcolumn gap=4 padding=0 target={target} {{\n\
                             \t\t\t\t\tcolumn style=\"ov-bg-{i}\" {{\n\
                             \t\t\t\t\t\trect height=8 style=\"ov-chrome-{i}\"\n\
                             \t\t\t\t\t\tspacer size=4\n\
                             \t\t\t\t\t\trect height=22 style=\"ov-surface-{i}\"\n\
                             \t\t\t\t\t\tspacer size=4\n\
                             \t\t\t\t\t\trect width=26 height=8 style=\"ov-accent-{i}\"\n\
                             \t\t\t\t\t}}\n\
                             \t\t\t\t\trow gap=0 padding=0 {{ spacer; text {label} style=\"{label_style}\"; spacer }}\n\
                             \t\t\t\t}}\n",
                            target = kdl_escape(&format!("/studio/apply/{name}")),
                            label = kdl_escape(name),
                        ));
                    }
                    body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                }

                head(&mut body, "ACCENT");
                let accent_now = self.color_value("colors", "accent", "#7c5cff");
                body.push_str("\t\t\trow style=\"swatchrow\" {\n");
                for (i, hex) in [
                    "#e25c5c", "#e08a3c", "#d8a657", "#37b86b", "#3fb8b0", "#629fd8",
                    "#7c5cff", "#b17fd4", "#d46a9e",
                ]
                .iter()
                .enumerate()
                {
                    let ring = if accent_now.eq_ignore_ascii_case(hex) { 2 } else { 0 };
                    styles.push_str(&format!(
                        "style \"acc-{i}\" background=\"{hex}\" corner=11 padding=0 width=22 \
                         size=16 border={ring} border-color=\"text\"\n"
                    ));
                    body.push_str(&format!(
                        "\t\t\t\tbutton \"\" style=\"acc-{i}\" {{ submit \"/studio/actions/accent/{}\" }}\n",
                        hex.trim_start_matches('#')
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(&mut body, "one colour, everywhere the desktop highlights — the full palette lives in Colours");

                head(&mut body, "BACKGROUND");
                let desktop = self.table("desktop");
                let floor = desktop
                    .get("background_color")
                    .and_then(|v| v.as_str())
                    .unwrap_or("#0e1020")
                    .to_string();
                let covered = desktop.get("wallpaper").and_then(|v| v.as_str()).is_some()
                    || desktop.get("background_shader").and_then(|v| v.as_str()).is_some();
                styles.push_str(&format!("style \"ov-floor\" background=\"{floor}\" corner=4\n"));
                let wearing = if covered { "an image or shader over" } else { "bare floor" };
                body.push_str(&format!(
                    "\t\t\trow style=\"row\" {{ rect width=44 height=28 style=\"ov-floor\"; \
                     text \"{wearing} {floor}\" style=\"note\"; spacer; \
                     link \"Background settings…\" target=\"/studio/background\" }}\n"
                ));

                head(&mut body, "DENSITY");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                let (f_now, p_now) = (m.font_size, m.padding);
                for (label, pf, pp) in
                    [("Compact", 14.0, 6.0), ("Normal", 16.0, 8.0), ("Spacious", 18.0, 10.0)]
                {
                    let on = (f_now - pf).abs() < 0.01 && (p_now - pp).abs() < 0.01;
                    body.push_str(&chip(
                        label,
                        on,
                        &format!("/studio/actions/density/{}", label.to_lowercase()),
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(&mut body, "type and spacing in one move — the exact knobs live in Type");
            }
            "colors" => {
                intro(&mut body, "The palette in one move, or every colour one at a time", "[colors]");
                // Presets first, the knobs they set beneath — the KDE
                // lesson: a bundle in a different room from its parts reads
                // as two unrelated features.
                head(&mut body, "PRESETS");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (name, _) in PALETTES {
                    body.push_str(&format!(
                        "\t\t\t\tbutton \"{name}\" style=\"chip-{name}\" {{ submit \"/studio/actions/palette/{name}\" }}\n",
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                head(&mut body, "EDITING");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (token, _) in TOKENS {
                    body.push_str(&chip(token, *token == target, &format!("/studio/actions/target/{token}")));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (t, label) in SURFACE_COLORS {
                    body.push_str(&chip(label, *t == target, &format!("/studio/actions/target/{t}")));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                let recent = self.recent.lock().unwrap().clone();
                let picker = color_picker(&target_value, target_alpha, &recent);
                body.push_str(&picker.body);
                styles.push_str(&picker.styles);
                extra_states.push_str(&picker.states);
                head(&mut body, "EXACT VALUES");
                body.push_str("\t\t\trow style=\"grid\" {\n");
                for (token, _) in TOKENS {
                    body.push_str(&format!(
                        "\t\t\t\trow style=\"knob\" {{ rect width={ib} height={ib} style=\"sw-{token}\"; \
                         text \"{token}\" style=\"knob-label\"; spacer; \
                         text_input bind=\"hex-{token}\" style=\"hexfield\" placeholder=\"#rrggbb\" {{ \
                         submit \"/studio/actions/set/{token}\" {{ field \"value\" from=\"hex-{token}\" }} }} }}\n"
                    ));
                }
                body.push_str("\t\t\t}\n");
                head(&mut body, "TERMINAL PALETTE");
                body.push_str(
                    "\t\t\ttext \"The sixteen colours a terminal paints with. Derive them from \
                     the accent, the page and the text, then edit any of them by hand — they are \
                     ordinary theme colours once written.\" style=\"note\"\n",
                );
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(
                    "\t\t\t\tbutton \"Derive from this theme\" style=\"text-button\" \
                     { submit \"/studio/actions/derive_ansi\" }\n",
                );
                body.push_str(
                    "\t\t\t\tbutton \"Back to the stock palette\" style=\"text-button\" \
                     { submit \"/studio/actions/clear-ansi\" }\n",
                );
                body.push_str("\t\t\t}\n");
                let ansi = self.ansi_now();
                if ansi.is_empty() {
                    body.push_str(
                        "\t\t\ttext \"Not set — the terminal is using its stock palette.\" \
                         style=\"note\"\n",
                    );
                } else {
                    body.push_str("\t\t\trow style=\"grid\" {\n");
                    for (name, value) in ansi {
                        body.push_str(&format!(
                            "\t\t\t\trow style=\"knob\" {{ rect width={ib} height={ib} \
                             style=\"sw-{name}\"; text \"{label}\" style=\"knob-label\"; spacer; \
                             text \"{value}\" style=\"readout\" }}\n",
                            label = name.trim_start_matches("ansi_").replace('_', " "),
                        ));
                    }
                    body.push_str("\t\t\t}\n");
                }
            }
            "window" => {
                intro(&mut body, "What a window is made of and how it sits above the desktop", "[window]");
                head(&mut body, "FOCUS AND DEPTH");
                body.push_str(
                    "\t\t\trow style=\"row\" { text \"The glow and shadow colours live on \
                     the Colours page now — one room for everything the desktop wears.\" \
                     style=\"note\"; link \"Colours\" target=\"/studio/colors\"; spacer }\n",
                );
                                let window = self.table("window");
                body.push_str("\t\t\trow style=\"grid\" {\n");
                for (key, default, ..) in WINDOW_KNOBS {
                    let value = window
                        .get(*key)
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                        .unwrap_or(*default);
                    body.push_str(&knob(&key.replace('_', " "), &format!("{value:.0}"), &format!("win/{key}")));
                }
                body.push_str("\t\t\t}\n");
                note(&mut body, "the ring around the focused window, the shadow under every one, and the chrome's own opacity — [window], live");
            }
            "dock" => {
                intro(&mut body, "The strip along the top: what it holds and what it is made of", "[desktop.dock]");
                let dock = self.dock();
                head(&mut body, "MATERIAL");
                let bg = dock.get("background").and_then(|v| v.as_str()).unwrap_or("glass");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (value, label) in DOCK_BACKGROUNDS {
                    body.push_str(&chip(
                        label,
                        bg == *value,
                        &format!("/studio/actions/dock-bg/{value}"),
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(
                    &mut body,
                    "glass: frost + tint + chrome, the same three layers a titlebar is; solid: the page colour, opaque; none: nothing at all — the mark and the clock on the wallpaper",
                );

                head(&mut body, "SHAPE");
                body.push_str("\t\t\trow style=\"grid\" {\n");
                for (key, default, ..) in DOCK_KNOBS {
                    let value = dock
                        .get(*key)
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                        .unwrap_or(*default);
                    body.push_str(&knob(key, &format!("{value:.0}"), &format!("dock-size/{key}")));
                }
                body.push_str("\t\t\t}\n");
                note(
                    &mut body,
                    "height is the compositor's: it reserves the strip, so windows follow it the moment it changes",
                );

                head(&mut body, "WHERE THINGS SIT");
                for (item, label) in DOCK_ITEMS {
                    let placed = DOCK_SLOTS
                        .iter()
                        .find(|(slot, _)| self.dock_slot(slot).iter().any(|i| i == item));
                    body.push_str("\t\t\trow style=\"row\" {\n");
                    body.push_str(&format!(
                        "\t\t\t\ttext \"{label}\" style=\"knob-label\"\n"
                    ));
                    for (slot, slot_label) in DOCK_SLOTS {
                        let on = placed.map(|(s, _)| s) == Some(slot);
                        body.push_str(&chip(
                            slot_label,
                            on,
                            &format!("/studio/actions/dock-place/{item}/{slot}"),
                        ));
                    }
                    body.push_str(&chip(
                        "Off",
                        placed.is_none(),
                        &format!("/studio/actions/dock-place/{item}/off"),
                    ));
                    // Priority within a slot is position, so ordering is a
                    // nudge rather than a number.
                    body.push_str(&format!(
                        "\t\t\t\tbutton icon=\"chevron-left\" style=\"stepper\" {{ submit \"/studio/actions/dock-move/{item}/back\" }}\n\
                         \t\t\t\tbutton icon=\"chevron-right\" style=\"stepper\" {{ submit \"/studio/actions/dock-move/{item}/fwd\" }}\n"
                    ));
                    body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                }
                note(&mut body, "order within a slot is priority — nudge an item to move it along");

                head(&mut body, "CLOCK");
                let style = dock.get("clock").and_then(|v| v.as_str()).unwrap_or("24h");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (value, label) in [("24h", "24 hour"), ("12h", "12 hour"), ("off", "Off")] {
                    body.push_str(&chip(
                        label,
                        style == value,
                        &format!("/studio/actions/dock-clock/{value}"),
                    ));
                }
                let date_on = dock.get("clock_date").and_then(|v| v.as_bool()).unwrap_or(false);
                body.push_str(&chip("Date", date_on, "/studio/actions/dock-date"));
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(&mut body, "the strip is a document, so it redraws when the minute turns and not once between");

                head(&mut body, "RESET");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("Dock reset", false, "/studio/actions/dock-reset"));
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(&mut body, "[desktop.dock] — the strip wears the window chrome colour, so the Colors and Window sections dress it too");
            }
            "cursor" => {
                intro(&mut body, "The pointer the compositor draws itself", "[cursor]");
                let cur = self.table("cursor");
                let drawn = cur.get("draw").and_then(|v| v.as_bool()).unwrap_or(true);
                head(&mut body, "POINTER");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("Drawn", drawn, "/studio/actions/cursor-draw"));
                body.push_str(&chip("Reset", false, "/studio/actions/cursor-reset"));
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(
                    &mut body,
                    "drawn: the compositor paints the pointer as geometry, so it scales and takes these colours; off: the host's own bitmap cursor",
                );
                head(&mut body, "COLOURS");
                body.push_str(
                    "\t\t\trow style=\"row\" { text \"The cursor's colours live on the \
                     Colours page now.\" style=\"note\"; \
                     link \"Colours\" target=\"/studio/colors\"; spacer }\n",
                );
                head(&mut body, "SIZE AND DEPTH");
                body.push_str("\t\t\trow style=\"grid\" {\n");
                for (key, default, ..) in CURSOR_KNOBS {
                    let value = cur
                        .get(*key)
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                        .unwrap_or(*default);
                    body.push_str(&knob(key, &format!("{value:.0}"), &format!("cur/{key}")));
                }
                body.push_str("\t\t\t}\n");
                note(&mut body, "[cursor] — the arrow, the text beam and the resize arrows all wear it");
            }
            "showroom" => {
                intro(&mut body, "The 3D object on the desktop, and the room it stands in", "[desktop.showroom]");
                head(&mut body, "MODEL");
                let choices = self.model_choices();
                let current = self.showroom().get("model").and_then(|v| v.as_str()).map(str::to_string);
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("None", current.is_none(), "/studio/actions/model/none"));
                for (i, (label, path)) in choices.iter().enumerate() {
                    body.push_str(&chip(
                        label,
                        current.as_deref() == Some(path.as_str()),
                        &format!("/studio/actions/model/{i}"),
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                if choices.is_empty() {
                    note(&mut body, "drop .obj files in ~/.config/rill/models to choose them here");
                } else {
                    note(&mut body, "the model belongs to this scene: it shows while the showroom is the wallpaper, and steps aside for any other");
                }

                head(&mut body, "SCENE COLOURS");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (name, _) in SHOWROOM_COLORS {
                    let t = format!("sr:{name}");
                    body.push_str(&chip(&name.replace('_', " "), t == target, &format!("/studio/actions/target/{t}")));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                body.push_str(&picker_grid(&cells));
                let sr = self.showroom();
                head(&mut body, "LIGHTS, SPIN, CAMERA");
                body.push_str("\t\t\trow style=\"grid\" {\n");
                for (key, default, ..) in SHOWROOM_KNOBS {
                    let value = sr
                        .get(*key)
                        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                        .unwrap_or(*default);
                    body.push_str(&knob(&key.replace('_', " "), &format!("{value:.2}"), &format!("sr/{key}")));
                }
                body.push_str("\t\t\t}\n");
                let fill_on = sr.get("fill").and_then(|v| v.as_bool()).unwrap_or(true);
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("Fill light", fill_on, "/studio/actions/sr-fill"));
                body.push_str(&chip("Reverse spin", false, "/studio/actions/sr-reverse"));
                body.push_str(&chip("Scene reset", false, "/studio/actions/sr-reset"));
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                note(&mut body, "[desktop.showroom] — the room and the model read the same table");
            }
            "rices" => {
                intro(&mut body, "A whole desktop, saved as a file. Ctrl+Shift+R cycles them", "~/.config/rill/rices");
                // A rice is the whole desktop as one file: saving copies
                // theme.toml aside under a name, loading copies it back.
                // Nothing here knows what a theme *contains*, which is why
                // this page needs no maintenance when the theme grows.
                let config = self.theme_path.parent().unwrap_or(Path::new("."));
                let saved = rill_appkit::rices::list(config);
                let current = rill_appkit::rices::current(config, &self.theme_path);

                head(&mut body, "SAVE THIS DESKTOP");
                // Inline nodes need `;` between them — the same shape the
                // exact-colour rows use.
                body.push_str(
                    "\t\t\trow style=\"knob\" { \
                     text_input bind=\"rice-name\" style=\"field\" placeholder=\"name it\" { \
                     submit \"/studio/actions/rice/save\" { field \"name\" from=\"rice-name\" } }; \
                     spacer; \
                     button \"Save\" style=\"chip\" { \
                     submit \"/studio/actions/rice/save\" { field \"name\" from=\"rice-name\" } } }\n",
                );

                head(&mut body, "SAVED LOOKS");
                if saved.is_empty() {
                    note(&mut body, "Nothing saved yet. Save one above; Ctrl+Shift+R then cycles them.");
                } else {
                    // A card per look: a thumbnail painted from the rice's
                    // own colours (the file is the truth about what loading
                    // it would look like), the name as the load control,
                    // and a right-click menu for the rest. Only names that
                    // are their own sanitized form get cards — a hand-
                    // dropped file with a strange name is skipped rather
                    // than escaped into the page.
                    body.push_str("\t\t\trow style=\"wrap\" {\n");
                    for (i, name) in saved.iter().enumerate() {
                        if rill_appkit::rices::sanitize(name).as_deref() != Some(name.as_str()) {
                            continue;
                        }
                        let Some(rice_path) = rill_appkit::rices::path(config, name) else {
                            continue;
                        };
                        let (bg, chrome_c, surface_c, accent_c) = look_swatches(&rice_path);
                        styles.push_str(&format!(
                            "style \"lk-bg-{i}\" background=\"{bg}\" width=128 height=84 corner=4 padding=10 gap=0\n\
                             style \"lk-chrome-{i}\" background=\"{chrome_c}\" corner=0\n\
                             style \"lk-surface-{i}\" background=\"{surface_c}\" corner=0\n\
                             style \"lk-accent-{i}\" background=\"{accent_c}\" corner=2\n",
                        ));
                        let card = match current.as_deref() == Some(name.as_str()) {
                            true => "look-card--current",
                            false => "look-card",
                        };
                        let context = rill_appkit::menu(&[
                            rill_appkit::MenuEntry::Item {
                                label: "Load",
                                icon: Some("refresh"),
                                danger: false,
                                wire: rill_appkit::MenuWire::Action(&rill_appkit::submit(
                                    &format!("/studio/actions/rice/load/{name}"),
                                    "",
                                )),
                            },
                            rill_appkit::MenuEntry::Item {
                                label: "Rename…",
                                icon: Some("pencil"),
                                danger: false,
                                wire: rill_appkit::MenuWire::Action(&rill_appkit::submit(
                                    &format!("/studio/actions/rice/rename-target/{name}"),
                                    "",
                                )),
                            },
                            rill_appkit::MenuEntry::Separator,
                            rill_appkit::MenuEntry::Item {
                                label: "Delete",
                                icon: Some("trash"),
                                danger: true,
                                wire: rill_appkit::MenuWire::Action(&rill_appkit::submit(
                                    &format!("/studio/actions/rice/delete/{name}"),
                                    "",
                                )),
                            },
                        ]);
                        // The thumbnail: two mock windows (chrome strip over
                        // a body) and a dock hint, all rects — enough to
                        // read a palette at a glance without pretending to
                        // be a screenshot.
                        body.push_str(&format!(
                            "\t\t\t\tcolumn style=\"{card}\" {{\n\
                             \t\t\t\t\tcolumn style=\"lk-bg-{i}\" {{\n\
                             \t\t\t\t\t\trow style=\"lk-row\" {{\n\
                             \t\t\t\t\t\t\tcolumn style=\"lk-winbox\" {{ rect width=46 height=7 style=\"lk-chrome-{i}\"; rect width=46 height=24 style=\"lk-surface-{i}\" }}\n\
                             \t\t\t\t\t\t\tcolumn style=\"lk-winbox\" {{ rect width=34 height=6 style=\"lk-chrome-{i}\"; rect width=34 height=17 style=\"lk-surface-{i}\" }}\n\
                             \t\t\t\t\t\t}}\n\
                             \t\t\t\t\t\tspacer\n\
                             \t\t\t\t\t\trow {{ spacer; rect width=34 height=4 style=\"lk-accent-{i}\"; spacer }}\n\
                             \t\t\t\t\t}}\n\
                             \t\t\t\t\tbutton {label} style=\"look-name\" {{ {load} }}\n\
                             \t\t\t\t\t{context}\n\
                             \t\t\t\t}}\n",
                            label = kdl_escape(name),
                            load = rill_appkit::submit(
                                &format!("/studio/actions/rice/load/{name}"),
                                "",
                            ),
                        ));
                    }
                    body.push_str("\t\t\t}\n");
                }
                let renaming = self
                    .rename
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default()
                    .filter(|n| saved.iter().any(|s| s == n));
                if let Some(old) = renaming {
                    extra_states.push_str(&format!(
                        "state \"rice-rename\" initial={}\n",
                        kdl_escape(&old),
                    ));
                    body.push_str(&format!(
                        "\t\t\trow style=\"knob\" {{ \
                         text {} style=\"knob-label\"; \
                         text_input bind=\"rice-rename\" style=\"field\" placeholder=\"new name\" {{ \
                         submit \"/studio/actions/rice/rename\" {{ field \"name\" from=\"rice-rename\" }} }}; \
                         spacer; \
                         button \"Rename\" style=\"chip\" {{ \
                         submit \"/studio/actions/rice/rename\" {{ field \"name\" from=\"rice-rename\" }} }}; \
                         button \"Cancel\" style=\"chip\" {{ submit \"/studio/actions/rice/rename-cancel\" }} }}\n",
                        kdl_escape(&format!("rename \u{201c}{old}\u{201d} to")),
                    ));
                }
                note(
                    &mut body,
                    "Click a look to load it; right-click a card to rename or delete. Ctrl+Shift+R cycles them in name order.",
                );
            }
            "widgets" => {
                intro(&mut body, "Small always-on windows the theme places. Drag one to move it", "[[desktop.widgets]]");
                // `[[desktop.widgets]]` is an array of tables inside
                // `[desktop]`, so it is read and written through the same
                // one-table edit every other page uses.
                let desktop = self.table("desktop");
                let widgets = desktop
                    .get("widgets")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                head(&mut body, "ON THE DESKTOP");
                if widgets.is_empty() {
                    note(&mut body, "No widgets yet. Add one below; drag it anywhere once it appears.");
                } else {
                    for (i, w) in widgets.iter().enumerate() {
                        let t = w.as_table().cloned().unwrap_or_default();
                        let app = t.get("app").and_then(|v| v.as_str()).unwrap_or("?");
                        let name = app.rsplit('/').next().unwrap_or(app);
                        let anchor = t.get("anchor").and_then(|v| v.as_str()).unwrap_or("top-left");
                        let num = |k: &str| {
                            t.get(k).and_then(|v| v.as_integer()).unwrap_or(0)
                        };
                        // Name, then the two things worth changing from here:
                        // where it is anchored, and whether it exists. Size
                        // and offset are set by dragging and resizing it,
                        // which is a better way to choose them than typing.
                        body.push_str(&format!(
                            "\t\t\trow style=\"knob\" {{ text \"{}\" style=\"knob-label\"; \
                             text \"{}x{}\" style=\"note\"; spacer; }}\n",
                            kdl_escape(name).trim_matches('"'),
                            num("width"),
                            num("height"),
                        ));
                        body.push_str("\t\t\trow style=\"wrap\" {\n");
                        for a in ANCHORS {
                            body.push_str(&chip(
                                a,
                                *a == anchor,
                                &format!("/studio/actions/widget/anchor/{i}/{a}"),
                            ));
                        }
                        body.push_str(&chip("Remove", false, &format!("/studio/actions/widget/remove/{i}")));
                        body.push_str("\t\t\t}\n");
                    }
                }

                head(&mut body, "ADD");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (label, kind) in [("Meter", "meter"), ("ASCII", "ascii")] {
                    body.push_str(&chip(label, false, &format!("/studio/actions/widget/add/{kind}")));
                }
                body.push_str("\t\t\t}\n");

                // The ASCII widget's own source, since it is the one with a
                // choice worth making: three built-in generators, a folder
                // of frames, or a .gif to loop.
                let ascii = self.table("desktop");
                let ascii = ascii
                    .get("ascii")
                    .and_then(|v| v.as_table())
                    .cloned()
                    .unwrap_or_default();
                let art = ascii.get("art").and_then(|v| v.as_str()).unwrap_or("cube");
                head(&mut body, "ASCII SOURCE");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for name in ["cube", "wave", "plasma"] {
                    body.push_str(&chip(name, name == art, &format!("/studio/actions/ascii/art/{name}")));
                }
                body.push_str("\t\t\t}\n");
                body.push_str(
                    "\t\t\trow style=\"knob\" { \
                     text_input bind=\"ascii-file\" style=\"field\" placeholder=\"a .gif, or a folder of .txt frames\" { \
                     submit \"/studio/actions/ascii/file\" { field \"path\" from=\"ascii-file\" } }; \
                     spacer; \
                     button \"Use\" style=\"chip\" { \
                     submit \"/studio/actions/ascii/file\" { field \"path\" from=\"ascii-file\" } } }\n",
                );
                note(
                    &mut body,
                    "A .gif is decoded once and looped at its own speed. Widgets drag to move.",
                );
            }
            "effects" => {
                intro(&mut body, "What the compositor draws over and around the desktop", "[desktop]");
                let desktop = self.table("desktop");
                let on = |key: &str| desktop.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
                head(&mut body, "EFFECTS");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                for (label, key, is_on) in [
                    ("Glass", "glass", on("glass")),
                    ("Stats", "hud", on("hud")),
                    ("Override", "enforce", on("enforce")),
                ] {
                    body.push_str(&chip(label, is_on, &format!("/studio/actions/desk/{key}")));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                let current_shader =
                    desktop.get("shader").and_then(|v| v.as_str()).unwrap_or("").to_string();
                head(&mut body, "SCREEN EFFECT");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("off", current_shader.is_empty(), "/studio/actions/shader/off"));
                for (stem, path) in effect_choices() {
                    let sel = current_shader == path.display().to_string();
                    body.push_str(&chip(
                        &wall_label(&stem),
                        sel,
                        &format!("/studio/actions/shader/{stem}"),
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                self.shader_param_ui("fx", &current_shader, &mut body, &mut extra_states);
                // Particles: a simulation that runs over the whole desktop
                // and reacts to where the windows are.
                head(&mut body, "PARTICLES");
                // One control for the whole particle system, because two —
                // a Boids toggle and a set picker, writing different keys —
                // is how "I could not turn it off" happens.
                let count = desktop
                    .get("particles")
                    .or_else(|| desktop.get("boids"))
                    .and_then(|v| v.as_integer())
                    .unwrap_or(0);
                let set_name = desktop
                    .get("particle_shader")
                    .and_then(|v| v.as_str())
                    .and_then(|p| Path::new(p).file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .map(|stem| stem.trim_end_matches("_update").to_string());
                let selected = match (count > 0, set_name.as_deref()) {
                    (false, _) => "off".to_string(),
                    (true, None) => "flock".to_string(),
                    (true, Some(name)) => name.to_string(),
                };
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("off", selected == "off", "/studio/actions/particles/off"));
                body.push_str(&chip("flock", selected == "flock", "/studio/actions/particles/flock"));
                for set in particle_sets() {
                    body.push_str(&chip(
                        &wall_label(&set.name),
                        set.name == selected,
                        &format!("/studio/actions/particles/{}", set.name),
                    ));
                }
                body.push_str("\t\t\t}\n");
                note(&mut body, "off stops the simulation entirely. A set is <name>_update.wgsl plus optional _diffuse and _draw.");

                // Per-window effects: drawn at each window's own z, so glass
                // in front of one blurs it.
                head(&mut body, "WINDOW EFFECT");
                let current_winfx_path =
                    desktop.get("window_shader").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let current_winfx = Path::new(&current_winfx_path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("off", current_winfx.is_empty(), "/studio/actions/winfx/off"));
                for (stem, _) in window_fx_choices() {
                    body.push_str(&chip(
                        &wall_label(stem.trim_start_matches("window_")),
                        stem == current_winfx,
                        &format!("/studio/actions/winfx/{stem}"),
                    ));
                }
                body.push_str("\t\t\t}\n");
                self.shader_param_ui("winfx", &current_winfx_path, &mut body, &mut extra_states);
            }
            "background" => {
                intro(&mut body, "A colour is the floor, an image covers it, a shader covers both", "[desktop]");
                let desktop = self.table("desktop");
                let current_wall = desktop
                    .get("background_shader")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // Picking one clears whatever it would be hidden behind, so
                // the thing you chose is always the thing you see.
                let bg_color = desktop
                    .get("background_color")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let wall_img =
                    desktop.get("wallpaper").and_then(|v| v.as_str()).unwrap_or("").to_string();
                extra_states.push_str(&format!(
                    "state \"bg-hex\" initial={}\nstate \"bg-img\" initial=\"\"\n",
                    kdl_escape(&bg_color),
                ));
                head(&mut body, "BACKGROUND COLOUR");
                body.push_str(&picker_grid_for(&cells, "bg-swatch"));
                let bg_readout = match bg_color.is_empty() {
                    true => "#0e1020 (default)".to_string(),
                    false => bg_color.clone(),
                };
                body.push_str(&format!(
                    "\t\t\trow style=\"row\" {{ rect width={ib} height={ib} style=\"sw-bg\"; \
                     text \"{bg_readout}\" style=\"readout\"; spacer; \
                     text_input bind=\"bg-hex\" style=\"field\" placeholder=\"#rrggbb\" {{ \
                     submit \"/studio/actions/bg/hex\" {{ field \"value\" from=\"bg-hex\" }} }} }}\n",
                ));
                note(&mut body, "the desktop's floor: it shows bare, and picking it turns image and shader off");

                head(&mut body, "BACKGROUND IMAGE");
                let (images, capped) = image_choices();
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("off", wall_img.is_empty(), "/studio/actions/bg/img-off"));
                for (stem, path) in &images {
                    body.push_str(&chip(
                        &wall_label(stem),
                        wall_img == path.display().to_string(),
                        &format!("/studio/actions/bg/img/{stem}"),
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                body.push_str(
                    "\t\t\trow style=\"knob\" {\n\
                     \t\t\t\ttext_input bind=\"bg-img\" style=\"field\" placeholder=\"path to a .jpg or .png\u{2026}\" { \
                     submit \"/studio/actions/bg/image\" { field \"path\" from=\"bg-img\" } }\n\
                     \t\t\t\tspacer\n\
                     \t\t\t\tbutton \"Use\" style=\"chip\" { \
                     submit \"/studio/actions/bg/image\" { field \"path\" from=\"bg-img\" } }\n\
                     \t\t\t}\n",
                );
                match capped {
                    true => note(&mut body, &format!(
                        "chips are ~/.config/rill/wallpapers plus ~/Pictures — first {IMAGE_CHOICE_CAP} shown, the path box reaches the rest"
                    )),
                    false => note(&mut body, "chips are ~/.config/rill/wallpapers plus ~/Pictures; the path box takes anything"),
                }

                head(&mut body, "BACKGROUND SHADER");
                body.push_str("\t\t\trow style=\"wrap\" {\n");
                body.push_str(&chip("off", current_wall.is_empty(), "/studio/actions/wall/off"));
                for (stem, path) in wall_choices() {
                    let sel = current_wall == path.display().to_string();
                    body.push_str(&chip(
                        &wall_label(&stem),
                        sel,
                        &format!("/studio/actions/wall/{stem}"),
                    ));
                }
                body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
                self.shader_param_ui("bg", &current_wall, &mut body, &mut extra_states);
                note(&mut body, "The dock launches apps; this page dresses the desktop they sit on.");
            }
            other => {
                // Unreachable while SECTIONS gates `get`, and deliberately
                // loud rather than silently rendering some other page.
                head(&mut body, "NOT BUILT");
                note(&mut body, &format!("No page is written for section {other:?}."));
            }
        }
        body.push_str("\t\t\tspacer\n");

        // --- the shell ----------------------------------------------------
        let mut states = String::new();
        // The rice-name field's buffer. Declared for every section so the
        // shell's state table has a stable shape whichever page is showing.
        states.push_str("state \"rice-name\" initial=\"\"\n");
        states.push_str("state \"ascii-file\" initial=\"\"\n");
        states.push_str(&extra_states);
        for (token, _) in TOKENS {
            states.push_str(&format!(
                "state \"hex-{token}\" initial={}\n",
                kdl_escape(&self.token_value(&colors, token)),
            ));
        }
        let places: Vec<rill_appkit::Place> = SECTIONS
            .iter()
            .map(|(slug, label, icon)| rill_appkit::Place {
                label: (*label).into(),
                target: format!("/studio/{slug}"),
                icon: (*icon).into(),
                current: *slug == section,
            })
            .collect();
        let title = SECTIONS
            .iter()
            .find(|(slug, ..)| *slug == section)
            .map(|(_, label, _)| *label)
            .unwrap_or("Studio");
        let titlebar = rill_appkit::sidebar_header(&rill_appkit::location_title("Theme Studio"))
            + &rill_appkit::toolbar(
                &(rill_appkit::location_title(title)
                    + "\t\t\t\tspacer\n"
                    + &rill_appkit::close_button()),
            );
        let kdl = rill_appkit::shell(&rill_appkit::Shell {
            metrics: m,
            states: &states,
            titlebar: &titlebar,
            places: &places,
            footer: None,
            sidebar_top_gap: m.sidebar_align_gap() as u32,
            extra_styles: &styles,
            content_style: None,
            body: &body,
            rail_body: None,
            scroll_content: true,
        });

        // STUDIO_DUMP_KDL is the failure-only dump, kept separate from
        // RILL_DUMP_KDL (which `compile_page` honours and writes every time):
        // this page is the largest generated in the tree, and when it breaks
        // the useful artifact is the one that broke, not every one before it.
        rill_appkit::compile_page("studio-app", &kdl).inspect_err(|_| {
            if let Ok(dump) = std::env::var("STUDIO_DUMP_KDL") {
                let _ = std::fs::write(dump, &kdl);
            }
        })
    }

    /// Step a `[metrics]` number, clamped.
    fn step_metric(&self, key: &str, step: f32, range: (f32, f32)) -> Result<(), Status> {
        let m = rill_appkit::Metrics::from_theme_file(&self.theme_path);
        let current = match key {
            "font_size" => m.font_size,
            _ => m.padding,
        };
        let next = (current + step).clamp(range.0, range.1);
        self.update_table("metrics", |t| {
            t.insert(key.to_string(), toml::Value::Float(next as f64));
        })
    }

    /// The weight monospaced surfaces ask for. Its own step because it is
    /// its own decision: the terminal and the widgets can be a step heavier
    /// than the body type they sit beside, and on this font family what a
    /// mono surface looks like is decided here rather than by the face.
    fn step_mono(&self, step: i32) -> Result<(), Status> {
        let current = rill_appkit::Metrics::from_theme_file(&self.theme_path).mono_weight as i32;
        let next = (current + step).clamp(200, 900);
        self.update_table("metrics", move |t| {
            t.insert("mono_weight".to_string(), toml::Value::Integer(next as i64));
        })
    }

    fn write_density(&self, f: f32, p: f32) -> Result<(), Status> {
        self.update_table("metrics", |t| {
            t.insert("font_size".to_string(), toml::Value::Float(f as f64));
            t.insert("padding".to_string(), toml::Value::Float(p as f64));
        })
    }

    /// The `[desktop]` key an effect role reads its shader path from.
    fn role_shader_key(role: &str) -> Option<&'static str> {
        match role {
            "bg" => Some("background_shader"),
            "fx" => Some("shader"),
            "winfx" => Some("window_shader"),
            _ => None,
        }
    }

    /// A shader path as theme.toml spells it, made absolute the way the
    /// compositor resolves it (`~/` expanded, relative against the config
    /// directory) so both sides read the same file.
    fn resolve_shader_path(&self, spec: &str) -> PathBuf {
        if let Some(rest) = spec.strip_prefix("~/")
            && let Some(home) = std::env::var_os("HOME")
        {
            return Path::new(&home).join(rest);
        }
        let p = Path::new(spec);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.theme_path.parent().unwrap_or(Path::new(".")).join(p)
        }
    }

    /// One slider per `// @param` line of the role's active shader — the
    /// declaration supplies label, range, default and blurb; theme.toml
    /// supplies the current value. Nothing declared, nothing drawn.
    fn shader_param_ui(&self, role: &str, spec: &str, body: &mut String, states: &mut String) {
        if spec.is_empty() {
            return;
        }
        let path = self.resolve_shader_path(spec);
        let Ok(src) = std::fs::read_to_string(&path) else { return };
        let decls = rill_appkit::params::shader_params(&src);
        if decls.is_empty() {
            return;
        }
        let stem =
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let stored = self
            .table("desktop")
            .get("shader_params")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get(&stem))
            .and_then(|v| v.as_table())
            .cloned()
            .unwrap_or_default();
        for d in &decls {
            let value = stored
                .get(&d.name)
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .map(|v| (v as f32).clamp(d.min, d.max))
                .unwrap_or(d.default);
            let slot = format!("sp-{role}-{}", d.name);
            states.push_str(&format!("state \"{slot}\" initial={value}\n"));
            body.push_str(&format!(
                "\t\t\trow style=\"knob\" {{ text {label} style=\"knob-label\"; spacer; \
                 slider bind=\"{slot}\" min={min} max={max} style=\"param\" {{ \
                 submit \"/studio/actions/shaderparam/{role}/{name}\" {{ \
                 field \"value\" from=\"{slot}\" }} }}; \
                 text \"{value:.2}\" style=\"readout\" }}\n",
                label = kdl_escape(&d.name.replace('_', " ")),
                min = d.min,
                max = d.max,
                name = d.name,
            ));
            if !d.doc.is_empty() {
                body.push_str(&format!("\t\t\ttext {} style=\"note\"\n", kdl_escape(&d.doc)));
            }
        }
    }

    /// Store one declared parameter's value in
    /// `[desktop.shader_params.<stem>]`, clamped to the shader's declared
    /// range. The declaration is re-read from the shader at write time —
    /// the file is the authority on what exists and what is legal.
    fn set_shader_param(&self, role: &str, name: &str, value: f64) -> Result<(), Status> {
        let key = Self::role_shader_key(role).ok_or(Status::NotFound)?;
        let spec =
            self.table("desktop").get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
        if spec.is_empty() {
            return Err(Status::NotFound);
        }
        let path = self.resolve_shader_path(&spec);
        let src = std::fs::read_to_string(&path).map_err(|_| Status::NotFound)?;
        let decl = rill_appkit::params::shader_params(&src)
            .into_iter()
            .find(|d| d.name == name)
            .ok_or(Status::NotFound)?;
        let clamped = value.clamp(decl.min as f64, decl.max as f64);
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or(Status::NotFound)?;
        self.update_table("desktop", |t| {
            let params =
                t.entry("shader_params").or_insert(toml::Value::Table(Default::default()));
            if let Some(params) = params.as_table_mut() {
                let entry = params.entry(stem).or_insert(toml::Value::Table(Default::default()));
                if let Some(entry) = entry.as_table_mut() {
                    entry.insert(name.to_string(), toml::Value::Float(clamped));
                }
            }
        })
    }
}

/// One knob: a label, a value, and a minus/plus pair — laid out so the
/// labels share a measure group and every column lines up across the grid.
fn knob(label: &str, value: &str, path: &str) -> String {
    format!(
        "\t\t\t\trow style=\"knob\" {{ \
         text {} style=\"knob-label\"; spacer; \
         button icon=\"minus\" style=\"stepper\" {{ submit \"/studio/actions/{path}/down\" }}; \
         text \"{value}\" style=\"readout\"; \
         button icon=\"plus\" style=\"stepper\" {{ submit \"/studio/actions/{path}/up\" }} }}\n",
        kdl_escape(label),
    )
}

/// A chip: on-state is the accent fill, per the kit's rule that state is a
/// style, not a different control.
fn chip(label: &str, on: bool, action: &str) -> String {
    format!(
        "\t\t\t\tbutton {} style=\"{}\" {{ submit \"{action}\" }}\n",
        kdl_escape(label),
        if on { "chip--on" } else { "chip" },
    )
}

/// The colour grid: hue bands plus a grey ramp, one button per cell.
fn picker_grid(cells: &[String]) -> String {
    picker_grid_for(cells, "pick")
}

/// The composed picker: everything a "real" colour picker has, made only
/// of nodes every app already speaks — buttons, sliders, a text input, a
/// `when`. This is the vocabulary-gate evidence (lineage.md): if this
/// version fails the feel test on the live desktop, a dedicated wheel node
/// is admitted on the strength of the attempt; until then, the platform
/// stays smaller.
///
/// Shape: a header row that is always there — big swatch, hex field, an
/// unfold button — and, unfolded, a hue bar, a saturation/lightness field
/// at the current hue, three sliders that say exactly what the bar and
/// field say, an alpha slider, and the recent picks. The unfold state is
/// client-side (`toggle`), so opening it costs nothing and it stays open
/// across the refresh every edit causes.
struct PickerRender {
    body: String,
    styles: String,
    states: String,
}

fn color_picker(current_hex: &str, alpha: u8, recent: &[String]) -> PickerRender {
    // Cell geometry, chosen so the whole panel reads as one compact
    // instrument: the hue bar and the field the same width (24×14 = 14×24),
    // every cell flat, square-cornered, unpadded.
    // One instrument, one width: 24 hue stops × 14px = 14 field columns
    // × 24px = the sliders' length = 336. Gapless rows, so the field reads
    // as a canvas rather than a tray of chiclets.
    const HUE_W: u32 = 14;
    const FIELD_W: u32 = 24;
    let rgb = current_hex.trim_start_matches('#');
    let c = rill_doc::Color::parse_hex(&format!("#{}", &rgb[..6.min(rgb.len())]))
        .unwrap_or(rill_doc::Color { r: 0x80, g: 0x80, b: 0x80, a: 0xff });
    let (h, sat, l) = rgb_to_hsl(c);

    let mut body = String::new();
    let mut styles = String::new();
    let mut states = String::new();

    states.push_str("state \"picker-open\" initial=#true\n");
    states.push_str(&format!("state \"pick-hex\" initial=\"#{}\"\n", &rgb[..6.min(rgb.len())]));
    states.push_str(&format!("state \"pick-h\" initial={}\n", h.round()));
    states.push_str(&format!("state \"pick-s\" initial={}\n", (sat * 100.0).round()));
    states.push_str(&format!("state \"pick-l\" initial={}\n", (l * 100.0).round()));
    states.push_str(&format!(
        "state \"pick-a\" initial={}\n",
        (alpha as f32 / 255.0 * 100.0).round()
    ));

    // Header: the colour as it is, the hex as text, the unfold.
    styles.push_str(&format!(
        "style \"pick-now\" background=\"#{}\" corner=4\n",
        &rgb[..6.min(rgb.len())]
    ));
    body.push_str(&format!(
        "\t\t\trow gap=8 padding=0 {{\n\
         \t\t\t\trect width=34 height=22 style=\"pick-now\"\n\
         \t\t\t\ttext_input bind=\"pick-hex\" style=\"hexfield\" placeholder=\"#rrggbb\" {{ \
         submit \"/studio/actions/pickhex\" {{ field \"value\" from=\"pick-hex\" }} }}\n\
         \t\t\t\ttext \"{pct}%\" style=\"knob-label\"\n\
         \t\t\t\tbutton icon=\"chevron-down\" style=\"stepper\" {{ toggle \"picker-open\" }}\n\
         \t\t\t\tspacer\n\
         \t\t\t}}\n",
        pct = (alpha as u32 * 100 + 127) / 255,
    ));

    // The unfolded panel.
    body.push_str("\t\t\twhen \"picker-open\" {\n\t\t\tcolumn gap=3 padding=0 {\n");

    // Hue bar: the wheel, unrolled. Twenty-four stops reads as continuous
    // at swatch size; the slider under it is the fine adjustment.
    body.push_str("\t\t\trow style=\"pickrow\" {\n");
    for i in 0..24 {
        let hh = i as f32 * 15.0;
        let cell = hsl_hex(hh, 0.85, 0.55);
        styles.push_str(&format!(
            "style \"ph-{i}\" background=\"#{cell}\" corner=0 padding=0 \
             width={HUE_W} size=14\n"
        ));
        body.push_str(&format!(
            "\t\t\t\tbutton \"\" style=\"ph-{i}\" {{ submit \"/studio/actions/hue/{}\" }}\n",
            hh as u32
        ));
    }
    body.push_str("\t\t\t}\n");

    // Saturation/lightness field at the current hue — the face of the
    // wheel. Lightness falls row by row, saturation grows left to right.
    body.push_str("\t\t\tcolumn gap=0 padding=0 {\n");
    for (row, ll) in [88u32, 76, 64, 55, 46, 37, 28, 18].iter().enumerate() {
        body.push_str("\t\t\trow style=\"pickrow\" {\n");
        for col in 0..14 {
            let ss = 0.04 + col as f32 * (0.96 / 13.0);
            let cell = hsl_hex(h, ss, *ll as f32 / 100.0);
            styles.push_str(&format!(
                "style \"pf-{row}-{col}\" background=\"#{cell}\" corner=0 padding=0 \
                 width={FIELD_W} size=13\n"
            ));
            body.push_str(&format!(
                "\t\t\t\tbutton \"\" style=\"pf-{row}-{col}\" {{ submit \"/studio/actions/pick/{cell}\" }}\n"
            ));
        }
        body.push_str("\t\t\t}\n");
    }
    body.push_str("\t\t\t}\n");

    // The same colour as three numbers. Sliders commit on release; the
    // server recomputes the field above at the new hue, so the bar, the
    // field and the sliders never disagree for longer than one refresh.
    styles.push_str("style \"pick-slider\" width=316\n");
    styles.push_str("style \"pick-letter\" color=\"text-muted\" size=11 width=14\n");
    body.push_str("\t\t\tspacer size=4\n");
    for (label, bind, max) in
        [("H", "pick-h", 360), ("S", "pick-s", 100), ("L", "pick-l", 100), ("A", "pick-a", 100)]
    {
        body.push_str(&format!(
            "\t\t\trow gap=6 padding=0 {{ text \"{label}\" style=\"pick-letter\"; \
             slider bind=\"{bind}\" min=0 max={max} step=1 style=\"pick-slider\" {{ \
             submit \"/studio/actions/hsl\" {{ field \"h\" from=\"pick-h\"; \
             field \"s\" from=\"pick-s\"; field \"l\" from=\"pick-l\"; \
             field \"a\" from=\"pick-a\" }} }} }}\n"
        ));
    }

    // What was picked lately: the working palette.
    if !recent.is_empty() {
        body.push_str("\t\t\trow style=\"swatchrow\" {\n");
        body.push_str("\t\t\t\ttext \"recent\" style=\"knob-label\"\n");
        for (i, rgb) in recent.iter().enumerate() {
            styles.push_str(&format!(
                "style \"pr-{i}\" background=\"#{rgb}\" corner=2 padding=0 \
                 width=20 size=11\n"
            ));
            body.push_str(&format!(
                "\t\t\t\tbutton \"\" style=\"pr-{i}\" {{ submit \"/studio/actions/pick/{rgb}\" }}\n"
            ));
        }
        body.push_str("\t\t\t\tspacer\n\t\t\t}\n");
    }

    body.push_str("\t\t\t}\n\t\t\t}\n");
    PickerRender { body, styles, states }
}

/// The handful of colours a saved look's thumbnail is painted from —
/// (background, chrome, surface, accent) — read straight off the rice's
/// own file: the file is the truth about what loading it would look like.
/// Missing or malformed values fall back to the stock palette, so a sparse
/// rice still draws a plausible card.
fn look_swatches(path: &Path) -> (String, String, String, String) {
    let table = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .unwrap_or_default();
    let color = |home: &str, key: &str, fallback: &str| {
        table
            .get(home)
            .and_then(|t| t.as_table())
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_str())
            .filter(|s| is_hex_color(s))
            .unwrap_or(fallback)
            .to_string()
    };
    let page = color("colors", "page", "#10121e");
    (
        color("desktop", "background_color", &page),
        color("colors", "chrome", "#232634"),
        color("colors", "surface", "#1a1c2b"),
        color("colors", "accent", "#6ea8ff"),
    )
}

/// The same grid aimed at a different action — the background colour uses
/// it with its own verb, so one grid serves both without a target dance.
fn picker_grid_for(cells: &[String], action: &str) -> String {
    let mut out = String::new();
    for row in 0..=GRID_LIGHTNESS.len() {
        let slice = &cells[row * GRID_HUES..((row + 1) * GRID_HUES).min(cells.len())];
        if slice.is_empty() {
            break;
        }
        out.push_str("\t\t\trow style=\"swatchrow\" {\n");
        for c in slice {
            out.push_str(&format!(
                "\t\t\t\tbutton \" \" style=\"cx-{c}\" {{ submit \"/studio/actions/{action}/{c}\" }}\n"
            ));
        }
        out.push_str("\t\t\t}\n");
    }
    out
}


fn is_hex_color(s: &str) -> bool {
    rill_doc::Color::parse_hex(s).is_some()
}

/// The sixteen ANSI names a terminal looks for, with the hue each one is
/// expected to be. The hues are the convention every palette keeps — a
/// terminal's "red" has to still read as red — so what a rice gets to
/// change is how light, how saturated, and how far the nearest one leans
/// toward its accent.
const ANSI_ORDER: [&str; 16] = [
    "ansi-black",
    "ansi-red",
    "ansi-green",
    "ansi-yellow",
    "ansi-blue",
    "ansi-magenta",
    "ansi-cyan",
    "ansi-white",
    "ansi-bright-black",
    "ansi-bright-red",
    "ansi-bright-green",
    "ansi-bright-yellow",
    "ansi-bright-blue",
    "ansi-bright-magenta",
    "ansi-bright-cyan",
    "ansi-bright-white",
];

const ANSI_HUES: [(&str, f32); 6] =
    [("red", 0.0), ("yellow", 45.0), ("green", 130.0), ("cyan", 185.0), ("blue", 220.0), ("magenta", 290.0)];

fn rgb_to_hsl(c: rill_doc::Color) -> (f32, f32, f32) {
    let (r, g, b) = (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d.abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), s, l)
}

fn hsl_to_hex(h: f32, s: f32, l: f32) -> String {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// Mix two colours in RGB, `t` of the way from `a` to `b`.
fn mix(a: rill_doc::Color, b: rill_doc::Color, t: f32) -> String {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    format!("#{:02x}{:02x}{:02x}", f(a.r, b.r), f(a.g, b.g), f(a.b, b.b))
}

/// The sixteen, derived from the three colours a rice actually chooses.
///
/// The hues are fixed by convention — nobody wants a terminal whose "red"
/// is blue — so what is derived is everything else: the two ends come from
/// the page and the text, the saturation and lightness come from the
/// accent, and whichever hue sits closest to the accent is pulled onto it
/// so the palette reads as belonging to this rice rather than beside it.
/// The result is written into the theme, where it can be edited by hand:
/// derived, not computed forever.
fn derive_ansi(accent: rill_doc::Color, page: rill_doc::Color, text: rill_doc::Color)
-> Vec<(String, String)> {
    let (ah, asat, al) = rgb_to_hsl(accent);
    let (_, _, page_l) = rgb_to_hsl(page);
    let dark = page_l < 0.5;
    // Saturation follows the accent but is not allowed to go flat or lurid:
    // a terminal is read for hours.
    let sat = asat.clamp(0.35, 0.75);
    let (norm_l, bright_l) = if dark { (al.clamp(0.45, 0.62), al.clamp(0.62, 0.78)) }
                             else { (al.clamp(0.34, 0.46), al.clamp(0.46, 0.60)) };

    let mut out = vec![
        ("ansi-black".to_string(), mix(page, text, if dark { 0.06 } else { 0.78 })),
        ("ansi-white".to_string(), mix(text, page, 0.18)),
        ("ansi-bright-black".to_string(), mix(page, text, if dark { 0.30 } else { 0.55 })),
        ("ansi-bright-white".to_string(), mix(text, page, 0.0)),
    ];
    for (name, hue) in ANSI_HUES {
        // How far this hue is from the accent's, the short way round.
        let delta = ((hue - ah + 540.0).rem_euclid(360.0) - 180.0).abs();
        // The nearest hue leans onto the accent; the rest keep their own.
        let pull = if delta < 45.0 { 1.0 - delta / 45.0 } else { 0.0 };
        let h = hue + (ah - hue) * pull * 0.8;
        out.push((format!("ansi-{name}"), hsl_to_hex(h, sat, norm_l)));
        out.push((format!("ansi-bright-{name}"), hsl_to_hex(h, (sat * 0.92).min(1.0), bright_l)));
    }
    out
}

/// Split `#rrggbb[aa]` into the rgb part and the alpha byte (255 when absent).
fn split_alpha(hex: &str) -> (String, u8) {
    let raw = hex.trim_start_matches('#');
    match raw.len() {
        8 => (
            raw[..6].to_string(),
            u8::from_str_radix(&raw[6..], 16).unwrap_or(255),
        ),
        _ => (raw.to_string(), 255),
    }
}

/// Join rgb + alpha back into a hex token value; full alpha stays 6-digit.
fn join_alpha(rgb: &str, alpha: u8) -> String {
    if alpha == 255 {
        format!("#{rgb}")
    } else {
        format!("#{rgb}{alpha:02x}")
    }
}

/// HSL → `rrggbb` (lowercase, no '#') — the grid's cells.
fn hsl_hex(h: f32, s: f32, l: f32) -> String {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let byte = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

impl AppHandler for Studio {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        let section = match path {
            "/studio" | "/studio/" => None,
            other => Some(other.strip_prefix("/studio/")?),
        };
        if let Some(rest) = path.strip_prefix("/studio/apply/") {
            // A look card on the landing page: navigating to it applies the
            // look and lands back on Appearance. Sanitized like every other
            // rice name, and unknown names are simply not found.
            let name = rill_appkit::rices::sanitize(rest)?;
            if name != rest {
                return None;
            }
            let config = self.theme_path.parent().unwrap_or(Path::new("."));
            rill_appkit::rices::load(config, &self.theme_path, &name).ok()?;
            *self.section.lock().unwrap() = "appearance".to_string();
            return self.page().ok();
        }
        if let Some(slug) = section {
            if !SECTIONS.iter().any(|(s, ..)| *s == slug) {
                return None;
            }
            *self.section.lock().unwrap() = slug.to_string();
        }
        self.page().ok()
    }

    fn action(
        &self,
        path: &str,
        fields: &[(String, ActionValue)],
        _identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        match path {
            "/studio/actions/f/up" => self.step_metric("font_size", 1.0, F_RANGE)?,
            "/studio/actions/f/down" => self.step_metric("font_size", -1.0, F_RANGE)?,
            "/studio/actions/p/up" => self.step_metric("padding", 1.0, P_RANGE)?,
            "/studio/actions/p/down" => self.step_metric("padding", -1.0, P_RANGE)?,
            "/studio/actions/mono/up" => self.step_mono(100)?,
            "/studio/actions/mono/down" => self.step_mono(-100)?,
            "/studio/actions/density/compact" => self.write_density(14.0, 6.0)?,
            "/studio/actions/density/normal" => self.write_density(16.0, 8.0)?,
            "/studio/actions/density/spacious" => self.write_density(18.0, 10.0)?,
            "/studio/actions/reset" => {
                self.update_table("colors", |t| t.clear())?;
                self.update_table("metrics", |t| t.clear())?;
                self.update_table("window", |t| t.clear())?;
                self.update_table("desktop", |t| t.clear())?;
            }
            "/studio/actions/desk/glass" | "/studio/actions/desk/hud"
            | "/studio/actions/desk/enforce" => {
                let key = path.rsplit('/').next().unwrap_or("").to_string();
                let on = !self
                    .table("desktop")
                    .get(&key)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.update_table("desktop", move |t| {
                    if on {
                        t.insert(key, toml::Value::Boolean(true));
                    } else {
                        t.remove(&key);
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/shader/") => {
                let label = &p["/studio/actions/shader/".len()..];
                let chosen = match label {
                    "off" => None,
                    stem => Some(
                        effect_choices()
                            .into_iter()
                            .find(|(s, _)| s == stem)
                            .ok_or(Status::NotFound)?,
                    ),
                };
                self.update_table("desktop", move |t| match chosen {
                    Some((stem, path)) => {
                        t.insert("shader".into(), toml::Value::String(path.display().to_string()));
                        match EFFECT_BARRELS.iter().find(|(s, _)| *s == stem) {
                            Some((_, b)) => t.insert("warp_barrel".into(), toml::Value::Float(*b)),
                            None => t.remove("warp_barrel"),
                        };
                    }
                    None => {
                        t.remove("shader");
                        t.remove("warp_barrel");
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/shaderparam/") => {
                let rest = &p["/studio/actions/shaderparam/".len()..];
                let (role, name) = rest.split_once('/').ok_or(Status::NotFound)?;
                let value = fields
                    .iter()
                    .find(|(k, _)| k == "value")
                    .and_then(|(_, v)| match v {
                        ActionValue::Num(n) => Some(*n),
                        _ => None,
                    })
                    .ok_or(Status::NotFound)?;
                self.set_shader_param(role, name, value)?;
            }
            "/studio/actions/rice/save" => {
                let name = fields
                    .iter()
                    .find(|(k, _)| k == "name")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                let config = self.theme_path.parent().unwrap_or(Path::new("."));
                // An unnamed save is a no-op rather than an error: the button
                // sits beside an empty field, and pressing it is a normal
                // thing to do by accident.
                if rill_appkit::rices::sanitize(name).is_some() {
                    rill_appkit::rices::save(config, &self.theme_path, name)
                        .map_err(|_| Status::Internal)?;
                }
            }
            "/studio/actions/rice/update" => {
                // Overwrite the look this desktop is based on with what the
                // desktop now is — the divergence, kept.
                let config = self
                    .theme_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let base = rill_appkit::rices::last(&config).ok_or(Status::NotFound)?;
                rill_appkit::rices::save(&config, &self.theme_path, &base)
                    .map_err(|_| Status::Internal)?;
            }
            p if p.starts_with("/studio/actions/rice/load/") => {
                let name = &p["/studio/actions/rice/load/".len()..];
                let config = self.theme_path.parent().unwrap_or(Path::new("."));
                rill_appkit::rices::load(config, &self.theme_path, name)
                    .map_err(|_| Status::NotFound)?;
            }
            p if p.starts_with("/studio/actions/rice/delete/") => {
                let name = &p["/studio/actions/rice/delete/".len()..];
                let config = self.theme_path.parent().unwrap_or(Path::new("."));
                rill_appkit::rices::delete(config, name).map_err(|_| Status::Internal)?;
                // A rename aimed at the look that just vanished aims at
                // nothing; drop it rather than leave the row up.
                let mut renaming = self.rename.lock().map_err(|_| Status::Internal)?;
                if renaming.as_deref() == Some(name) {
                    *renaming = None;
                }
            }
            p if p.starts_with("/studio/actions/rice/rename-target/") => {
                let name = &p["/studio/actions/rice/rename-target/".len()..];
                let config = self.theme_path.parent().unwrap_or(Path::new("."));
                if !rill_appkit::rices::list(config).iter().any(|s| s == name) {
                    return Err(Status::NotFound);
                }
                *self.rename.lock().map_err(|_| Status::Internal)? = Some(name.to_string());
            }
            "/studio/actions/rice/rename-cancel" => {
                *self.rename.lock().map_err(|_| Status::Internal)? = None;
            }
            "/studio/actions/rice/rename" => {
                let new_name = fields
                    .iter()
                    .find(|(k, _)| k == "name")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                let old = self.rename.lock().map_err(|_| Status::Internal)?.take();
                let config = self.theme_path.parent().unwrap_or(Path::new("."));
                // A rename to nothing, to itself, or onto an existing look
                // is a quiet no-op — the same grace the save field extends.
                if let (Some(old), Some(from), Some(to)) = (
                    old.as_deref(),
                    old.as_deref().and_then(|o| rill_appkit::rices::path(config, o)),
                    rill_appkit::rices::path(config, new_name),
                ) {
                    let same = rill_appkit::rices::sanitize(new_name).as_deref() == Some(old);
                    if !same && from.is_file() && !to.exists() {
                        std::fs::rename(&from, &to).map_err(|_| Status::Internal)?;
                    }
                }
            }
            p if p.starts_with("/studio/actions/widget/add/") => {
                let kind = &p["/studio/actions/widget/add/".len()..];
                if !matches!(kind, "meter" | "ascii") {
                    return Err(Status::NotFound);
                }
                // Which server to point it at: whatever the widgets already
                // on the desktop use, so a second widget joins the first
                // rather than guessing a port. Falling back to the demo's
                // default is only for the very first one.
                let desktop = self.table("desktop");
                let authority = desktop
                    .get("widgets")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.iter().find_map(|w| {
                        let app = w.as_table()?.get("app")?.as_str()?;
                        let rest = app.strip_prefix("rill://")?;
                        rest.split('/').next().map(str::to_string)
                    }))
                    .unwrap_or_else(|| "127.0.0.1:7420".to_string());
                // Sizes that suit each one at default metrics; both are then
                // the user's to drag and resize.
                let (w, h) = match kind {
                    // Four gauges, a spark line and a footer now — the old
                    // 160 clipped the disk row.
                    "meter" => (300, 210),
                    _ => (420, 300),
                };
                let mut entry = toml::Table::new();
                entry.insert("app".into(), toml::Value::String(format!("rill://{authority}/{kind}")));
                entry.insert("anchor".into(), toml::Value::String("top-right".into()));
                entry.insert("width".into(), toml::Value::Integer(w));
                entry.insert("height".into(), toml::Value::Integer(h));
                entry.insert("x".into(), toml::Value::Integer(20));
                entry.insert("y".into(), toml::Value::Integer(20));
                self.update_table("desktop", move |t| {
                    let list = t
                        .entry("widgets")
                        .or_insert_with(|| toml::Value::Array(Vec::new()));
                    if let Some(array) = list.as_array_mut() {
                        array.push(toml::Value::Table(entry));
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/widget/remove/") => {
                let at: usize = p["/studio/actions/widget/remove/".len()..]
                    .parse()
                    .map_err(|_| Status::PathInvalid)?;
                self.update_table("desktop", move |t| {
                    if let Some(array) = t.get_mut("widgets").and_then(|v| v.as_array_mut())
                        && at < array.len()
                    {
                        array.remove(at);
                        // An empty list is removed rather than left as an
                        // empty array, so a reset theme has no debris in it.
                        if array.is_empty() {
                            t.remove("widgets");
                        }
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/widget/anchor/") => {
                let rest = &p["/studio/actions/widget/anchor/".len()..];
                let (at, anchor) = rest.rsplit_once('/').ok_or(Status::PathInvalid)?;
                let at: usize = at.parse().map_err(|_| Status::PathInvalid)?;
                if !ANCHORS.contains(&anchor) {
                    return Err(Status::NotFound);
                }
                let anchor = anchor.to_string();
                self.update_table("desktop", move |t| {
                    if let Some(entry) = t
                        .get_mut("widgets")
                        .and_then(|v| v.as_array_mut())
                        .and_then(|a| a.get_mut(at))
                        .and_then(|w| w.as_table_mut())
                    {
                        entry.insert("anchor".into(), toml::Value::String(anchor));
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/ascii/art/") => {
                let name = p["/studio/actions/ascii/art/".len()..].to_string();
                if !matches!(name.as_str(), "cube" | "wave" | "plasma") {
                    return Err(Status::NotFound);
                }
                self.update_ascii(move |t| {
                    t.insert("art".into(), toml::Value::String(name));
                })?;
            }
            "/studio/actions/ascii/file" => {
                let path = fields
                    .iter()
                    .find(|(k, _)| k == "path")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.trim().to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();
                // An empty box is a no-op: the button sits beside it and
                // pressing it by accident should not blank the widget.
                if !path.is_empty() {
                    self.update_ascii(move |t| {
                        t.insert("art".into(), toml::Value::String(path));
                    })?;
                }
            }
            p if p.starts_with("/studio/actions/particles/") => {
                let name = p["/studio/actions/particles/".len()..].to_string();
                let flock = name == "flock";
                let chosen = match name.as_str() {
                    "off" | "flock" => None,
                    other => Some(
                        particle_sets()
                            .into_iter()
                            .find(|s| s.name == other)
                            .ok_or(Status::NotFound)?,
                    ),
                };
                self.update_table("desktop", move |t| match chosen {
                    Some(set) => {
                        let put = |t: &mut toml::Table, k: &str, v: Option<PathBuf>| match v {
                            Some(p) => {
                                t.insert(k.into(), toml::Value::String(p.display().to_string()));
                            }
                            None => {
                                t.remove(k);
                            }
                        };
                        put(t, "particle_shader", Some(set.update));
                        put(t, "particle_diffuse", set.diffuse);
                        put(t, "particle_render", set.draw);
                        // A set with no count set yet would install and draw
                        // nothing; give it one rather than looking broken.
                        // The set's own count, not whatever the last one
                        // used. Carrying it over is how a flock ends up with
                        // a slime mould's two hundred thousand agents.
                        t.remove("boids");
                        t.insert("particles".into(), toml::Value::Integer(set.count));
                    }
                    None if flock => {
                        // The built-in flock: a count, and no shaders over it.
                        t.remove("particle_shader");
                        t.remove("particle_diffuse");
                        t.remove("particle_render");
                        t.remove("boids");
                        t.insert("particles".into(), toml::Value::Integer(DEFAULT_PARTICLES));
                    }
                    None => {
                        // Off means off. Both spellings of the count go, or
                        // the one left behind keeps the simulation running.
                        t.remove("particle_shader");
                        t.remove("particle_diffuse");
                        t.remove("particle_render");
                        t.remove("particles");
                        t.remove("boids");
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/winfx/") => {
                let name = p["/studio/actions/winfx/".len()..].to_string();
                let chosen = match name.as_str() {
                    "off" => None,
                    other => Some(
                        window_fx_choices()
                            .into_iter()
                            .find(|(s, _)| s == other)
                            .ok_or(Status::NotFound)?,
                    ),
                };
                self.update_table("desktop", move |t| match chosen {
                    Some((_, path)) => {
                        t.insert(
                            "window_shader".into(),
                            toml::Value::String(path.display().to_string()),
                        );
                    }
                    None => {
                        t.remove("window_shader");
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/wall/") => {
                let name = &p["/studio/actions/wall/".len()..];
                if name == "off" {
                    self.update_table("desktop", |t| {
                        t.remove("background_shader");
                    })?;
                    self.sync_model()?;
                    return self.page();
                }
                let (_, path) = wall_choices()
                    .into_iter()
                    .find(|(stem, _)| stem == name)
                    .ok_or(Status::NotFound)?;
                self.update_table("desktop", move |t| {
                    t.insert(
                        "background_shader".into(),
                        toml::Value::String(path.display().to_string()),
                    );
                    // A shader covers an image entirely; keeping the image
                    // key set would make "which background am I on?" a
                    // trick question. The colour stays — it is the floor
                    // the shader is painted over.
                    t.remove("wallpaper");
                })?;
                // A wallpaper change decides whether the scene's model is on
                // stage: the showroom brings it, everything else dismisses it.
                self.sync_model()?;
            }
            p if p.starts_with("/studio/actions/bg-swatch/") => {
                let hex = format!("#{}", &p["/studio/actions/bg-swatch/".len()..]);
                if !is_hex_color(&hex) {
                    return Err(Status::NotFound);
                }
                self.set_background_color(hex)?;
            }
            "/studio/actions/bg/hex" => {
                let value = fields
                    .iter()
                    .find(|(k, _)| k == "value")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.trim().to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();
                if !is_hex_color(&value) {
                    // A typo re-renders rather than erroring — the same
                    // grace the token hex fields extend.
                    return self.page();
                }
                self.set_background_color(value)?;
            }
            "/studio/actions/bg/img-off" => {
                self.update_table("desktop", |t| {
                    t.remove("wallpaper");
                })?;
            }
            p if p.starts_with("/studio/actions/bg/img/") => {
                let name = &p["/studio/actions/bg/img/".len()..];
                let (images, _) = image_choices();
                let (_, path) = images
                    .into_iter()
                    .find(|(stem, _)| stem == name)
                    .ok_or(Status::NotFound)?;
                self.set_background_image(path)?;
            }
            "/studio/actions/bg/image" => {
                let path = fields
                    .iter()
                    .find(|(k, _)| k == "path")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.trim().to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();
                if path.is_empty() {
                    return self.page();
                }
                // `~/` spelled at a prompt means the server's home — the
                // same same-machine assumption the whole studio makes.
                let expanded = match path.strip_prefix("~/") {
                    Some(rest) => std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_default()
                        .join(rest),
                    None => PathBuf::from(&path),
                };
                if !expanded.is_file() {
                    return self.page();
                }
                self.set_background_image(expanded)?;
            }
            p if p.starts_with("/studio/actions/cur/") => {
                let rest = &p["/studio/actions/cur/".len()..];
                let (key, dir) = rest.rsplit_once('/').ok_or(Status::NotFound)?;
                let (key, default, lo, hi, step) = CURSOR_KNOBS
                    .iter()
                    .find(|(k, ..)| *k == key)
                    .copied()
                    .ok_or(Status::NotFound)?;
                let current = self
                    .table("cursor")
                    .get(key)
                    .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                    .unwrap_or(default);
                let delta = match dir {
                    "up" => step,
                    "down" => -step,
                    _ => return Err(Status::NotFound),
                };
                let next = (current + delta).clamp(lo, hi);
                self.update_table("cursor", move |t| {
                    t.insert(key.to_string(), toml::Value::Float(next));
                })?;
            }
            "/studio/actions/cursor-draw" => {
                let on = !self.table("cursor").get("draw").and_then(|v| v.as_bool()).unwrap_or(true);
                self.update_table("cursor", move |t| {
                    if on {
                        t.remove("draw"); // drawn is the default
                    } else {
                        t.insert("draw".into(), toml::Value::Boolean(false));
                    }
                })?;
            }
            "/studio/actions/cursor-reset" => {
                self.update_table("cursor", |t| t.clear())?;
            }
            p if p.starts_with("/studio/actions/dock-place/") => {
                let rest = &p["/studio/actions/dock-place/".len()..];
                let (item, slot) = rest.split_once('/').ok_or(Status::NotFound)?;
                if !DOCK_ITEMS.iter().any(|(i, _)| *i == item)
                    || (slot != "off" && !DOCK_SLOTS.iter().any(|(s, _)| *s == slot))
                {
                    return Err(Status::NotFound);
                }
                self.place_dock_item(item, slot)?;
            }
            p if p.starts_with("/studio/actions/dock-move/") => {
                let rest = &p["/studio/actions/dock-move/".len()..];
                let (item, dir) = rest.split_once('/').ok_or(Status::NotFound)?;
                if !DOCK_ITEMS.iter().any(|(i, _)| *i == item) {
                    return Err(Status::NotFound);
                }
                self.nudge_dock_item(item, if dir == "fwd" { 1 } else { -1 })?;
            }
            p if p.starts_with("/studio/actions/dock-clock/") => {
                let style = p["/studio/actions/dock-clock/".len()..].to_string();
                if !["24h", "12h", "off"].contains(&style.as_str()) {
                    return Err(Status::NotFound);
                }
                self.update_dock(move |t| {
                    t.insert("clock".into(), toml::Value::String(style));
                })?;
            }
            p if p.starts_with("/studio/actions/dock-bg/") => {
                let mode = p["/studio/actions/dock-bg/".len()..].to_string();
                if !DOCK_BACKGROUNDS.iter().any(|(v, _)| *v == mode) {
                    return Err(Status::NotFound);
                }
                self.update_dock(move |t| {
                    // Glass is the default, so choosing it removes the key
                    // rather than writing one — a theme file should say what
                    // is unusual about a desktop, not restate its defaults.
                    if mode == "glass" {
                        t.remove("background");
                    } else {
                        t.insert("background".into(), toml::Value::String(mode));
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/dock-size/") => {
                let rest = &p["/studio/actions/dock-size/".len()..];
                let (key, dir) = rest.rsplit_once('/').ok_or(Status::NotFound)?;
                let (key, default, lo, hi, step) = DOCK_KNOBS
                    .iter()
                    .find(|(k, ..)| *k == key)
                    .copied()
                    .ok_or(Status::NotFound)?;
                let current = self
                    .dock()
                    .get(key)
                    .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                    .unwrap_or(default);
                let delta = match dir {
                    "up" => step,
                    "down" => -step,
                    _ => return Err(Status::NotFound),
                };
                let next = (current + delta).clamp(lo, hi);
                self.update_dock(move |t| {
                    t.insert(key.to_string(), toml::Value::Float(next));
                })?;
            }
            "/studio/actions/dock-date" => {
                let on = !self.dock().get("clock_date").and_then(|v| v.as_bool()).unwrap_or(false);
                self.update_dock(move |t| {
                    t.insert("clock_date".into(), toml::Value::Boolean(on));
                })?;
            }
            "/studio/actions/dock-reset" => {
                self.update_dock(|t| t.clear())?;
            }
            p if p.starts_with("/studio/actions/model/") => {
                let which = &p["/studio/actions/model/".len()..];
                if which == "none" {
                    self.update_showroom(|t| {
                        t.remove("model");
                    })?;
                } else {
                    let idx: usize = which.parse().map_err(|_| Status::NotFound)?;
                    let (_, path) =
                        self.model_choices().into_iter().nth(idx).ok_or(Status::NotFound)?;
                    self.update_showroom(move |t| {
                        t.insert("model".into(), toml::Value::String(path.clone()));
                    })?;
                    let (_, path) =
                        self.model_choices().into_iter().nth(idx).ok_or(Status::NotFound)?;
                    self.apply_model_hints(&path)?;
                }
                self.sync_model()?;
            }
            p if p.starts_with("/studio/actions/palette/") => {
                let name = &p["/studio/actions/palette/".len()..];
                let (_, pairs) = PALETTES
                    .iter()
                    .find(|(n, _)| *n == name)
                    .ok_or(Status::NotFound)?;
                self.update_table("colors", |colors| {
                    for (token, value) in *pairs {
                        colors.insert(token.to_string(), toml::Value::String(value.to_string()));
                    }
                })?;
            }
            p if p.starts_with("/studio/actions/target/") => {
                let token = &p["/studio/actions/target/".len()..];
                let known = if let Some(name) = token.strip_prefix("sr:") {
                    SHOWROOM_COLORS.iter().any(|(k, _)| *k == name)
                } else if let Some(name) = token.strip_prefix("win:") {
                    WINDOW_COLORS.iter().any(|(k, _)| *k == name)
                } else if let Some(name) = token.strip_prefix("cur:") {
                    CURSOR_COLORS.iter().any(|(k, _)| *k == name)
                } else {
                    token == "desk:background" || TOKENS.iter().any(|(t, _)| t == &token)
                };
                if !known {
                    return Err(Status::NotFound);
                }
                *self.target.lock().unwrap() = token.to_string();
            }
            // The whole colour as three numbers, from the sliders. Alpha
            // rides along so one submit carries the full state of the
            // panel — four sliders, one action, no ordering dance.
            "/studio/actions/hsl" => {
                let num = |name: &str| {
                    fields.iter().find(|(n, _)| n == name).and_then(|(_, v)| match v {
                        ActionValue::Num(n) => Some(*n as f32),
                        ActionValue::Str(s) => s.parse::<f32>().ok(),
                        _ => None,
                    })
                };
                let (Some(h), Some(sat), Some(l)) = (num("h"), num("s"), num("l")) else {
                    return Err(Status::Internal);
                };
                let rgb = hsl_hex(h.clamp(0.0, 360.0), (sat / 100.0).clamp(0.0, 1.0), (l / 100.0).clamp(0.0, 1.0));
                let alpha = num("a")
                    .map(|a| ((a / 100.0).clamp(0.0, 1.0) * 255.0).round() as u8)
                    .unwrap_or(255);
                let target = self.target.lock().unwrap().clone();
                let (home, name, _) = Self::color_home(&target);
                self.remember(&rgb);
                self.set_color(home, name, join_alpha(&rgb, alpha))?;
            }
            // Hue alone: the bar was clicked. Saturation and lightness stay
            // what they were — turning the wheel must not also move along it.
            // The accent dot row on the landing page: one colour, set
            // directly — the landing is for the person who wants the
            // decision, not the instrument.
            p if p.starts_with("/studio/actions/accent/") => {
                let rgb = &p["/studio/actions/accent/".len()..];
                if rgb.len() != 6 || !rgb.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(Status::NotFound);
                }
                self.remember(rgb);
                self.set_color("colors", "accent".into(), format!("#{rgb}"))?;
            }
            p if p.starts_with("/studio/actions/hue/") => {
                let deg: f32 = p["/studio/actions/hue/".len()..]
                    .parse()
                    .map_err(|_| Status::NotFound)?;
                let target = self.target.lock().unwrap().clone();
                let (home, name, fallback) = Self::color_home(&target);
                let (rgb, alpha) = split_alpha(&self.color_value(home, &name, fallback));
                let c = rill_doc::Color::parse_hex(&format!("#{rgb}"))
                    .ok_or(Status::Internal)?;
                let (_, sat, l) = rgb_to_hsl(c);
                let rgb = hsl_hex(deg.clamp(0.0, 360.0), sat.max(0.15), l.clamp(0.1, 0.9));
                self.remember(&rgb);
                self.set_color(home, name, join_alpha(&rgb, alpha))?;
            }
            // A typed hex, target-routed — accepts #rgb, #rrggbb, #rrggbbaa.
            "/studio/actions/pickhex" => {
                let value = fields
                    .iter()
                    .find(|(n, _)| n == "value")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.trim().to_string()),
                        _ => None,
                    })
                    .ok_or(Status::Internal)?;
                if !is_hex_color(&value) {
                    // Not a colour: re-serve rather than error, the way the
                    // token hex fields already behave.
                    return self.page();
                }
                let target = self.target.lock().unwrap().clone();
                let (home, name, _) = Self::color_home(&target);
                let (rgb, alpha) = split_alpha(&value);
                self.remember(&rgb);
                self.set_color(home, name, join_alpha(&rgb, alpha))?;
            }
            p if p.starts_with("/studio/actions/pick/") => {
                // A grid pick sets the target's rgb and keeps its alpha — a
                // translucent accent stays translucent through a hue change.
                let rgb = &p["/studio/actions/pick/".len()..];
                if rgb.len() != 6 || !rgb.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(Status::NotFound);
                }
                let rgb = rgb.to_lowercase();
                let target = self.target.lock().unwrap().clone();
                let (home, name, fallback) = Self::color_home(&target);
                let (_, alpha) = split_alpha(&self.color_value(home, &name, fallback));
                self.remember(&rgb);
                self.set_color(home, name, join_alpha(&rgb, alpha))?;
            }
            "/studio/actions/alpha/up" | "/studio/actions/alpha/down" => {
                let step: i32 = if path.ends_with("up") { 16 } else { -16 };
                let target = self.target.lock().unwrap().clone();
                let (home, name, fallback) = Self::color_home(&target);
                let (rgb, alpha) = split_alpha(&self.color_value(home, &name, fallback));
                let alpha = (alpha as i32 + step).clamp(0, 255) as u8;
                self.set_color(home, name, join_alpha(&rgb, alpha))?;
            }
            p if p.starts_with("/studio/actions/sr/") => {
                let rest = &p["/studio/actions/sr/".len()..];
                let (key, dir) = rest.rsplit_once('/').ok_or(Status::NotFound)?;
                let (key, default, lo, hi, step) = SHOWROOM_KNOBS
                    .iter()
                    .find(|(k, ..)| *k == key)
                    .copied()
                    .ok_or(Status::NotFound)?;
                let current = self
                    .showroom()
                    .get(key)
                    .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                    .unwrap_or(default);
                let delta = match dir {
                    "up" => step,
                    "down" => -step,
                    _ => return Err(Status::NotFound),
                };
                let next = (current + delta).clamp(lo, hi);
                self.update_showroom(move |t| {
                    t.insert(key.to_string(), toml::Value::Float(next));
                })?;
            }
            "/studio/actions/sr-fill" => {
                let on = !self.showroom().get("fill").and_then(|v| v.as_bool()).unwrap_or(true);
                self.update_showroom(move |t| {
                    if on {
                        t.remove("fill"); // on is the default
                    } else {
                        t.insert("fill".into(), toml::Value::Boolean(false));
                    }
                })?;
            }
            "/studio/actions/sr-reverse" => {
                let spin = self
                    .showroom()
                    .get("spin")
                    .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                    .unwrap_or(0.08);
                self.update_showroom(move |t| {
                    t.insert("spin".into(), toml::Value::Float(-spin));
                })?;
            }
            "/studio/actions/sr-reset" => {
                self.update_showroom(|t| t.clear())?;
            }
            p if p.starts_with("/studio/actions/win/") => {
                let rest = &p["/studio/actions/win/".len()..];
                let (key, dir) = rest.rsplit_once('/').ok_or(Status::NotFound)?;
                let (key, default, max, step) = WINDOW_KNOBS
                    .iter()
                    .find(|(k, ..)| *k == key)
                    .copied()
                    .ok_or(Status::NotFound)?;
                let current = self
                    .table("window")
                    .get(key)
                    .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                    .unwrap_or(default);
                let delta = match dir {
                    "up" => step,
                    "down" => -step,
                    _ => return Err(Status::NotFound),
                };
                let next = (current + delta).clamp(0.0, max);
                self.update_table("window", move |t| {
                    t.insert(key.to_string(), toml::Value::Float(next));
                })?;
            }
            "/studio/actions/derive_ansi" => {
                let colors = self.table("colors");
                let find = |name: &str, fallback: &str| -> rill_doc::Color {
                    colors
                        .get(name)
                        .and_then(|v| v.as_str())
                        .and_then(rill_doc::Color::parse_hex)
                        .or_else(|| rill_doc::Color::parse_hex(fallback))
                        .unwrap_or(rill_doc::Color { r: 0x80, g: 0x80, b: 0x80, a: 0xff })
                };
                let derived = derive_ansi(
                    find("accent", "#7c5cff"),
                    find("page", "#121219"),
                    find("text", "#e8e8f0"),
                );
                self.update_table("colors", move |colors| {
                    for (name, hex) in derived {
                        colors.insert(name, toml::Value::String(hex));
                    }
                })?;
            }
            "/studio/actions/clear-ansi" => {
                self.update_table("colors", |colors| {
                    colors.retain(|k, _| !k.starts_with("ansi_"));
                })?;
            }
            p if p.starts_with("/studio/actions/set/") => {
                let token = &p["/studio/actions/set/".len()..];
                if !TOKENS.iter().any(|(t, _)| t == &token) {
                    return Err(Status::NotFound);
                }
                let value = fields
                    .iter()
                    .find(|(n, _)| n == "value")
                    .and_then(|(_, v)| match v {
                        ActionValue::Str(s) => Some(s.trim().to_string()),
                        _ => None,
                    })
                    .ok_or(Status::Internal)?;
                // Reject anything that isn't a color rather than writing a
                // token the whole desktop then fails to parse.
                if !is_hex_color(&value) {
                    return self.page();
                }
                let token = token.to_string();
                self.update_table("colors", move |colors| {
                    colors.insert(token, toml::Value::String(value));
                })?;
            }
            _ => return Err(Status::NotFound),
        }
        self.page()
    }
}

#[cfg(test)]
mod ansi_tests {
    use super::*;

    fn c(hex: &str) -> rill_doc::Color {
        rill_doc::Color::parse_hex(hex).unwrap()
    }

    fn hue_of(hex: &str) -> f32 {
        rgb_to_hsl(c(hex)).0
    }

    /// A derived palette is still a palette: red reads as red, green as
    /// green. What the rice changes is how light and how saturated they
    /// are, not which colour they are — nobody wants a terminal whose
    /// "red" is blue.
    #[test]
    fn the_derived_hues_are_still_the_colours_they_are_named_after() {
        let set: std::collections::HashMap<String, String> =
            derive_ansi(c("#37b86b"), c("#202020"), c("#ececec")).into_iter().collect();
        for (name, want, tolerance) in [
            ("ansi-red", 0.0, 40.0),
            ("ansi-yellow", 45.0, 30.0),
            ("ansi-blue", 220.0, 40.0),
            ("ansi-magenta", 290.0, 40.0),
        ] {
            let got = hue_of(&set[name]);
            let delta = ((got - want + 540.0).rem_euclid(360.0) - 180.0).abs();
            assert!(delta <= tolerance, "{name} came out at hue {got}, wanted about {want}");
        }
    }

    /// The hue nearest the accent leans onto it, so the palette reads as
    /// belonging to the rice rather than sitting beside it. rill-green's
    /// accent is a green, so the terminal's green should move toward it.
    #[test]
    fn the_nearest_hue_leans_onto_the_accent() {
        let accent = "#37b86b";
        let set: std::collections::HashMap<String, String> =
            derive_ansi(c(accent), c("#202020"), c("#ececec")).into_iter().collect();
        let accent_hue = hue_of(accent);
        let green = hue_of(&set["ansi-green"]);
        let stock = 130.0_f32;
        assert!(
            (green - accent_hue).abs() < (stock - accent_hue).abs(),
            "green ({green}) did not move toward the accent ({accent_hue})"
        );
        // And a hue nowhere near the accent keeps its own.
        let red = hue_of(&set["ansi-red"]);
        assert!(red < 25.0 || red > 335.0, "red drifted to {red}");
    }

    /// Both ends come from the page and the text, so the palette belongs to
    /// the same surface the terminal is drawn on: black near the page,
    /// bright white at the text.
    #[test]
    fn the_ends_come_from_the_page_and_the_text() {
        let set: std::collections::HashMap<String, String> =
            derive_ansi(c("#37b86b"), c("#202020"), c("#ececec")).into_iter().collect();
        let (_, _, black) = rgb_to_hsl(c(&set["ansi-black"]));
        let (_, _, white) = rgb_to_hsl(c(&set["ansi-bright-white"]));
        let (_, _, page) = rgb_to_hsl(c("#202020"));
        let (_, _, text) = rgb_to_hsl(c("#ececec"));
        assert!((black - page).abs() < 0.12, "black is not near the page");
        assert!((white - text).abs() < 0.05, "bright white is not the text");
        assert!(black < white, "the palette is inside out");
        assert_eq!(set.len(), 16, "all sixteen were derived");
    }

    /// A light rice gets a light-rice palette: the colours go darker, not
    /// brighter, or nothing is readable on a pale page.
    #[test]
    fn a_light_theme_gets_darker_colours() {
        let dark: std::collections::HashMap<String, String> =
            derive_ansi(c("#37b86b"), c("#202020"), c("#ececec")).into_iter().collect();
        let light: std::collections::HashMap<String, String> =
            derive_ansi(c("#37b86b"), c("#f6f4ee"), c("#2a2620")).into_iter().collect();
        let l = |set: &std::collections::HashMap<String, String>, k: &str| rgb_to_hsl(c(&set[k])).2;
        assert!(
            l(&light, "ansi-blue") < l(&dark, "ansi-blue"),
            "the light theme's blue is not darker than the dark theme's"
        );
        assert!(l(&light, "ansi-black") > l(&light, "ansi-bright-white"),
            "on a light page the palette should run the other way round");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn studio(name: &str) -> (PathBuf, Studio) {
        let dir = std::env::temp_dir()
            .join(format!("studio-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), Studio::new(dir.join("theme.toml")))
    }

    fn act(s: &Studio, path: &str) {
        s.action(path, &[], &Identity::Anonymous).unwrap();
    }

    /// Whether an action was refused — for the paths that must not exist.
    fn act_err(s: &Studio, path: &str) -> bool {
        s.action(path, &[], &Identity::Anonymous).is_err()
    }

    /// The composed picker's three verbs: sliders set the whole colour,
    /// the hue bar turns the wheel without sliding along it, and a typed
    /// hex lands exactly, alpha included.
    #[test]
    fn the_picker_verbs_edit_the_target() {
        let (dir, s) = studio("picker-verbs");
        std::fs::write(dir.join("theme.toml"), "[colors]\naccent = \"#37b86b\"\n").unwrap();

        // Sliders: H/S/L/A as numbers.
        let f = |n: &str, v: f64| (n.to_string(), ActionValue::Num(v));
        s.action(
            "/studio/actions/hsl",
            &[f("h", 0.0), f("s", 100.0), f("l", 50.0), f("a", 100.0)],
            &Identity::Anonymous,
        )
        .unwrap();
        let now = s.color_value("colors", "accent", "#000000");
        assert_eq!(now, "#ff0000", "H0 S100 L50 is red: {now}");

        // The hue bar: saturation and lightness must survive the turn.
        s.action("/studio/actions/hue/240", &[], &Identity::Anonymous).unwrap();
        let now = s.color_value("colors", "accent", "#000000");
        let c = rill_doc::Color::parse_hex(&now).unwrap();
        let (h, sat, l) = rgb_to_hsl(c);
        assert!((h - 240.0).abs() < 2.0, "hue turned to blue: {h}");
        assert!(sat > 0.9, "saturation survived: {sat}");
        assert!((l - 0.5).abs() < 0.05, "lightness survived: {l}");

        // Typed hex, with alpha.
        s.action(
            "/studio/actions/pickhex",
            &[("value".into(), ActionValue::Str("#12345680".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        assert_eq!(s.color_value("colors", "accent", "#000000"), "#12345680");

        // Garbage is re-served, not written.
        s.action(
            "/studio/actions/pickhex",
            &[("value".into(), ActionValue::Str("not-a-colour".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        assert_eq!(s.color_value("colors", "accent", "#000000"), "#12345680");

        // And every pick fed the recent row, newest first, deduplicated.
        let recent = s.recent.lock().unwrap().clone();
        assert_eq!(recent.first().map(String::as_str), Some("123456"));
        assert!(recent.len() >= 2);

        // The page with the picker on it still compiles.
        assert!(s.get("/studio/colors", &Identity::Anonymous).is_some(), "the page serves");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The landing page: Studio opens on Appearance, the accent dots set
    /// the accent in one click, and a look card applies its rice by
    /// navigation — the whole card is the control.
    #[test]
    fn the_landing_page_makes_the_big_decisions_one_click_deep() {
        let (dir, s) = studio("landing");
        std::fs::write(dir.join("theme.toml"), "[colors]\naccent = \"#111111\"\n").unwrap();

        // Opens on Appearance, and the page compiles.
        assert!(s.get("/studio", &Identity::Anonymous).is_some());
        assert_eq!(s.section.lock().unwrap().as_str(), "appearance");

        // An accent dot is one action.
        act(&s, "/studio/actions/accent/37b86b");
        assert_eq!(s.color_value("colors", "accent", "#000000"), "#37b86b");

        // Save the current desktop as a look, recolour, then apply the look
        // by navigating to its card target: the colour comes back.
        s.action(
            "/studio/actions/rice/save",
            &[("name".into(), ActionValue::Str("green-one".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        act(&s, "/studio/actions/accent/e25c5c");
        assert_eq!(s.color_value("colors", "accent", "#000000"), "#e25c5c");
        assert!(s.get("/studio/apply/green-one", &Identity::Anonymous).is_some());
        assert_eq!(
            s.color_value("colors", "accent", "#000000"),
            "#37b86b",
            "the card's navigation applied the look"
        );
        // And a name that is not a saved look is simply not found.
        assert!(s.get("/studio/apply/no-such-look", &Identity::Anonymous).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The preset lifecycle, both directions: applying a look remembers
    /// it; a turned knob makes the desktop "based on X, modified"; Update
    /// keeps the divergence in the look itself.
    #[test]
    fn a_modified_look_can_be_updated_in_place() {
        let (dir, s) = studio("lifecycle");
        std::fs::write(dir.join("theme.toml"), "[colors]\naccent = \"#111111\"\n").unwrap();
        s.action(
            "/studio/actions/rice/save",
            &[("name".into(), ActionValue::Str("base".into()))],
            &Identity::Anonymous,
        )
        .unwrap();

        // Pristine: the landing shows no modified line.
        let bytes = s.get("/studio", &Identity::Anonymous).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("modified"));

        // Turn a knob: the divergence is named.
        act(&s, "/studio/actions/accent/37b86b");
        let bytes = s.get("/studio", &Identity::Anonymous).unwrap();
        let doc = rill_doc::decode(&bytes).unwrap();
        let all: Vec<&str> = (0..doc.strings.len() as u16).map(|i| doc.string(i)).collect();
        assert!(
            all.iter().any(|t| t.contains("Based on base — modified")),
            "the badge names what was diverged from"
        );

        // Update: the look absorbs the change; the badge stands down.
        act(&s, "/studio/actions/rice/update");
        let config = dir.as_path();
        let saved = std::fs::read_to_string(
            rill_appkit::rices::path(config, "base").unwrap(),
        )
        .unwrap();
        assert!(saved.contains("37b86b"), "the divergence was kept");
        let bytes = s.get("/studio", &Identity::Anonymous).unwrap();
        let doc = rill_doc::decode(&bytes).unwrap();
        let all: Vec<&str> = (0..doc.strings.len() as u16).map(|i| doc.string(i)).collect();
        assert!(!all.iter().any(|t| t.contains("modified")), "pristine again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Colour page is the one room: window glow, shadow, cursor and
    /// the desktop floor are all picker targets, and the floor keeps its
    /// mode semantics wherever it is edited from — picking a colour turns
    /// the image and shader off, exactly as the Desktop page's verb does.
    #[test]
    fn every_surface_colour_edits_from_the_colour_page() {
        let (dir, s) = studio("surface-colours");
        std::fs::write(
            dir.join("theme.toml"),
            "[desktop]\nwallpaper = \"/tmp/x.png\"\nbackground_shader = \"/tmp/x.glsl\"\n",
        )
        .unwrap();

        for (target, home, key) in [
            ("win:focus_glow", "window", "focus_glow"),
            ("win:shadow_color", "window", "shadow_color"),
            ("cur:color", "cursor", "color"),
            ("desk:background", "desktop", "background_color"),
        ] {
            act(&s, &format!("/studio/actions/target/{target}"));
            act(&s, "/studio/actions/pick/123456");
            assert_eq!(
                s.color_value(home, key, "#000000"),
                "#123456",
                "{target} did not land in [{home}] {key}"
            );
        }

        // The floor pick cleared what would have hidden it.
        let desktop = s.table("desktop");
        assert!(desktop.get("wallpaper").is_none(), "the wallpaper survived a floor pick");
        assert!(desktop.get("background_shader").is_none(), "the shader survived");

        // And the page with the new chip row still compiles.
        assert!(s.get("/studio/colors", &Identity::Anonymous).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A saved look renders as a card with a context menu, and rename moves
    /// exactly one file: not onto an existing look, not onto itself, and
    /// never leaving the old name behind.
    #[test]
    fn looks_are_cards_and_rename_moves_the_file() {
        let (dir, s) = studio("looks");
        std::fs::write(dir.join("theme.toml"), "[colors]\naccent = \"#ff0000\"\n").unwrap();
        let save = |name: &str| {
            s.action(
                "/studio/actions/rice/save",
                &[("name".into(), rill_protocol::ActionValue::Str(name.into()))],
                &Identity::Anonymous,
            )
            .unwrap();
        };
        save("day");
        let page = s.get("/studio/rices", &Identity::Anonymous).expect("looks page");
        let doc = rill_doc::decode(&page).expect("decodes");
        assert!(
            doc.nodes.iter().any(|n| matches!(n, rill_doc::Node::Menu { .. })),
            "a card carries its right-click menu"
        );

        act(&s, "/studio/actions/rice/rename-target/day");
        s.action(
            "/studio/actions/rice/rename",
            &[("name".into(), rill_protocol::ActionValue::Str("night".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        let rice = |n: &str| rill_appkit::rices::path(&dir, n).unwrap();
        assert!(rice("night").is_file(), "renamed into place");
        assert!(!rice("day").exists(), "the old name is gone");

        // Renaming onto an existing look must not clobber it.
        save("day");
        act(&s, "/studio/actions/rice/rename-target/day");
        s.action(
            "/studio/actions/rice/rename",
            &[("name".into(), rill_protocol::ActionValue::Str("night".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        assert!(rice("day").is_file(), "a name collision is a no-op, not a clobber");
        assert!(act_err(&s, "/studio/actions/rice/rename-target/ghost"), "no such look");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The background's three modes keep each other honest: a colour pick
    /// turns image and shader off, an image turns the shader off but keeps
    /// the colour (it is the floor a transparent image shows), and picking
    /// what you can see is the invariant throughout.
    #[test]
    fn background_modes_clear_what_would_hide_them() {
        let (dir, s) = studio("bg-modes");
        std::fs::write(
            dir.join("theme.toml"),
            "[desktop]\nbackground_shader = \"x.wgsl\"\nwallpaper = \"y.png\"\n",
        )
        .unwrap();
        act(&s, "/studio/actions/bg-swatch/336699");
        let desk = |s: &Studio| s.table("desktop");
        let t = desk(&s);
        assert_eq!(t.get("background_color").and_then(|v| v.as_str()), Some("#336699"));
        assert!(t.get("wallpaper").is_none(), "a colour pick uncovers itself");
        assert!(t.get("background_shader").is_none());

        let img = dir.join("wall.png");
        std::fs::write(&img, b"x").unwrap();
        s.action(
            "/studio/actions/bg/image",
            &[("path".into(), rill_protocol::ActionValue::Str(img.display().to_string()))],
            &Identity::Anonymous,
        )
        .unwrap();
        let t = desk(&s);
        assert_eq!(
            t.get("wallpaper").and_then(|v| v.as_str()),
            Some(img.display().to_string().as_str()),
        );
        assert_eq!(
            t.get("background_color").and_then(|v| v.as_str()),
            Some("#336699"),
            "the colour is the floor an image sits on — it stays"
        );
        assert!(act_err(&s, "/studio/actions/bg-swatch/nothex"), "a swatch is a colour");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shader's `// @param` lines become sliders on the desktop page, and
    /// the slider's action stores a clamped value in
    /// `[desktop.shader_params.<stem>]` — the whole tuning loop, minus the
    /// compositor that reads it.
    #[test]
    fn shader_params_render_sliders_and_store_clamped_values() {
        let (dir, s) = studio("shaderparams");
        let shader = dir.join("glow.wgsl");
        std::fs::write(
            &shader,
            "// @param decay 0.1 .. 3.0 = 0.62 \"How fast trails fade\"\n\
             // @param reach 4.0 .. 40.0 = 12.0\n\
             @fragment fn fs_main() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("theme.toml"),
            format!("[desktop]\nbackground_shader = \"{}\"\n", shader.display()),
        )
        .unwrap();

        // The desktop page declares one slider per param, bound to a state
        // slot carrying the current (here: default) value.
        let bytes = s.get("/studio/background", &Identity::Anonymous).unwrap();
        let doc = rill_doc::decode(&bytes).unwrap();
        let decay_slot = doc
            .states
            .iter()
            .position(|v| doc.string(v.name_idx) == "sp-bg-decay")
            .expect("a state slot for the decay param") as u16;
        assert_eq!(
            doc.states[decay_slot as usize].initial,
            rill_doc::ActionValue::Num(0.62),
            "seeded with the declared default"
        );
        let (min, max) = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Slider { bind, min, max, .. } if *bind == decay_slot => {
                    Some((*min, *max))
                }
                _ => None,
            })
            .expect("a slider bound to the decay slot");
        assert_eq!((min, max), (0.1, 3.0), "the declared range travels");
        assert!(
            doc.strings.iter().any(|s| s == "How fast trails fade"),
            "the blurb shows"
        );

        // Releasing the slider stores the value under the shader's stem…
        s.action(
            "/studio/actions/shaderparam/bg/decay",
            &[("value".into(), ActionValue::Num(1.5))],
            &Identity::Anonymous,
        )
        .unwrap();
        // …and an out-of-range value (a stale page, a hostile client) is
        // clamped to the declaration, not written verbatim.
        s.action(
            "/studio/actions/shaderparam/bg/reach",
            &[("value".into(), ActionValue::Num(999.0))],
            &Identity::Anonymous,
        )
        .unwrap();
        let theme: toml::Table =
            std::fs::read_to_string(dir.join("theme.toml")).unwrap().parse().unwrap();
        let stored = theme["desktop"]["shader_params"]["glow"].as_table().unwrap();
        assert_eq!(stored["decay"].as_float(), Some(1.5));
        assert_eq!(stored["reach"].as_float(), Some(40.0));

        // A knob the shader never declared is refused, not invented.
        assert!(
            s.action(
                "/studio/actions/shaderparam/bg/nope",
                &[("value".into(), ActionValue::Num(1.0))],
                &Identity::Anonymous,
            )
            .is_err()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Save, load and delete a rice through the action surface — the path a
    /// person actually takes, rather than the library underneath it (which
    /// has its own tests in rill-appkit).
    #[test]
    fn rices_save_load_and_delete_through_the_studio() {
        let (dir, s) = studio("rices");
        let theme = dir.join("theme.toml");

        // Save the desktop as it stands, then change it.
        std::fs::write(&theme, "[colors]\npage = \"#000010\"\n").unwrap();
        s.action(
            "/studio/actions/rice/save",
            &[("name".into(), ActionValue::Str("Midnight Blue".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        // Named loosely, filed strictly.
        assert_eq!(rill_appkit::rices::list(&dir), vec!["midnight-blue".to_string()]);

        std::fs::write(&theme, "[colors]\npage = \"#ffffff\"\n").unwrap();
        assert!(std::fs::read_to_string(&theme).unwrap().contains("ffffff"));

        // Loading it puts the whole desktop back.
        act(&s, "/studio/actions/rice/load/midnight-blue");
        assert!(std::fs::read_to_string(&theme).unwrap().contains("#000010"));

        // The page marks the loaded rice as the current one.
        assert!(s.get("/studio/rices", &Identity::Anonymous).is_some());

        // A save with no name is a no-op, not an error — the button sits
        // next to an empty field.
        s.action(
            "/studio/actions/rice/save",
            &[("name".into(), ActionValue::Str("   ".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        assert_eq!(rill_appkit::rices::list(&dir).len(), 1, "no anonymous rice appeared");

        // And a name that tries to climb out lands in the rices directory.
        s.action(
            "/studio/actions/rice/save",
            &[("name".into(), ActionValue::Str("../../escape".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        assert!(!dir.join("../../escape.toml").exists(), "a rice escaped its directory");

        act(&s, "/studio/actions/rice/delete/midnight-blue");
        assert!(!rill_appkit::rices::list(&dir).contains(&"midnight-blue".to_string()));
    }

    /// Widgets are added, anchored and removed through `[[desktop.widgets]]`,
    /// which is an array of tables rather than a table — the one shape the
    /// studio's other pages never touch.
    #[test]
    fn the_widgets_page_edits_the_widget_list() {
        let (dir, s) = studio("widgets");
        let theme = dir.join("theme.toml");
        // An existing widget, so a second one joins the same server rather
        // than guessing a port.
        std::fs::write(
            &theme,
            "[[desktop.widgets]]\napp = \"rill://127.0.0.1:9001/meter\"\nanchor = \"top-left\"\n\
             width = 300\nheight = 160\nx = 20\ny = 20\n",
        )
        .unwrap();

        act(&s, "/studio/actions/widget/add/ascii");
        let widgets = |s: &Studio| -> Vec<toml::Value> {
            s.table("desktop")
                .get("widgets")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        };
        let list = widgets(&s);
        assert_eq!(list.len(), 2, "the added widget joined the list");
        let added = list[1].as_table().unwrap();
        assert_eq!(
            added.get("app").unwrap().as_str().unwrap(),
            "rill://127.0.0.1:9001/ascii",
            "it points at the server the existing widget uses, not a guessed port"
        );

        // Anchoring writes only that widget's anchor.
        act(&s, "/studio/actions/widget/anchor/1/bottom-left");
        let list = widgets(&s);
        assert_eq!(list[1].as_table().unwrap().get("anchor").unwrap().as_str(), Some("bottom-left"));
        assert_eq!(
            list[0].as_table().unwrap().get("anchor").unwrap().as_str(),
            Some("top-left"),
            "the other widget was left alone"
        );
        assert!(act_err(&s, "/studio/actions/widget/anchor/1/sideways"), "unknown anchor refused");

        // The ASCII source: a generator by name, or any path as a file.
        act(&s, "/studio/actions/ascii/art/plasma");
        let ascii = |s: &Studio| -> toml::Table {
            s.table("desktop").get("ascii").and_then(|v| v.as_table()).cloned().unwrap_or_default()
        };
        assert_eq!(ascii(&s).get("art").unwrap().as_str(), Some("plasma"));
        s.action(
            "/studio/actions/ascii/file",
            &[("path".into(), ActionValue::Str("  ~/pics/loop.gif ".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        assert_eq!(ascii(&s).get("art").unwrap().as_str(), Some("~/pics/loop.gif"));
        // An empty box does not blank the widget.
        s.action(
            "/studio/actions/ascii/file",
            &[("path".into(), ActionValue::Str("   ".into()))],
            &Identity::Anonymous,
        )
        .unwrap();
        assert_eq!(ascii(&s).get("art").unwrap().as_str(), Some("~/pics/loop.gif"));

        // Removing the last one takes the empty array with it.
        act(&s, "/studio/actions/widget/remove/1");
        act(&s, "/studio/actions/widget/remove/0");
        assert!(widgets(&s).is_empty());
        assert!(
            !std::fs::read_to_string(&theme).unwrap().contains("widgets"),
            "an emptied list leaves no debris behind"
        );

        // And the page itself compiles — the failure that renders as a
        // blank section rather than as an error.
        assert!(s.get("/studio/widgets", &Identity::Anonymous).is_some());
    }

    /// Every section in the sidebar has a page, and every page compiles.
    ///
    /// This is the check the studio most needs and least obviously has. A
    /// section is three strings in a table and a match arm somewhere else;
    /// nothing connects them, so adding one and forgetting the other used to
    /// render the Desktop page under the new name. And a page whose KDL is
    /// malformed does not error — it comes back as *nothing*, which is how a
    /// missing `;` between inline nodes shipped a blank Rices page.
    #[test]
    fn every_section_renders_a_page_of_its_own() {
        let (_dir, s) = studio("sections");
        let mut seen: Vec<String> = Vec::new();
        for (slug, label, icon) in SECTIONS {
            let page = s
                .get(&format!("/studio/{slug}"), &Identity::Anonymous)
                .unwrap_or_else(|| panic!("section {slug:?} ({label}) has no page"));
            assert!(!page.is_empty(), "section {slug:?} compiled to nothing");
            // The fallback arm renders a perfectly valid page, so "it
            // rendered something" is not the check — "it rendered *its own*
            // page" is. The marker travels in the document's string table.
            assert!(
                !page.windows(NOT_BUILT.len()).any(|w| w == NOT_BUILT),
                "section {slug:?} ({label}) is in the sidebar with no page written for it"
            );
            assert!(!ICONS_MISSING.contains(icon), "section {slug:?} names an icon that is not bundled");
            // Two sections rendering byte-identical pages means one of them
            // fell through to the other's arm.
            assert!(
                !seen.contains(&format!("{:?}", page)),
                "section {slug:?} rendered the same page as an earlier one"
            );
            seen.push(format!("{:?}", page));
        }
        // And an unknown section is refused rather than served something.
        assert!(s.get("/studio/nonesuch", &Identity::Anonymous).is_none());
    }

    /// Icons the shell does not bundle. Naming one leaves a hole in the rail.
    const ICONS_MISSING: &[&str] = &[];

    /// What the unwritten-page arm prints, as it appears in a compiled
    /// document's string table.
    const NOT_BUILT: &[u8] = b"NOT BUILT";

    /// A set brings its own agent count, and switching sets replaces it.
    ///
    /// Carrying the count between sets is wrong in both directions: a flock
    /// of two hundred thousand is a smear, and a slime mould of two thousand
    /// never forms a network. The count is declared in each simulation's own
    /// shader, so adding a set stays a one-file job.
    #[test]
    fn each_particle_set_brings_its_own_count() {
        let (_dir, s) = studio("counts");
        let count = || {
            s.table("desktop").get("particles").and_then(|v| v.as_integer()).unwrap_or(0)
        };
        let sets = particle_sets();
        let slime = sets.iter().find(|s| s.name == "slime").expect("slime set");
        let dust = sets.iter().find(|s| s.name == "dust").expect("dust set");
        assert!(
            slime.count > dust.count * 10,
            "a field simulation needs far more agents than a drawn one \
             (slime {}, dust {})",
            slime.count,
            dust.count
        );

        act(&s, "/studio/actions/particles/slime");
        assert_eq!(count(), slime.count, "slime got its declared count");
        act(&s, "/studio/actions/particles/dust");
        assert_eq!(count(), dust.count, "switching sets replaces the count, never keeps it");
        act(&s, "/studio/actions/particles/flock");
        assert_eq!(count(), DEFAULT_PARTICLES, "the built-in flock is a flock's worth");
        act(&s, "/studio/actions/particles/slime");
        assert_eq!(count(), slime.count, "and back again");
    }

    /// A shader's role is decided by its name, and the pickers must respect
    /// it. Offering a particle compute pass as a wallpaper compiles it
    /// against the fullscreen-fx preamble and fills the log with errors
    /// about a missing `params` — which is exactly what shipped the first
    /// time particle shaders landed in the shader directory.
    #[test]
    fn shader_pickers_only_offer_shaders_of_their_own_kind() {
        assert_eq!(role_of_shader("slime_update"), ShaderRole::ParticleUpdate);
        assert_eq!(role_of_shader("slime_diffuse"), ShaderRole::ParticleDiffuse);
        assert_eq!(role_of_shader("slime_draw"), ShaderRole::ParticleDraw);
        assert_eq!(role_of_shader("window_aura"), ShaderRole::WindowFx);
        assert_eq!(role_of_shader("lofi"), ShaderRole::Fx);

        // The bundled directory has all of these in it, so the lists are a
        // real test rather than a hypothetical one.
        let walls: Vec<String> = wall_choices().into_iter().map(|(s, _)| s).collect();
        let effects: Vec<String> = effect_choices().into_iter().map(|(s, _)| s).collect();
        for offered in walls.iter().chain(effects.iter()) {
            assert_eq!(
                role_of_shader(offered),
                ShaderRole::Fx,
                "{offered:?} is offered as a wallpaper or grader but is not an fx shader"
            );
        }
        // And the roles that do exist are reachable from their own pickers.
        let sets: Vec<String> = particle_sets().into_iter().map(|s| s.name).collect();
        assert!(sets.iter().any(|n| n == "slime"), "slime set not offered: {sets:?}");
        assert!(sets.iter().any(|n| n == "dust"), "dust set not offered: {sets:?}");
        let winfx: Vec<String> = window_fx_choices().into_iter().map(|(s, _)| s).collect();
        assert!(winfx.iter().any(|s| s == "window_aura"), "window aura not offered: {winfx:?}");
        // The slime set carries all three files; dust has no field pass.
        let slime = particle_sets().into_iter().find(|s| s.name == "slime").unwrap();
        assert!(slime.diffuse.is_some() && slime.draw.is_some(), "slime has a field pass and a draw");
    }

    /// "Off" has to mean off, from wherever you were.
    ///
    /// This is the bug that shipped: the particle system had two controls —
    /// an EFFECTS toggle writing `boids` and a set picker writing
    /// `particles` — and the compositor prefers `particles`. So turning the
    /// toggle off left the other key behind and the simulation kept running,
    /// with nothing on screen explaining why.
    #[test]
    fn particles_off_means_off_from_every_starting_state() {
        let (_dir, s) = studio("particles");
        let d = || s.table("desktop");
        let count = |t: &toml::Table| {
            t.get("particles").or_else(|| t.get("boids")).and_then(|v| v.as_integer()).unwrap_or(0)
        };

        // A custom set: count plus all the shader keys it has files for.
        act(&s, "/studio/actions/particles/slime");
        let t = d();
        assert!(count(&t) > 0, "a set must install a count or it draws nothing");
        assert!(t.get("particle_shader").is_some());
        assert!(t.get("particle_diffuse").is_some(), "slime has a field pass");
        assert!(t.get("particle_render").is_some());
        assert!(t.get("boids").is_none(), "the legacy count key is cleared, not left to fight");

        // Off from a set: every key goes, both spellings of the count.
        act(&s, "/studio/actions/particles/off");
        let t = d();
        assert_eq!(count(&t), 0, "off left a count behind — the simulation would still run");
        for k in ["particle_shader", "particle_diffuse", "particle_render", "particles", "boids"] {
            assert!(t.get(k).is_none(), "off left {k} behind");
        }

        // The built-in flock: a count and no shaders over it.
        act(&s, "/studio/actions/particles/flock");
        let t = d();
        assert!(count(&t) > 0);
        assert!(t.get("particle_shader").is_none(), "the flock is the built-in, not a set");

        // Off from the flock, too.
        act(&s, "/studio/actions/particles/off");
        assert_eq!(count(&d()), 0);

        // And off from a legacy theme that only ever knew `boids`.
        s.update_table("desktop", |t| {
            t.insert("boids".into(), toml::Value::Integer(2000));
        })
        .unwrap();
        assert_eq!(count(&d()), 2000);
        act(&s, "/studio/actions/particles/off");
        assert_eq!(count(&d()), 0, "off must clear the old key as well as the new one");
    }

    /// The desktop knobs (moved from the dock) write [desktop]: toggles
    /// flip, the shader sets its file + warp, and reset clears it all.
    #[test]
    fn desktop_knobs_write_the_desktop_table() {
        let (dir, s) = studio("desk");
        act(&s, "/studio/actions/desk/glass");
        assert_eq!(s.table("desktop").get("glass").and_then(|v| v.as_bool()), Some(true));
        act(&s, "/studio/actions/desk/glass");
        assert!(s.table("desktop").get("glass").is_none(), "toggle off removes the key");
        act(&s, "/studio/actions/shader/crt");
        let d = s.table("desktop");
        assert!(d.get("shader").and_then(|v| v.as_str()).unwrap().ends_with("crt.wgsl"));
        assert_eq!(d.get("warp_barrel").and_then(|v| v.as_float()), Some(0.07));
        act(&s, "/studio/actions/shader/off");
        assert!(s.table("desktop").get("shader").is_none());
        act(&s, "/studio/actions/wall/lofi");
        assert!(
            s.table("desktop")
                .get("background_shader")
                .and_then(|v| v.as_str())
                .unwrap()
                .ends_with("lofi.wgsl")
        );
        act(&s, "/studio/actions/reset");
        assert!(s.table("desktop").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The dock's material and shape write `[desktop.dock]`, and the
    /// default material writes nothing — a theme file should say what is
    /// unusual about a desktop, not restate its defaults.
    #[test]
    fn dock_material_and_shape_write_the_dock_table() {
        let (dir, s) = studio("dock-style");
        act(&s, "/studio/actions/dock-bg/none");
        assert_eq!(s.dock().get("background").and_then(|v| v.as_str()), Some("none"));
        act(&s, "/studio/actions/dock-bg/glass");
        assert!(s.dock().get("background").is_none(), "the default is not written down");
        assert!(act_err(&s, "/studio/actions/dock-bg/frosted"), "an unknown material is refused");

        // Steppers move by their step and stop at their bounds.
        act(&s, "/studio/actions/dock-size/height/up");
        assert_eq!(s.dock().get("height").and_then(|v| v.as_float()), Some(46.0));
        for _ in 0..200 {
            act(&s, "/studio/actions/dock-size/height/down");
        }
        assert_eq!(
            s.dock().get("height").and_then(|v| v.as_float()),
            Some(20.0),
            "clamped at the floor rather than shrinking to nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mono weight is a metrics knob like F and P, and clamps like them.
    #[test]
    fn mono_weight_steps_and_clamps() {
        let (dir, s) = studio("mono");
        act(&s, "/studio/actions/mono/up");
        assert_eq!(s.table("metrics").get("mono_weight").and_then(|v| v.as_integer()), Some(600));
        for _ in 0..20 {
            act(&s, "/studio/actions/mono/up");
        }
        assert_eq!(s.table("metrics").get("mono_weight").and_then(|v| v.as_integer()), Some(900));
        for _ in 0..20 {
            act(&s, "/studio/actions/mono/down");
        }
        assert_eq!(
            s.table("metrics").get("mono_weight").and_then(|v| v.as_integer()),
            Some(200),
            "a mono surface never goes lighter than the face it has"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The showroom knobs write [desktop.showroom] — one table both the
    /// room and the model read — and clamp at their declared bounds.
    #[test]
    fn showroom_knobs_and_scene_colors_write_their_own_table() {
        let (dir, s) = studio("showroom");
        act(&s, "/studio/actions/sr/spin/down");
        assert_eq!(s.showroom().get("spin").and_then(|v| v.as_float()), Some(0.06));
        for _ in 0..80 {
            act(&s, "/studio/actions/sr/spin/down");
        }
        assert_eq!(
            s.showroom().get("spin").and_then(|v| v.as_float()),
            Some(-0.6),
            "spin clamps at its reverse bound"
        );
        act(&s, "/studio/actions/sr-reverse");
        assert_eq!(s.showroom().get("spin").and_then(|v| v.as_float()), Some(0.6));
        act(&s, "/studio/actions/sr-fill");
        assert_eq!(s.showroom().get("fill").and_then(|v| v.as_bool()), Some(false));

        // A scene colour lands in the showroom table, never in [colors].
        act(&s, "/studio/actions/target/sr:body_color");
        act(&s, "/studio/actions/pick/c81e1e");
        assert_eq!(
            s.showroom().get("body_color").and_then(|v| v.as_str()),
            Some("#c81e1e"),
            "the car repaints from the scene table"
        );
        assert!(s.table("colors").get("body_color").is_none());

        act(&s, "/studio/actions/sr-reset");
        assert!(s.showroom().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pointer's knobs live in [cursor], and its colours route there
    /// too — the picker's third home after theme tokens and the showroom.
    #[test]
    fn cursor_knobs_and_colours_write_the_cursor_table() {
        let (dir, s) = studio("cursor");
        act(&s, "/studio/actions/cur/size/up");
        assert_eq!(s.table("cursor").get("size").and_then(|v| v.as_float()), Some(24.0));
        for _ in 0..80 {
            act(&s, "/studio/actions/cur/size/up");
        }
        assert_eq!(
            s.table("cursor").get("size").and_then(|v| v.as_float()),
            Some(96.0),
            "size clamps where a pointer stops being a pointer"
        );
        act(&s, "/studio/actions/cursor-draw");
        assert_eq!(s.table("cursor").get("draw").and_then(|v| v.as_bool()), Some(false));

        act(&s, "/studio/actions/target/cur:color");
        act(&s, "/studio/actions/pick/ff8800");
        assert_eq!(
            s.table("cursor").get("color").and_then(|v| v.as_str()),
            Some("#ff8800"),
            "a cursor colour lands in [cursor], not [colors]"
        );
        assert!(s.table("colors").get("color").is_none());

        act(&s, "/studio/actions/cursor-reset");
        assert!(s.table("cursor").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The dock is arranged by placing items in slots, one place each, with
    /// order as priority — so clicking a slot moves an item rather than
    /// adding a second copy of it.
    #[test]
    fn dock_items_move_between_slots_and_keep_an_order() {
        let (dir, s) = studio("dock");
        // Defaults, before anything is written.
        assert_eq!(s.dock_slot("left"), vec!["menu".to_string()]);
        assert_eq!(s.dock_slot("center"), vec!["clock".to_string()]);

        act(&s, "/studio/actions/dock-place/clock/right");
        assert!(s.dock_slot("center").is_empty(), "an item lives in one slot");
        assert_eq!(s.dock_slot("right"), vec!["clock".to_string()]);

        act(&s, "/studio/actions/dock-place/apps/right");
        assert_eq!(s.dock_slot("right"), vec!["clock".to_string(), "apps".to_string()]);
        act(&s, "/studio/actions/dock-move/apps/back");
        assert_eq!(
            s.dock_slot("right"),
            vec!["apps".to_string(), "clock".to_string()],
            "priority is position, so a nudge reorders"
        );

        act(&s, "/studio/actions/dock-place/apps/off");
        assert_eq!(s.dock_slot("right"), vec!["clock".to_string()]);

        act(&s, "/studio/actions/dock-clock/12h");
        assert_eq!(s.dock().get("clock").and_then(|v| v.as_str()), Some("12h"));
        act(&s, "/studio/actions/dock-date");
        assert_eq!(s.dock().get("clock_date").and_then(|v| v.as_bool()), Some(true));

        act(&s, "/studio/actions/dock-reset");
        assert!(s.dock().is_empty());
        assert_eq!(s.dock_slot("left"), vec!["menu".to_string()], "back to the default arrangement");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The model belongs to the showroom scene: choosing one records it in
    /// [desktop.showroom], and the live [desktop] slot the compositor reads
    /// follows the wallpaper — showroom brings the model on stage, any other
    /// wallpaper dismisses it.
    #[test]
    fn the_model_follows_the_showroom_wallpaper() {
        let (dir, s) = studio("model");
        // A model configured by hand still shows as the active choice.
        s.update_showroom(|t| {
            t.insert("model".into(), toml::Value::String("/tmp/car.obj".into()));
        })
        .unwrap();
        assert_eq!(s.model_choices().first().map(|(l, _)| l.as_str()), Some("car"));

        act(&s, "/studio/actions/wall/showroom");
        assert_eq!(
            s.table("desktop").get("model").and_then(|v| v.as_str()),
            Some("/tmp/car.obj"),
            "the showroom brings the scene's model on stage"
        );
        assert!(s.table("desktop").get("model_shader").is_some(), "and dresses it");

        act(&s, "/studio/actions/wall/ocean");
        assert!(
            s.table("desktop").get("model").is_none(),
            "another wallpaper dismisses the model"
        );
        assert_eq!(
            s.showroom().get("model").and_then(|v| v.as_str()),
            Some("/tmp/car.obj"),
            "but the scene remembers its model"
        );

        act(&s, "/studio/actions/wall/showroom");
        assert!(s.table("desktop").get("model").is_some(), "and it returns with the scene");
        act(&s, "/studio/actions/model/none");
        assert!(s.showroom().get("model").is_none());
        assert!(s.table("desktop").get("model").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wallpapers come off disk, so a new one is a file and nothing else.
    /// The list must hold three lines at once: every entry is a real,
    /// non-empty shader; effects that sample the composited desktop are not
    /// wallpapers; and a shader dropped in with no registry entry anywhere
    /// still shows up and can be chosen.
    #[test]
    fn wallpapers_are_found_on_disk_not_declared() {
        let walls = wall_choices();
        assert!(walls.len() > 1, "found {walls:?}");

        for (stem, path) in &walls {
            let src = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("wallpaper {stem}: {} — {e}", path.display()));
            assert!(!src.trim().is_empty(), "wallpaper {stem} is empty");
            assert!(!samples_the_scene(&src), "{stem} is an effect, not a wallpaper");
        }
        for (stem, _) in effect_choices() {
            assert!(!walls.iter().any(|(s, _)| *s == stem), "effect {stem} listed as a wallpaper");
        }

        // Nothing names these anywhere: they are chips because the files
        // exist and paint rather than filter.
        assert!(walls.iter().any(|(s, _)| s == "matrix"), "{walls:?}");
        assert!(walls.iter().any(|(s, _)| s == "lofi"), "{walls:?}");

        let (dir, s) = studio("walls");
        act(&s, "/studio/actions/wall/matrix");
        let shader = s
            .table("desktop")
            .get("background_shader")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(shader.ends_with("matrix.wgsl"), "chose {shader}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window fire samples the desktop it burns over, so it is a grader
    /// however much it looks like scenery — the split follows what a shader
    /// does, and moving a file between the two grids is a code change inside
    /// the shader, not an edit to a list.
    #[test]
    fn the_window_fire_is_a_grader() {
        let effects = effect_choices();
        assert!(effects.iter().any(|(s, _)| s == "procedural_fire"), "{effects:?}");
        assert!(!wall_choices().iter().any(|(s, _)| s == "procedural_fire"));

        let (dir, s) = studio("fire");
        act(&s, "/studio/actions/shader/procedural_fire");
        let shader = s
            .table("desktop")
            .get("shader")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(shader.ends_with("procedural_fire.wgsl"), "chose {shader}");
        assert!(
            s.table("desktop").get("warp_barrel").is_none(),
            "only the CRT bends input; the fire must not"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whitespace must not decide what a shader is: the same sample written
    /// across three lines is still a read of the composited desktop.
    #[test]
    fn effects_are_told_apart_by_what_they_read() {
        assert!(samples_the_scene("let c = textureSample(scene, scene_samp, in.uv);"));
        assert!(samples_the_scene("textureSample(\n    scene,\n    scene_samp,\n    uv\n)"));
        assert!(!samples_the_scene("// paints the scene from nothing\nlet sky = 1.0;"));
    }

    /// A model may ship scene hints beside it as `<stem>.toml`: choosing it
    /// applies its orientation and framing, so a new mesh arrives standing
    /// up rather than needing three knobs hunted down by hand.
    #[test]
    fn choosing_a_model_applies_its_scene_hints() {
        let (dir, s) = studio("hints");
        let mesh = dir.join("figure.obj");
        std::fs::write(&mesh, "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").unwrap();
        std::fs::write(
            dir.join("figure.toml"),
            "model_up = \"-y\"\nmodel_scale = 1.4\nspin_phase = 2.0\n",
        )
        .unwrap();
        s.update_showroom(|t| {
            t.insert("model".into(), toml::Value::String(mesh.display().to_string()));
        })
        .unwrap();
        // Index 0 is the configured model; re-selecting it applies the hints.
        act(&s, "/studio/actions/model/0");
        let sr = s.showroom();
        assert_eq!(sr.get("model_up").and_then(|v| v.as_str()), Some("-y"));
        assert_eq!(sr.get("model_scale").and_then(|v| v.as_float()), Some(1.4));
        assert_eq!(sr.get("spin_phase").and_then(|v| v.as_float()), Some(2.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Density is desktop-wide: the steppers write [metrics], and
    /// `Metrics::from_theme_file` — what every kit app reads — sees it.
    #[test]
    fn density_lands_in_the_theme_and_clamps() {
        let (dir, s) = studio("density");
        act(&s, "/studio/actions/density/spacious");
        let m = rill_appkit::Metrics::from_theme_file(&s.theme_path);
        assert_eq!((m.font_size, m.padding), (18.0, 10.0));
        for _ in 0..30 {
            act(&s, "/studio/actions/f/up");
            act(&s, "/studio/actions/p/down");
        }
        let m = rill_appkit::Metrics::from_theme_file(&s.theme_path);
        assert_eq!((m.font_size, m.padding), (F_RANGE.1, P_RANGE.0));
        // The page itself compiles at the extremes it can reach.
        assert!(s.page().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Picks set rgb and keep alpha; the alpha stepper writes #rrggbbaa;
    /// full alpha collapses back to 6 digits.
    #[test]
    fn the_picker_keeps_alpha_and_alpha_steps_write_it() {
        let (dir, s) = studio("picker");
        act(&s, "/studio/actions/target/surface");
        act(&s, "/studio/actions/alpha/down"); // 255 → 239
        act(&s, "/studio/actions/pick/336699");
        let colors = s.table("colors");
        assert_eq!(
            colors.get("surface").and_then(|v| v.as_str()),
            Some("#336699ef"),
            "hue pick preserved the stepped alpha"
        );
        act(&s, "/studio/actions/alpha/up");
        assert_eq!(
            s.table("colors").get("surface").and_then(|v| v.as_str()),
            Some("#336699"),
            "full alpha collapses to 6 digits"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Palette chips write the full token set; garbage hex never lands in
    /// the file; glass knobs write [window]; Reset clears all three tables.
    #[test]
    fn writes_land_and_reset_clears_everything() {
        let (dir, s) = studio("writes");
        let anon = Identity::Anonymous;
        act(&s, "/studio/actions/palette/Midnight");
        assert_eq!(
            s.table("colors").get("accent").and_then(|v| v.as_str()),
            Some("#6ea8ff")
        );
        let val = |v: &str| [("value".to_string(), ActionValue::Str(v.into()))];
        s.action("/studio/actions/set/accent", &val("not-a-color"), &anon).unwrap();
        assert_eq!(
            s.table("colors").get("accent").and_then(|v| v.as_str()),
            Some("#6ea8ff"),
            "garbage left the file untouched"
        );
        act(&s, "/studio/actions/win/blur/up");
        assert_eq!(s.table("window").get("blur").and_then(|v| v.as_float()), Some(32.0));
        act(&s, "/studio/actions/reset");
        assert!(s.table("colors").is_empty());
        assert!(s.table("metrics").is_empty());
        assert!(s.table("window").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

