//! The dock as a vector-native window (`rill-vector --dock`): the Rill mark
//! in the corner opening the app menu, and the installed apps as launchers.
//! `/~launch/…` links are handled locally by spawning an app process.
//!
//! Deliberately *only* a launcher now. The ricing controls (palette, glass,
//! shaders, wallpaper, boids, stats, override) moved into Theme Studio,
//! which writes `theme.toml` — the single source of truth every process
//! watches. The runtime sidecar the dock used to broadcast is retired: it
//! applied a palette *over* the theme file, silently clobbering studio
//! edits; any stale sidecar is deleted at startup so old broadcasts stop
//! applying.

use std::path::PathBuf;

use rill_viewport::theme;

/// How the strip is laid out: what sits in each slot, in order — the order
/// *is* the priority, and slots are simply left, centre and right.
/// `[desktop.dock]` in theme.toml:
///
/// ```toml
/// [desktop.dock]
/// left = ["menu"]
/// center = ["clock"]
/// right = []
/// clock = "24h"        # or "12h", "off"
/// clock_date = false
/// background = "glass" # or "solid", "none"
/// height = 44
/// padding = 6
/// corner = 0
/// gap = 6
/// icon = 26
/// ```
struct DockLayout {
    left: Vec<String>,
    center: Vec<String>,
    right: Vec<String>,
    clock: ClockStyle,
    clock_date: bool,
}

#[derive(PartialEq)]
enum ClockStyle {
    Off,
    H24,
    H12,
}

impl Default for DockLayout {
    fn default() -> DockLayout {
        DockLayout {
            left: vec!["menu".into()],
            center: vec!["clock".into()],
            right: Vec::new(),
            clock: ClockStyle::H24,
            clock_date: false,
        }
    }
}

impl DockLayout {
    fn load(theme_path: &std::path::Path) -> DockLayout {
        let mut out = DockLayout::default();
        let Some(t) = std::fs::read_to_string(theme_path)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get("desktop")?.get("dock")?.as_table().cloned())
        else {
            return out;
        };
        let slot = |key: &str| -> Option<Vec<String>> {
            Some(
                t.get(key)?
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            )
        };
        if let Some(v) = slot("left") {
            out.left = v;
        }
        if let Some(v) = slot("center") {
            out.center = v;
        }
        if let Some(v) = slot("right") {
            out.right = v;
        }
        out.clock = match t.get("clock").and_then(|v| v.as_str()) {
            Some("off") => ClockStyle::Off,
            Some("12h") => ClockStyle::H12,
            _ => ClockStyle::H24,
        };
        out.clock_date = t.get("clock_date").and_then(|v| v.as_bool()).unwrap_or(false);
        out
    }
}

/// What the strip is made of.
///
/// `Glass` is the window material: frost, the page tint, the `chrome`
/// token — the same three layers a titlebar is, which is what makes the two
/// read as one surface. `Solid` is the same strip with the frost taken away
/// and the page colour left opaque behind it, for a desktop that would
/// rather have an edge than a pane. `None` paints nothing at all: the
/// launcher and the clock float on the wallpaper.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DockBackground {
    Glass,
    Solid,
    None,
}

/// The strip's shape and material — `[desktop.dock]`, with the density
/// (F and P) as the defaults so a dock nobody has configured still follows
/// the desktop it lives on.
#[derive(Clone, Copy, Debug)]
pub struct DockStyle {
    pub background: DockBackground,
    /// The strip's height in px. The *compositor* owns this — it reserves
    /// the space and keeps windows out of it — so it reads the same key.
    pub height: f32,
    pub padding: f32,
    pub corner: f32,
    pub gap: f32,
    /// Square size of a launcher button.
    pub icon: f32,
}

