//! The desktop theme: `~/.config/rill/theme.toml` → a token table plus the
//! shell-level bits (wallpaper, focus glow, bundled fonts). This is the
//! dotfile side of `specs/theming.md` — declarative data, no scripting;
//! dynamic themes are generated *into* this file, never embedded as code.
//!
//! Colors are `#rrggbb[aa]`. Every `[colors]` entry becomes a semantic token
//! an app can reference (`color=accent`, `background=surface`); a few also
//! seed the renderer roles (page background, body text, links).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rill_doc::Color;
use rill_ui::Defaults;

/// The numbers a window is built out of. These lived as constants in the
/// window hosts, which meant every question about how the desktop should look
/// was answered by a rebuild. They are theme data: edit `[window]` in
/// `theme.toml`, save, and every open window re-skins.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowStyle {
    /// How much of the frost shows through chrome — the titlebar, an app's
    /// sidebar and toolbars. 0 = invisible chrome, 255 = solid.
    pub chrome_alpha: u8,
    /// The same for chrome's raised states (hover, selection).
    pub chrome_raised_alpha: u8,
    /// How solid a glass window's body is *before* the document paints on it.
    /// Chrome is painted over this, so opacity spent here is opacity chrome
    /// can never get back — keep it low and let pages that want to be solid
    /// say so themselves.
    pub glass_body_alpha: u8,
    /// Corner radius of a glass window.
    pub radius: f32,
    /// Titlebar height: the taller step is used when a document claims the
    /// bar with a `titlebar {}` node, because then it holds controls.
    pub titlebar: f32,
    pub titlebar_tall: f32,
    /// Frost blur radius behind a glass window.
    pub blur: f32,
}

impl Default for WindowStyle {
    fn default() -> WindowStyle {
        WindowStyle {
            chrome_alpha: 0x4a,
            chrome_raised_alpha: 0xb4,
            glass_body_alpha: 0x3c,
            radius: 14.0,
            titlebar: 34.0,
            titlebar_tall: 44.0,
            blur: 28.0,
        }
    }
}

impl WindowStyle {
    fn parse(&mut self, table: &toml::Table) {
        let num = |key: &str| -> Option<f64> {
            table
                .get(key)
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
        };
        let alpha = |key: &str, out: &mut u8| {
            if let Some(n) = num(key) {
                *out = n.clamp(0.0, 255.0) as u8;
            }
        };
        alpha("chrome_alpha", &mut self.chrome_alpha);
        alpha("chrome_raised_alpha", &mut self.chrome_raised_alpha);
        alpha("glass_body_alpha", &mut self.glass_body_alpha);
        for (key, out, hi) in [
            ("radius", &mut self.radius, 64.0),
            ("titlebar", &mut self.titlebar, 120.0),
            ("titlebar_tall", &mut self.titlebar_tall, 120.0),
            ("blur", &mut self.blur, 128.0),
        ] {
            if let Some(n) = num(key) {
                *out = (n as f32).clamp(0.0, hi);
            }
        }
    }
}

/// A fully resolved desktop theme.
#[derive(Debug, Clone)]
pub struct DesktopTheme {
    /// The token table apps resolve against (also the renderer defaults).
    pub defaults: Defaults,
    /// Desktop wallpaper image, if any.
    pub wallpaper: Option<PathBuf>,
    /// Focus-glow color painted around the active window (None = no glow).
    pub glow: Option<Color>,
    /// Directory of bundled `.ttf`/`.otf` fonts to register at startup.
    pub fonts_dir: Option<PathBuf>,
    /// Per-window opacity (1.0 = opaque). The shell composites each window at
    /// this alpha so the wallpaper and windows behind show through — glass.
    pub window_opacity: f32,
    /// Fingerprint of the `[metrics]` table (0 when absent). Density is
    /// baked into *served* pages, so a change here means "refetch to
    /// re-densify" — hosts compare fingerprints across theme reloads and
    /// refresh only then, leaving color-only edits free to re-skin live
    /// without disturbing in-progress input.
    pub metrics_fingerprint: u64,
    /// Glass windows (`[desktop] glass`): vector windows frost the desktop
    /// behind their whole surface. Theme data like everything else — the
    /// studio toggles it, every process re-reads it on the theme watch.
    pub glass: bool,
    /// How windows themselves are drawn — chrome opacity, radius, bar height.
    pub window: WindowStyle,
}

impl Default for DesktopTheme {
    fn default() -> DesktopTheme {
        builtin_dark()
    }
}

fn color(v: &toml::Value) -> Option<Color> {
    Color::parse_hex(v.as_str()?)
}