impl DockStyle {
    /// Read `[desktop.dock]`, falling back to the theme's density.
    pub fn load(theme_path: &std::path::Path) -> DockStyle {
        let m = rill_appkit::Metrics::from_theme_file(theme_path);
        let mut out = DockStyle {
            background: DockBackground::Glass,
            height: DEFAULT_DOCK_HEIGHT,
            padding: m.padding,
            corner: 0.0,
            gap: m.padding,
            icon: m.icon_button(),
        };
        let Some(t) = std::fs::read_to_string(theme_path)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get("desktop")?.get("dock")?.as_table().cloned())
        else {
            return out;
        };
        out.background = match t.get("background").and_then(|v| v.as_str()) {
            Some("solid") => DockBackground::Solid,
            Some("none") => DockBackground::None,
            _ => DockBackground::Glass,
        };
        let num = |key: &str| -> Option<f32> {
            t.get(key)
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .map(|n| n as f32)
        };
        // Clamped, because these come from a file a person edits: a dock
        // 4000px tall is a desktop with no room left in it.
        if let Some(v) = num("height") {
            out.height = v.clamp(20.0, 200.0);
        }
        if let Some(v) = num("padding") {
            out.padding = v.clamp(0.0, 40.0);
        }
        if let Some(v) = num("corner") {
            out.corner = v.clamp(0.0, 40.0);
        }
        if let Some(v) = num("gap") {
            out.gap = v.clamp(0.0, 40.0);
        }
        if let Some(v) = num("icon") {
            out.icon = v.clamp(12.0, 96.0);
        }
        // An icon cannot be taller than the strip that holds it. The two
        // numbers come from the same hand-edited file and nothing else ties
        // them together, so a 26px icon in a 20px dock is one typo away —
        // and the compositor reserves exactly `height`, so the overflow
        // draws under whatever window sits above the strip.
        out.icon = out.icon.min(out.height);
        out
    }
}

/// The strip's height when nothing says otherwise. Shared with the
/// compositor, which reserves the space.
pub const DEFAULT_DOCK_HEIGHT: f32 = 44.0;

/// Local wall-clock parts: (hour, minute, weekday 0=Sunday, day, month).
fn local_now() -> (u32, u32, u32, u32, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    (
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_wday as u32,
        tm.tm_mday as u32,
        tm.tm_mon as u32,
    )
}

/// One `[[desktop.widgets]]` entry: what to show, and where.
struct WidgetEntry {
    app: String,
    place: String,
}

/// The app id the compositor parks on the desktop, below every window. A
/// widget appends `#<anchor>:<w>x<h>+<x>+<y>` to it.
pub const WIDGET_APP_ID: &str = "rill-shell-widget";

/// The app id the compositor pins to the bottom edge (`reflow_shell`).
pub const DOCK_APP_ID: &str = "rill-shell-dock";

/// Launch plumbing for the dock: where apps are installed and what a spawned
/// window needs to connect (identity, cache, theme, picker root).
pub struct Dock {
    data_dir: PathBuf,
    theme_path: PathBuf,
    identity_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    pick_root: Option<PathBuf>,
    /// Spawned app processes, kept so exited children are reaped (no
    /// zombies).
    children: Vec<std::process::Child>,
    /// Widget processes, keyed by their `app =` URL so a theme edit can be
    /// diffed against what is actually running.
    widget_children: Vec<(String, std::process::Child)>,
}

impl Dock {
    pub fn new(
        data_dir: PathBuf,
        theme_path: PathBuf,
        identity_dir: PathBuf,
        cache_dir: Option<PathBuf>,
        pick_root: Option<PathBuf>,
    ) -> Dock {
        // Retire any stale runtime sidecar from an older dock — left in
        // place it would keep re-skinning every process over theme.toml.
        let _ = std::fs::remove_file(theme::stale_runtime_sidecar(&theme_path));
        Dock {
            data_dir,
            theme_path,
            identity_dir,
            cache_dir,
            pick_root,
            children: Vec::new(),
            widget_children: Vec::new(),
        }
    }

    /// Collect any app or widget children that have exited.
    ///
    /// Reaping used to happen only on the next launch (apps) or the next theme
    /// change (widgets), so an app you opened and closed stayed a zombie in
    /// the process table until you opened another one. Bounded, but visible to
    /// anyone running `ps` — and on an appliance the dock is the process that
    /// never exits, so it is the one that accumulates them.
    pub fn reap(&mut self) {
        self.children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        self.widget_children.retain_mut(|(_, c)| !matches!(c.try_wait(), Ok(Some(_))));
    }

    /// The token table the dock's own view resolves against — the theme
    /// file, nothing layered over it.
    pub fn themed_defaults(&self) -> rill_viewport::Defaults {
        theme::load(&self.theme_path).defaults
    }

    /// Spawn an installed app as its own vector window. The child inherits
    /// WAYLAND_DISPLAY, so it connects to the same compositor.
    fn launch(&mut self, key: &str) {
        self.children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        let host = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rill-vector"));
        let mut cmd = std::process::Command::new(host);
        cmd.arg("--app").arg(key).arg("--data").arg(&self.data_dir);
        cmd.arg("--identity").arg(&self.identity_dir);
        match &self.cache_dir {
            Some(dir) => {
                cmd.arg("--cache").arg(dir);
            }
            None => {
                cmd.arg("--no-cache");
            }
        }
        cmd.arg("--theme").arg(&self.theme_path);
        if let Some(root) = &self.pick_root {
            cmd.arg("--pick-root").arg(root);
        }
        match cmd.spawn() {
            Ok(child) => {
                println!("rill-vector: launched app {key:?}");
                self.children.push(child);
            }
            Err(e) => eprintln!("rill-vector: could not launch {key:?}: {e}"),
        }
    }

    /// Spawn the desktop's widgets at startup.
    ///
    /// The dock does this rather than the compositor because a widget is a
    /// *client*, and clients need what the dock already holds: the data
    /// directory, the device identity that the server trusts, the cache and
    /// the theme. A compositor that spawned them itself produced widgets
    /// that could not authenticate — they rendered "no pinned fingerprint"
    /// where the art should have been.
    pub fn spawn_widgets(&mut self) {
        for widget in self.widgets() {
            self.spawn_widget(&widget);
        }
    }