/// Derive the elevation steps from the surfaces when a palette doesn't name
/// them. Same argument as [`derive_chrome`]: the kit paints hover and
/// selection as elevation, so a palette that forgot the steps would have
/// invisible hover states — derived, every palette lifts. `lg` leans the
/// raised surface toward the text color, which is what "closer to the
/// light" means on dark *and* light grounds.
fn derive_elevation(colors: &mut HashMap<String, Color>) {
    let Some(surface) = colors.get("surface").copied() else { return };
    let raised = colors.get("surface-raised").copied().unwrap_or(surface);
    let text = colors.get("text").copied().unwrap_or(Color { r: 128, g: 128, b: 128, a: 255 });
    let lerp = |a: Color, b: Color, t: f32| Color {
        r: (a.r as f32 + (b.r as f32 - a.r as f32) * t) as u8,
        g: (a.g as f32 + (b.g as f32 - a.g as f32) * t) as u8,
        b: (a.b as f32 + (b.b as f32 - a.b as f32) * t) as u8,
        a: a.a,
    };
    colors.entry("elevation-sm".to_string()).or_insert(surface);
    colors.entry("elevation-md".to_string()).or_insert(raised);
    colors.entry("elevation-lg".to_string()).or_insert(lerp(raised, text, 0.14));
}

/// Derive the chrome surfaces from a palette. A titlebar and a sidebar are the
/// same surface, and on a glass window that surface is *translucent* — the
/// frost behind the window is the point, and a solid panel buries it. On a
/// host with no backdrop the same fill reads as a lighter panel, so nothing
/// has to know which kind of window it is in.
///
/// Derived for every palette rather than listed per-palette: one that forgot
/// `chrome` would give one app glass chrome and its neighbour an opaque slab.
/// A palette naming them explicitly keeps its own.
fn derive_chrome(colors: &mut HashMap<String, Color>, window: &WindowStyle) {
    let Some(base) = colors.get("surface-raised").or_else(|| colors.get("surface")).copied()
    else {
        return;
    };
    let alphas = [("chrome", window.chrome_alpha), ("chrome-raised", window.chrome_raised_alpha)];
    for (name, alpha) in alphas {
        colors.entry(name.to_string()).or_insert(Color { a: alpha, ..base });
    }
}

/// A built-in dark theme, used when no `theme.toml` is present so tokens
/// always resolve to something sensible.
pub fn builtin_dark() -> DesktopTheme {
    let mut colors: HashMap<String, Color> = HashMap::new();
    let mut put = |name: &str, hex: &str| {
        colors.insert(name.to_string(), Color::parse_hex(hex).unwrap());
    };
    put("accent", "#7c5cff");
    put("accent-text", "#ffffff");
    put("surface", "#1b1b28");
    put("surface-raised", "#242438");
    put("text", "#e8e8f0");
    put("text-muted", "#9a9ab0");
    put("border", "#33334a");
    put("page", "#121219");
    // Each elevation step carries a surface as well as a shadow. On a dark
    // page a black shadow has almost nothing to darken, so depth that is only
    // a shadow barely reads; lifting the surface is what actually says
    // "closer". Named by step so a style asking for shadow="md" gets both.
    put("elevation-sm", "#1b1b28");
    put("elevation-md", "#242438");
    put("elevation-lg", "#2c2c44");
    let window = WindowStyle::default();
    derive_chrome(&mut colors, &window);

    let defaults = Defaults {
        page_background: colors["page"],
        text_color: colors["text"],
        link_color: colors["accent"],
        font_size: 15.0,
        color_tokens: colors,
        ..Defaults::default()
    };
    DesktopTheme {
        defaults,
        wallpaper: None,
        glow: Color::parse_hex("#7c5cff"),
        fonts_dir: None,
        window_opacity: 1.0,
        glass: false,
        metrics_fingerprint: 0,
        window,
    }
}

/// A swappable colour palette: the token table plus the focus-glow colour.
/// The name is for the switcher UI. Swapping the palette re-skins every
/// token-referencing surface at once (the "swap the table → everything
/// re-renders" property from §1) while wallpaper and fonts stay put.
#[derive(Debug, Clone)]
pub struct Palette {
    pub name: String,
    pub colors: HashMap<String, Color>,
    pub glow: Option<Color>,
}

fn palette(name: &str, pairs: &[(&str, &str)]) -> Palette {
    let colors = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Color::parse_hex(v).unwrap()))
        .collect::<HashMap<_, _>>();
    let glow = colors.get("accent").copied();
    Palette { name: name.to_string(), colors, glow }
}

/// The built-in palettes the shell's switcher cycles through.
pub fn palettes() -> Vec<Palette> {
    vec![
        // Greyscale first: if the layout does not read without colour, colour
        // was doing load-bearing work it should not. One hue (none); the
        // accent is pure lightness, so selection reads as *closer to white*.
        // The working palette while sizing, scaling and placement are being
        // mastered — the coloured palettes are what it graduates back into.
        palette(
            "Mono",
            &[
                ("page", "#131313"), ("surface", "#1d1d1d"), ("surface-raised", "#282828"),
                ("text", "#ececec"), ("text-muted", "#969696"), ("accent", "#e0e0e0"),
                ("accent-text", "#131313"), ("border", "#3a3a3a"),
                ("elevation-sm", "#1d1d1d"), ("elevation-md", "#282828"),
                ("elevation-lg", "#323232"),
            ],
        ),
        palette(
            "Midnight",
            &[
                ("page", "#0e1020"), ("surface", "#161a2e"), ("surface-raised", "#212747"),
                ("text", "#e9ecff"), ("text-muted", "#8b90b8"), ("accent", "#6ea8ff"),
                ("accent-text", "#0b1024"), ("border", "#2b315a"),
            ],
        ),
        palette(
            "Dusk",
            &[
                ("page", "#1a1020"), ("surface", "#241528"), ("surface-raised", "#3a2142"),
                ("text", "#f6e9ff"), ("text-muted", "#b892c8"), ("accent", "#ff7ac6"),
                ("accent-text", "#1a1020"), ("border", "#4a2e52"),
            ],
        ),
        palette(
            "Forest",
            &[
                ("page", "#0c1613"), ("surface", "#111f1a"), ("surface-raised", "#1a2f26"),
                ("text", "#e6f4ec"), ("text-muted", "#84a795"), ("accent", "#5fd39a"),
                ("accent-text", "#08130d"), ("border", "#274539"),
            ],
        ),
        palette(
            "Paper",
            &[
                ("page", "#f4f1ea"), ("surface", "#ffffff"), ("surface-raised", "#ece7db"),
                ("text", "#2a2620"), ("text-muted", "#6b6558"), ("accent", "#b4552d"),
                ("accent-text", "#ffffff"), ("border", "#d8d2c4"),
            ],
        ),
        palette(
            "Synthwave",
            &[
                ("page", "#160b2e"), ("surface", "#1f1140"), ("surface-raised", "#33195e"),
                ("text", "#f2e7ff"), ("text-muted", "#a48ad4"), ("accent", "#00e5ff"),
                ("accent-text", "#12082a"), ("border", "#ff2ea6"),
            ],
        ),
        palette(
            "Ember",
            &[
                ("page", "#171310"), ("surface", "#211a15"), ("surface-raised", "#33271d"),
                ("text", "#f7ede2"), ("text-muted", "#b39a84"), ("accent", "#ff9d45"),
                ("accent-text", "#1c130b"), ("border", "#4a382a"),
            ],
        ),
    ]
}

/// Re-seed a token table and renderer roles from a palette, in place.
pub fn apply_palette(defaults: &mut Defaults, palette: &Palette, window: &WindowStyle) {
    defaults.color_tokens = palette.colors.clone();
    derive_elevation(&mut defaults.color_tokens);
    derive_chrome(&mut defaults.color_tokens, window);
    if let Some(c) = palette.colors.get("page") {
        defaults.page_background = *c;
    }
    if let Some(c) = palette.colors.get("text") {
        defaults.text_color = *c;
    }
    if let Some(c) = palette.colors.get("accent") {
        defaults.link_color = *c;
    }
}

/// Load a theme from `path`, falling back to [`builtin_dark`] for anything the
/// file omits or gets wrong (a theme should never fail a desktop to boot).
pub fn load(path: &Path) -> DesktopTheme {
    match std::fs::read_to_string(path) {
        Ok(text) => from_toml(&text),
        Err(_) => builtin_dark(),
    }
}