    /// Bring the running widgets back in line with `[[desktop.widgets]]`,
    /// called when the theme file changes: the studio's Add spawns a
    /// process, its Remove kills one.
    ///
    /// The diff is over the *set* of `app =` URLs, deliberately blind to
    /// anchor and position. Placement of an already-running widget is the
    /// compositor's to apply (it moves the live window), and the compositor
    /// also writes positions back to this same file on drag — a diff that
    /// keyed on position would kill and respawn a widget for the crime of
    /// having been dragged.
    pub fn sync_widgets(&mut self) {
        // Reap exited widget processes first, so a crashed widget's slot
        // reads as vacant instead of pinning its entry forever.
        self.widget_children
            .retain_mut(|(_, c)| !matches!(c.try_wait(), Ok(Some(_))));
        let want = self.widgets();
        // Two widgets can share a URL (two ASCII panes), so the diff counts
        // per URL rather than treating the URL as unique.
        let mut want_count: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for w in &want {
            *want_count.entry(w.app.as_str()).or_default() += 1;
        }
        // Kill the surplus: any running widget beyond its URL's wanted count.
        let mut kept: Vec<(String, std::process::Child)> = Vec::new();
        for (app, mut child) in self.widget_children.drain(..) {
            match want_count.get_mut(app.as_str()) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    kept.push((app, child));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    println!("rill-vector: widget {app} removed");
                }
            }
        }
        self.widget_children = kept;
        // Spawn the deficit: wanted entries with no process to their name.
        for widget in &want {
            let n = want_count.get_mut(widget.app.as_str()).expect("counted above");
            match *n {
                0 => {}
                _ => {
                    *n -= 1;
                    self.spawn_widget(widget);
                }
            }
        }
    }

    /// Spawn one widget process; its placement rides in the app id.
    fn spawn_widget(&mut self, widget: &WidgetEntry) {
        let host = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rill-vector"));
        let mut cmd = std::process::Command::new(host);
        cmd.arg("--widget").arg(&widget.app);
        cmd.arg("--widget-place").arg(&widget.place);
        cmd.arg("--data").arg(&self.data_dir);
        cmd.arg("--identity").arg(&self.identity_dir);
        match &self.cache_dir {
            Some(dir) => {
                cmd.arg("--cache").arg(dir);
            }
            None => {
                cmd.arg("--no-cache");
            }
        }
        cmd.arg("--theme").arg(&self.theme_path);
        match cmd.spawn() {
            Ok(child) => {
                println!("rill-vector: widget {} at {}", widget.app, widget.place);
                self.widget_children.push((widget.app.clone(), child));
            }
            Err(e) => eprintln!("rill-vector: could not spawn widget: {e}"),
        }
    }

    /// `[[desktop.widgets]]`, flattened into what a spawn needs.
    fn widgets(&self) -> Vec<WidgetEntry> {
        let Some(list) = std::fs::read_to_string(&self.theme_path)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get("desktop")?.get("widgets")?.as_array().cloned())
        else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|entry| {
                let t = entry.as_table()?;
                let app = t.get("app")?.as_str()?.to_string();
                let num = |key: &str, default: i64| -> i64 {
                    t.get(key).and_then(|v| v.as_integer()).unwrap_or(default)
                };
                let anchor = t.get("anchor").and_then(|v| v.as_str()).unwrap_or("top-right");
                Some(WidgetEntry {
                    app,
                    place: format!(
                        "{anchor}:{}x{}+{}+{}",
                        num("width", 320).clamp(16, 4000),
                        num("height", 140).clamp(16, 4000),
                        num("x", 16).clamp(0, 4000),
                        num("y", 16).clamp(0, 4000),
                    ),
                })
            })
            .collect()
    }

    /// Handle a dock link. Returns true when the dock consumed it.
    pub fn follow(&mut self, target: &str) -> bool {
        let Some(key) = target.strip_prefix("/~launch/") else { return false };
        let key = key.to_string();
        self.launch(&key);
        true
    }

    /// The strip's material and shape, read fresh from the theme. Callers
    /// cache it — the host asks once per theme change, not once per frame.
    pub fn style(&self) -> DockStyle {
        DockStyle::load(&self.theme_path)
    }

    /// The dock document: the slots the layout asks for, in the order it
    /// asks for them. The strip paints `chrome` — the same token a window's
    /// titlebar uses — so the dock reads as the top edge of the desktop
    /// rather than a different material floating above it.
    pub fn document(&self) -> Vec<u8> {
        let m = rill_appkit::Metrics::from_theme_file(&self.theme_path);
        let f = m.font_size;
        let style = DockStyle::load(&self.theme_path);
        let (p, ib) = (style.padding, style.icon);
        let layout = DockLayout::load(&self.theme_path);
        // Glass and Solid both wear `chrome`, the token a window's titlebar
        // uses; None wears nothing, and the host paints nothing behind it,
        // so the launcher and the clock sit straight on the wallpaper.
        let strip = match style.background {
            DockBackground::None => "#00000000",
            _ => "chrome",
        };
        // A page background of its own is how the strip tells the host what
        // is behind it: clear for a dock that paints nothing, so no body
        // tint is laid down under it either.
        let page = match style.background {
            DockBackground::None => "\t\t\tpage background=\"#00000000\"\n",
            _ => "",
        };
        let mut kdl = format!(
            "style \"dock\" background=\"{strip}\" gap={gap} padding=0 padding-x={p} valign=\"center\" height=\"fill\" corner={corner}\n\
             style \"dock-logo\" color=\"accent\" background=\"#00000000\" size={lh} corner=0 padding={p} width={ib} hover=\"dock-logo--hover\"\n\
             style \"dock-logo--hover\" color=\"accent\" background=\"chrome-raised\" size={lh} corner=0 padding={p} width={ib}\n\
             style \"dock-slot\" width={ib} padding=0 gap=0\n\
             style \"clock\" color=\"text\" size={f} font=\"mono\"\n\
             style \"clock-date\" color=\"text-muted\" size={quiet}\n\
             style \"muted\" color=\"text-muted\" size={quiet}\n\n\
             row style=\"dock\" {{\n{page}",
            lh = (style.icon * 0.62).round(),
            gap = style.gap,
            corner = style.corner,
            quiet = f - 3.0,
        );
        let apps = rill_app::InstallStore::open(&self.data_dir)
            .ok()
            .and_then(|s| s.list().ok())
            .unwrap_or_default();

        // One item, whatever slot it was placed in.
        let item = |name: &str, kdl: &mut String| match name {
            "menu" => {
                kdl.push_str(
                    "\trow style=\"dock-slot\" { button icon=\"rill-logo\" style=\"dock-logo\" { menu }; menu {",
                );
                if apps.is_empty() {
                    kdl.push_str(" item \"No apps installed\" target=\"/~launch/none\";");
                } else {
                    // The menu proper leads with the menu app: the flat
                    // list below is the shortcut, the grid is the place.
                    if apps.iter().any(|a| a.app_id == "launcher") {
                        kdl.push_str(
                            " item \"All apps\u{2026}\" icon=\"grid\" target=\"/~launch/launcher\"; separator;",
                        );
                    }
                    for app in &apps {
                        // App names are remote-influenced (from the installed
                        // manifest) — escape them into the KDL literal.
                        kdl.push_str(&format!(
                            " item {} target=\"/~launch/{}\";",
                            rill_doc::kdl_escape(&app.name),
                            app.key
                        ));
                    }
                }
                kdl.push_str(" } }\n");
            }
            "clock" => {
                if layout.clock == ClockStyle::Off {
                    return;
                }
                let (h, min, wday, mday, mon) = local_now();
                let text = match layout.clock {
                    ClockStyle::H12 => {
                        let hour12 = match h % 12 {
                            0 => 12,
                            other => other,
                        };
                        format!("{hour12}:{min:02} {}", if h < 12 { "AM" } else { "PM" })
                    }
                    _ => format!("{h:02}:{min:02}"),
                };
                kdl.push_str(&format!("\ttext {} style=\"clock\"\n", rill_doc::kdl_escape(&text)));
                if layout.clock_date {
                    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
                    const MONTHS: [&str; 12] = [
                        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
                        "Nov", "Dec",
                    ];
                    let date = format!(
                        "{} {} {mday}",
                        DAYS[(wday as usize).min(6)],
                        MONTHS[(mon as usize).min(11)],
                    );
                    kdl.push_str(&format!(
                        "\ttext {} style=\"clock-date\"\n",
                        rill_doc::kdl_escape(&date)
                    ));
                }
            }
            "apps" => {
                for app in &apps {
                    kdl.push_str(&format!(
                        "\tlink {} target=\"/~launch/{}\" style=\"muted\"\n",
                        rill_doc::kdl_escape(&app.name),
                        app.key
                    ));
                }
            }
            _ => {}
        };

        for name in &layout.left {
            item(name, &mut kdl);
        }
        kdl.push_str("\tspacer\n");
        for name in &layout.center {
            item(name, &mut kdl);
        }
        kdl.push_str("\tspacer\n");
        for name in &layout.right {
            item(name, &mut kdl);
        }
        kdl.push('}');
        rill_doc::compile(&kdl).map(|c| c.bytes).unwrap_or_default()
    }

    /// The minute the clock last rendered — the dock redraws when it turns,
    /// and not once between.
    pub fn clock_minute(&self) -> Option<u32> {
        let layout = DockLayout::load(&self.theme_path);
        let places_clock = layout.left.iter().chain(&layout.center).chain(&layout.right)
            .any(|i| i == "clock");
        (places_clock && layout.clock != ClockStyle::Off).then(|| {
            let (h, m, ..) = local_now();
            h * 60 + m
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dock whose theme says `background = <mode>`, plus a shape.
    fn dock_with(mode: &str) -> (PathBuf, Dock) {
        let dir = std::env::temp_dir()
            .join(format!("rill-dock-test-{}-{mode}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let theme = dir.join("theme.toml");
        std::fs::write(
            &theme,
            format!(
                "[desktop]\nglass = true\n\n[desktop.dock]\nbackground = \"{mode}\"\n\
                 height = 52\npadding = 10\ngap = 12\ncorner = 8\nicon = 34\n"
            ),
        )
        .unwrap();
        let dock = Dock::new(dir.clone(), theme, dir.clone(), None, None);
        (dir, dock)
    }

    /// The knobs reach the file and come back, clamped where they must be.
    #[test]
    fn the_shape_is_read_from_the_theme() {
        let (dir, dock) = dock_with("solid");
        let style = dock.style();
        assert_eq!(style.background, DockBackground::Solid);
        assert_eq!((style.height, style.padding, style.gap), (52.0, 10.0, 12.0));
        assert_eq!((style.corner, style.icon), (8.0, 34.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An icon taller than its strip is clamped to it — the two numbers come
    /// from the same hand-edited file, and a 26px icon in a 20px dock is one
    /// typo away (it happened on the first rice anyone made).
    #[test]
    fn an_icon_cannot_outgrow_the_strip() {
        let dir = std::env::temp_dir().join(format!("rill-dock-clamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let theme = dir.join("theme.toml");
        std::fs::write(&theme, "[desktop.dock]\nheight = 20\nicon = 26\n").unwrap();
        let style = DockStyle::load(&theme);
        assert_eq!(style.height, 20.0);
        assert_eq!(style.icon, 20.0, "the icon overflowed the reserved strip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dock nobody has configured still follows the desktop's density
    /// rather than a hard-coded look.
    #[test]
    fn an_unconfigured_dock_follows_the_density() {
        let dir = std::env::temp_dir().join(format!("rill-dock-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let theme = dir.join("theme.toml");
        std::fs::write(&theme, "[metrics]\nfont_size = 20\npadding = 9\n").unwrap();
        let style = DockStyle::load(&theme);
        assert_eq!(style.background, DockBackground::Glass);
        assert_eq!(style.padding, 9.0, "P is the padding until someone says otherwise");
        assert_eq!(style.gap, 9.0);
        assert_eq!(style.height, DEFAULT_DOCK_HEIGHT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three materials differ where it counts: what the strip paints.
    /// `none` must also declare a clear page, or the host would lay a body
    /// tint under a strip that is supposed to paint nothing.
    #[test]
    fn the_material_decides_what_the_strip_paints() {
        for (mode, opaque_strip, clear_page) in
            [("glass", true, false), ("solid", true, false), ("none", false, true)]
        {
            let (dir, dock) = dock_with(mode);
            let doc = rill_doc::decode(&dock.document()).expect("the dock document decodes");

            // The strip's own fill: `chrome` for the two that paint, a
            // transparent literal for the one that does not.
            let strip = doc
                .styles
                .iter()
                .find_map(|s| s.background)
                .expect("the strip has a background");
            match strip {
                rill_doc::ColorRef::Token(idx) => {
                    assert!(opaque_strip, "{mode}: painted {:?}", doc.string(idx));
                    assert_eq!(doc.string(idx), "chrome", "{mode}");
                }
                rill_doc::ColorRef::Literal(c) => {
                    assert!(!opaque_strip, "{mode}: painted a literal");
                    assert_eq!(c.a, 0, "{mode}: a strip that paints nothing is transparent");
                }
            }

            let declares_clear = doc.nodes.iter().any(|n| {
                matches!(
                    n,
                    rill_doc::Node::Page { color: rill_doc::ColorRef::Literal(c) } if c.a == 0
                )
            });
            assert_eq!(declares_clear, clear_page, "{mode}: page declaration");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