/// Parse theme TOML text over the built-in dark base — every field the text
/// omits or malforms keeps its built-in value.
pub fn from_toml(text: &str) -> DesktopTheme {
    let Ok(root) = text.parse::<toml::Table>() else { return builtin_dark() };
    let mut theme = builtin_dark();

    // Parsed before the colours, because the chrome tokens are derived from
    // the alphas it carries.
    if let Some(table) = root.get("window").and_then(|v| v.as_table()) {
        theme.window.parse(table);
        theme.defaults.color_tokens.remove("chrome");
        theme.defaults.color_tokens.remove("chrome-raised");
        derive_chrome(&mut theme.defaults.color_tokens, &theme.window);
    }

    if let Some(colors) = root.get("colors").and_then(|v| v.as_table()) {
        for (name, value) in colors {
            if let Some(c) = color(value) {
                theme.defaults.color_tokens.insert(name.clone(), c);
            }
        }
        // A theme that re-skins `surface-raised` must re-derive the chrome
        // (and the elevation steps) that were built from the *old* one, or
        // its windows keep the built-in tint. Explicit entries still win.
        if colors.contains_key("surface-raised") || colors.contains_key("surface") {
            theme.defaults.color_tokens.remove("chrome");
            theme.defaults.color_tokens.remove("chrome-raised");
            for step in ["elevation-sm", "elevation-md", "elevation-lg"] {
                if !colors.contains_key(step) {
                    theme.defaults.color_tokens.remove(step);
                }
            }
        }
        derive_elevation(&mut theme.defaults.color_tokens);
        derive_chrome(&mut theme.defaults.color_tokens, &theme.window);
        // Re-seed renderer roles from the (possibly overridden) tokens.
        let tok = &theme.defaults.color_tokens;
        if let Some(c) = tok.get("page") {
            theme.defaults.page_background = *c;
        }
        if let Some(c) = tok.get("text") {
            theme.defaults.text_color = *c;
        }
        if let Some(c) = tok.get("accent") {
            theme.defaults.link_color = *c;
        }
    }

    if let Some(metrics) = root.get("metrics").and_then(|v| v.as_table()) {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        for (k, v) in metrics {
            k.hash(&mut h);
            v.to_string().hash(&mut h);
        }
        theme.metrics_fingerprint = h.finish();
    }

    // The desktop-behavior knobs the studio writes: glass and the enforced
    // override are theme data, not process state — one file, one watcher.
    if let Some(desktop) = root.get("desktop").and_then(|v| v.as_table()) {
        if let Some(g) = desktop.get("glass").and_then(|v| v.as_bool()) {
            theme.glass = g;
        }
        if let Some(e) = desktop.get("enforce").and_then(|v| v.as_bool()) {
            theme.defaults.enforce = e;
        }
    }

    if let Some(fonts) = root.get("fonts").and_then(|v| v.as_table()) {
        for (name, value) in fonts {
            if name == "default_size" {
                if let Some(n) = value.as_float().or_else(|| value.as_integer().map(|i| i as f64)) {
                    theme.defaults.font_size = n as f32;
                }
            } else if let Some(family) = value.as_str() {
                theme.defaults.font_tokens.insert(name.clone(), family.to_string());
            }
        }
        // The `ui` token also becomes the default body family.
        if let Some(ui) = theme.defaults.font_tokens.get("ui") {
            theme.defaults.font_family = ui.clone();
        }
    }

    if let Some(desktop) = root.get("desktop").and_then(|v| v.as_table()) {
        theme.wallpaper = desktop.get("wallpaper").and_then(|v| v.as_str()).map(PathBuf::from);
        theme.fonts_dir = desktop.get("fonts_dir").and_then(|v| v.as_str()).map(PathBuf::from);
        if let Some(v) = desktop.get("glow") {
            // An explicit empty string disables the glow.
            theme.glow = if v.as_str() == Some("") { None } else { color(v) };
        }
        if let Some(v) = desktop.get("window_opacity")
            && let Some(n) = v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
        {
            theme.window_opacity = (n as f32).clamp(0.2, 1.0);
        }
    }

    theme
}

/// Look up a built-in palette by name.
pub fn palette_by_name(name: &str) -> Option<Palette> {
    palettes().into_iter().find(|p| p.name == name)
}

impl DesktopTheme {
    /// Layer a runtime palette + enforce flag on top of this theme, keeping the
    /// wallpaper and fonts. This is how a live theme change reaches an
    /// already-loaded process.
    pub fn apply_runtime(&mut self, palette: &str, enforce: bool) {
        if let Some(p) = palette_by_name(palette) {
            let window = self.window.clone();
            apply_palette(&mut self.defaults, &p, &window);
            self.glow = p.glow;
        }
        self.defaults.enforce = enforce;
    }
}

/// The retired runtime sidecar (`theme.runtime`). Palette/override used to
/// be broadcast here by the dock *over* the theme file — which silently
/// clobbered a studio-written `[colors]` on every reload. `theme.toml` is
/// now the single source of truth; the dock deletes any stale sidecar at
/// startup so old broadcasts stop applying.
pub fn stale_runtime_sidecar(theme_path: &Path) -> PathBuf {
    theme_path.with_extension("runtime")
}

/// What a live host polls to notice an appearance change: the theme file and
/// the one theme file (the sidecar is retired — `theme.toml` is the single
/// source of truth).
pub type ThemeStamp = Option<std::time::SystemTime>;

pub fn stamp(theme_path: &Path) -> ThemeStamp {
    file_mtime(theme_path)
}

/// Modification time of a file, for cheap change-polling.
pub fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The default theme path, `~/.config/rill/theme.toml`.
pub fn default_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| PathBuf::from(".config"));
    base.join("rill").join("theme.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_toml_populates_tokens_roles_and_desktop() {
        let t = from_toml(
            r##"
            [colors]
            page = "#101018"
            text = "#eeeeee"
            accent = "#ff8800"
            surface = "#202030"
            [fonts]
            ui = "Comfortaa"
            mono = "Fira Code"
            default_size = 17
            [desktop]
            wallpaper = "/w.png"
            glow = "#ff8800"
        "##,
        );
        // Tokens are all queryable.
        assert_eq!(t.defaults.token("accent"), Color::parse_hex("#ff8800"));
        assert_eq!(t.defaults.token("surface"), Color::parse_hex("#202030"));
        // Renderer roles are seeded from the tokens.
        assert_eq!(t.defaults.page_background, Color::parse_hex("#101018").unwrap());
        assert_eq!(t.defaults.text_color, Color::parse_hex("#eeeeee").unwrap());
        assert_eq!(t.defaults.link_color, Color::parse_hex("#ff8800").unwrap());
        // Fonts: tokens map, `ui` also becomes the default family, size applies.
        assert_eq!(t.defaults.font_tokens.get("ui").map(String::as_str), Some("Comfortaa"));
        assert_eq!(t.defaults.font_family, "Comfortaa");
        assert_eq!(t.defaults.font_size, 17.0);
        // Desktop bits.
        assert_eq!(t.wallpaper.as_deref(), Some(Path::new("/w.png")));
        assert_eq!(t.glow, Color::parse_hex("#ff8800"));
    }

    #[test]
    fn malformed_toml_falls_back_to_builtin() {
        let t = from_toml("this is not = = toml");
        // Built-in dark still resolves its tokens.
        assert!(t.defaults.token("accent").is_some());
        assert!(t.defaults.token("surface").is_some());
    }

    #[test]
    fn empty_glow_string_disables_glow() {
        let t = from_toml("[desktop]\nglow = \"\"\n");
        assert_eq!(t.glow, None);
    }

    /// The numbers a window is built out of are theme data, not constants —
    /// otherwise every question about how the desktop should look is answered
    /// by a rebuild.
    #[test]
    fn window_table_drives_chrome() {
        let t = from_toml("[window]\nchrome_alpha = 0x20\nradius = 6\ntitlebar_tall = 52\n");
        assert_eq!(t.window.chrome_alpha, 0x20);
        assert_eq!(t.window.radius, 6.0);
        assert_eq!(t.window.titlebar_tall, 52.0);
        // Untouched keys keep the built-in value rather than resetting.
        assert_eq!(t.window.blur, WindowStyle::default().blur);
        // And the derived token follows the alpha, since that is the whole
        // point of it being in the table.
        assert_eq!(t.defaults.token("chrome").unwrap().a, 0x20);
    }

    /// A palette swap must not resurrect the built-in chrome alpha.
    #[test]
    fn a_palette_swap_keeps_the_themes_chrome_alpha() {
        let mut t = from_toml("[window]\nchrome_alpha = 0x11\n");
        t.apply_runtime("Dusk", false);
        assert_eq!(t.defaults.token("chrome").unwrap().a, 0x11);
        let dusk = palette_by_name("Dusk").unwrap();
        let raised = dusk.colors["surface-raised"];
        assert_eq!(t.defaults.token("chrome").unwrap().r, raised.r, "and the palette's hue");
    }

    /// Watching the sidecar alone meant editing the theme did nothing until
    /// the dock happened to rewrite it.
    #[test]
    fn the_stamp_covers_the_theme_file_itself() {
        let dir = std::env::temp_dir().join("rill-theme-stamp-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("theme.toml");
        std::fs::write(&path, "[window]\nblur = 4\n").unwrap();
        let before = stamp(&path);
        assert!(before.is_some(), "the theme file is watched");
        std::fs::write(&path, "[window]\nblur = 5\n").unwrap();
        assert_ne!(stamp(&path), before, "an edit to the theme is a change");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
