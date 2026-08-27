//! The shared shell for TopBar + Sidebar apps.
//!
//! The regions and names follow Evan's component spec (2026-08-10):
//!
//! ```text
//! Window
//! ├── SidebarHeader            the bar segment over the rail
//! │   ├── IconSlot
//! │   └── SearchField
//! ├── TopToolbar               the bar segment over the content
//! │   ├── LocationBar
//! │   └── ToolbarButton(s)     (the host draws Close in its own corner)
//! ├── Sidebar
//! │   └── SidebarItem          (--active marks where you are)
//! └── ContentPane
//!     ├── SortBar → SortControl   equal-width, fill the bar
//!     ├── ListView → FileRow      FileNameCell + RowActionButton
//!     └── ContentBody             whatever remains
//! ```
//!
//! Rules the shape rests on:
//!
//! * **One inset everywhere.** [`INSET`] is the single horizontal padding
//!   every region derives from, so inner edges agree across bands by
//!   construction. A test pins [`STYLES`] to the track constants.
//! * **Every control is wired to something true.** Builders take the action
//!   they perform; a do-nothing button is unrepresentable.
//! * **State is a modifier, not a name.** `sidebar-item` stays
//!   `sidebar-item`; being current is `--active`, hover is `--hover`.
//! * **Values are escaped here; fragments are the caller's to keep safe.**
//!   Every `&str` a builder writes as a KDL *value* — a label, target, icon,
//!   bind, style, key combo — goes through [`rill_doc::kdl_escape`], because
//!   any of them may carry a name from outside (a file on disk, a title from
//!   a peer) and one unescaped quote would otherwise end the value and inject
//!   whatever follows into the page. Parameters named `action`, `inner`,
//!   `children`, or `rows` are different: they are KDL *source* the caller
//!   composed, so escaping them would destroy them. A caller interpolating an
//!   untrusted string into an action must escape it there — which for a path
//!   means building it through `rill_protocol`'s path rules, not `format!`.
//!
//! The rendered catalogue lives in `examples/vocabulary.rs` — one page with
//! every atom, re-rendered whenever [`STYLES`] moves. `scripts/trace-styles.py`
//! recolours any page so each style names itself by hue.
//!
//! Surfaces speak theme tokens (2026-08-11): panes sit on `surface`,
//! controls on `surface-raised`, hover and selection as `elevation-md/lg`,
//! sunken strips (the sort bar, input fields) on `page`.
//!
//! One material rule (2026-08-12): **every piece of window furniture is
//! `chrome`** — the toolbar, the sidebar header, the sidebar itself, and
//! the desktop's dock strip. They are the same surface at different edges
//! of the screen, so they take the same token; on a glass window that means
//! the frost runs through all of them instead of stopping where the
//! titlebar ends. Content is what sits *on* the furniture, and that is
//! `surface`.

pub mod params;
pub mod rices;

pub use rill_doc::kdl_escape;

/// The renderer's line-height factor (`rill_ui::text::LINE_HEIGHT_FACTOR`).
/// Duplicated here as data because the whole scale derives from it.
const LINE_HEIGHT_FACTOR: f32 = 1.4;

/// The two numbers the whole interface derives from, plus the macro
/// dimensions that are genuine layout policy rather than spacing.
///
/// Three nested tiers, same padding unit applied at each step:
///
/// ```text
/// content                 F → line height (renderer metrics)
///   ↓ + P                 control  = line + 2P   (button, row, field)
///   ↓ + P                 region   = control + 2P (toolbar, sort bar)
/// ```
///
/// Changing `font_size` rescales the content; changing `padding` changes
/// the density of the entire UI coherently — compact 14/6, normal 16/8,
/// spacious 18/10 — with no per-component retuning, because component
/// heights are derived, never assigned.
#[derive(Clone, Copy)]
pub struct Metrics {
    /// F: the base type size everything scales from.
    pub font_size: f32,
    /// P: the one padding unit, applied once per tier.
    pub padding: f32,
    /// Macro layout policy — explicitly sized, not derived.
    pub sidebar_width: u32,
    /// The weight monospaced surfaces ask for — the terminal, the widgets,
    /// anything that is characters on a grid.
    ///
    /// Its own knob because the bundled mono cut registers a single face at
    /// ExtraLight, so what a mono surface *looks* like is decided by how
    /// much weight the renderer synthesises rather than by the face it
    /// picks. 500 sits a step above Regular, which is where terminal text
    /// wants to be; the body type it sits beside is unaffected.
    pub mono_weight: u16,
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics { font_size: 14.0, padding: 6.0, sidebar_width: 190, mono_weight: 500 }
    }
}

impl Metrics {
    /// Read the desktop's density from the `[metrics]` table of a
    /// `theme.toml` — `font_size`, `padding`, `sidebar_width`, every field
    /// optional, anything missing or malformed keeping its default. This is
    /// what makes density a *theme* decision like color already is: the
    /// studio writes the table, every server-side page builder reads it, and
    /// the whole desktop re-densifies together. Reading the file assumes the
    /// server shares the desktop's machine — the same assumption theme
    /// *writing* already makes, honest until a theming capability exists.
    pub fn from_theme_file(path: &std::path::Path) -> Metrics {
        let mut m = Metrics::default();
        let Some(table) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok())
            .and_then(|root| root.get("metrics")?.as_table().cloned())
        else {
            return m;
        };
        let num = |key: &str| -> Option<f64> {
            table
                .get(key)
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
        };
        // The studio's stepper bounds, enforced here too: a theme file
        // cannot ask for an unreadable desktop.
        if let Some(f) = num("font_size") {
            m.font_size = (f as f32).clamp(10.0, 24.0);
        }
        if let Some(p) = num("padding") {
            m.padding = (p as f32).clamp(2.0, 16.0);
        }
        if let Some(w) = num("sidebar_width") {
            m.sidebar_width = (w as u32).clamp(120, 400);
        }
        if let Some(w) = num("mono_weight") {
            m.mono_weight = (w as u16).clamp(100, 900);
        }
        m
    }

    /// The desktop's theme path: `$XDG_CONFIG_HOME/rill/theme.toml`, else
    /// `~/.config/rill/theme.toml`.
    ///
    /// XDG first, because the compositor and the viewport already resolve it
    /// that way and this did not — so a run with XDG_CONFIG_HOME set gave the
    /// two halves of the desktop *different themes*. `bench-device.sh` sets it
    /// to a scratch directory and writes the workload's theme there, which
    /// means every benchmark so far configured the compositor from the bench
    /// and the server-side apps from the developer's own `~/.config`: a run
    /// declaring `ascii seconds=0.08` served pages asking for 0.2 if that is
    /// what the machine's real theme said. The bundles record hermetic=true.
    pub fn theme_path() -> std::path::PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
                    .join(".config")
            });
        base.join("rill/theme.toml")
    }

    /// Tier 1: the line box the renderer gives `font_size` text.
    pub fn line_height(&self) -> f32 {
        self.font_size * LINE_HEIGHT_FACTOR
    }
    /// Tier 2: content plus one padding layer — a button, a row, a field.
    pub fn control_height(&self) -> f32 {
        self.line_height() + 2.0 * self.padding
    }
    /// Tier 3: a control plus one more layer — a toolbar, a sort bar.
    pub fn region_height(&self) -> f32 {
        self.control_height() + 2.0 * self.padding
    }
    /// Square controls (icon buttons, row actions) are control-height wide.
    pub fn icon_button(&self) -> f32 {
        self.control_height()
    }
    /// Clear space above the sidebar's first item so it sits level with the
    /// first file row. Derived, not tuned: both columns spend `P` twice
    /// above their first row; the pane additionally spends one sort bar,
    /// which is one control tall. So: exactly one control height.
    pub fn sidebar_align_gap(&self) -> f32 {
        self.control_height()
    }
}

/// The style vocabulary, derived from [`Metrics`]. Surfaces speak theme
/// tokens (page/surface/surface-raised, hover and selection as elevation
/// steps), so every palette re-skins the kit. Ordinary heights are *not*
/// in this table — a control is as tall as its text plus its padding,
/// which is the whole point.
pub fn styles(m: &Metrics) -> String {
    let f = m.font_size;
    let p = m.padding;
    let rail = m.sidebar_width;
    let ib = m.icon_button();
    // Secondary type sizes, derived: data cells a step down, quiet text two,
    // section heads three. Deliberate exceptions, not parallel scales.
    let (meta, quiet, head, title) = (f - 2.0, f - 3.0, f - 4.0, f + 1.0);
    let lh = m.line_height();
    format!(
        "\
     style \"window\" height=\"fill\"\n\
     style \"bar\" padding=0 gap=0 height=\"fill\" valign=\"center\"\n\
     style \"sidebar-header\" background=\"chrome\" width={rail} padding=0 padding-x={p} height=\"fill\" gap={p} valign=\"center\"\n\
     style \"icon-slot\" color=\"text-muted\" background=\"#00000000\" size={f} corner=0 padding={p} width={ib} hover=\"icon-slot--hover\"\n\
     style \"icon-slot--hover\" color=\"accent\" background=\"#00000000\" size={f} corner=0 padding={p} width={ib}\n\
     style \"search-field\" background=\"page\" color=\"text\" size={f} corner=0\n\
     style \"location-title\" color=\"text\" size={f} underline=#false\n\
     style \"toolbar\" background=\"chrome\" padding=0 padding-x={p} height=\"fill\" gap={p} valign=\"center\"\n\
     style \"location-bar\" background=\"page\" color=\"text\" size={f} corner=0\n\
     style \"menu-slot\" width={ib} padding=0 gap=0\n\
     style \"toolbar-button\" color=\"text-muted\" background=\"#00000000\" size={f} corner=0 padding={p} width={ib} hover=\"toolbar-button--hover\"\n\
     style \"toolbar-button--hover\" color=\"accent\" background=\"#00000000\" size={f} corner=0 padding={p} width={ib}\n\
     style \"text-button\" color=\"text\" background=\"surface-raised\" size={f} corner=0 padding={p} underline=#false hover=\"text-button--hover\"\n\
     style \"text-button--hover\" color=\"text\" background=\"elevation-lg\" size={f} corner=0 padding={p} underline=#false\n\
     style \"sidebar\" background=\"chrome\" width={rail} height=\"fill\" padding={p} gap={p}\n\
     style \"sidebar-rail\" width={rail}\n\
     style \"sidebar-item\" background=\"surface\" padding={p} corner=0 valign=\"center\" hover=\"sidebar-item--hover\"\n\
     style \"sidebar-item--hover\" background=\"elevation-md\" padding={p} corner=0 valign=\"center\"\n\
     style \"sidebar-item--active\" background=\"elevation-lg\" padding={p} corner=0 valign=\"center\"\n\
     style \"sidebar-label\" color=\"text-muted\" size={f} underline=#false\n\
     style \"sidebar-label--active\" color=\"text\" size={f} underline=#false\n\
     style \"sidebar-ico\" color=\"text-muted\" size={lh}\n\
     style \"sidebar-ico--active\" color=\"accent\" size={lh}\n\
     style \"content-pane\" background=\"surface\" padding={p} gap={p} height=\"fill\"\n\
     style \"sort-bar\" background=\"page\" gap={p} padding=0 valign=\"center\"\n\
     style \"sort-control\" color=\"text-muted\" background=\"surface-raised\" size={f} width=\"fill\" padding={p} corner=0 align=\"center\" hover=\"sort-control--hover\"\n\
     style \"sort-control--hover\" color=\"text\" background=\"elevation-lg\" size={f} width=\"fill\" padding={p} corner=0 align=\"center\"\n\
     style \"sort-control--active\" color=\"accent\" background=\"elevation-lg\" size={f} width=\"fill\" padding={p} corner=0 align=\"center\"\n\
     style \"list-view\" padding=0 gap={p}\n\
     style \"file-row\" padding=0 gap={p} corner=0 valign=\"center\" hover=\"file-row--hover\"\n\
     style \"file-row--hover\" background=\"elevation-md\" padding=0 gap={p} corner=0 valign=\"center\"\n\
     style \"file-row--selected\" background=\"elevation-lg\" padding=0 gap={p} corner=0 valign=\"center\"\n\
     style \"file-cell\" padding={p} gap={p} corner=0 valign=\"center\"\n\
     style \"file-name\" color=\"text\" size={f} ellipsis=#true underline=#false\n\
     style \"file-name--dir\" color=\"text\" size={f} ellipsis=#true underline=#false\n\
     style \"row-action\" color=\"text-muted\" background=\"#00000000\" size={f} corner=0 padding={p} width={ib}\n\
     style \"cell-meta\" color=\"text-muted\" size={meta} group=\"col-meta\" align=\"right\"\n\
     style \"cell-kind\" color=\"text-muted\" size={meta} group=\"col-kind\"\n\
     style \"panel\" background=\"elevation-md\" corner=0 padding={p} gap={p}\n\
     style \"prop-row\" padding=0 gap={p} valign=\"center\"\n\
     style \"prop-title\" color=\"text\" size={f}\n\
     style \"prop-label\" color=\"text-muted\" size={meta} group=\"prop-label\"\n\
     style \"prop-value\" color=\"text\" size={meta}\n\
     style \"field\" background=\"page\" color=\"text\" corner=0 border=1 border-color=\"border\"\n\
     style \"cta\" background=\"accent\" color=\"accent-text\" corner=0 padding={p}\n\
     style \"danger\" color=\"#ff9aa6\" size={f} corner=0 padding={p} border=1 border-color=\"#7a2431\" hover=\"danger--hover\"\n\
     style \"danger--hover\" background=\"#5a1a24\" color=\"#ffd7dc\" size={f} corner=0 padding={p} border=1 border-color=\"#a63a4a\"\n\
     style \"title\" size={title} color=\"text\"\n\
     style \"hd\" color=\"text-muted\" size={head}\n\
     style \"muted\" color=\"text-muted\" size={quiet} underline=#false\n\
     style \"mono\" font=\"mono\" size={f} color=\"text\"\n\
     style \"rule\" background=\"border\"\n\
     style \"card\" background=\"elevation-md\" corner=0\n\
     style \"ico\" color=\"accent\" size={lh}\n\
     style \"ico-dim\" color=\"text-muted\" size={lh}\n"
    )
}

/// One sidebar entry. `current` fills the row: the pane shows the contents,
/// the sidebar shows the position. `icon` names a glyph from the icon set
/// (Phosphor fill weights read best at sidebar size — `home-fill`,
/// `folder-fill`, `star-fill`…).
pub struct Place {
    pub label: String,
    pub target: String,
    pub icon: String,
    pub current: bool,
}

/// Everything the shell needs from the app. Strings are KDL fragments built
/// with the helpers below — the shell owns the frame, the app its verbs.
pub struct Shell<'a> {
    /// F and P, plus the macro dimensions.
    pub metrics: Metrics,
    /// `state` declarations, one per line.
    pub states: &'a str,
    /// The window strip: a [`sidebar_header`] plus a [`toolbar`]. Empty =
    /// no titlebar claim.
    pub titlebar: &'a str,
    pub places: &'a [Place],
    /// Custom rail content in place of the `places` rows — a file tree, a
    /// channel list, anything richer than flat places. The rail's scroll
    /// region, width, and footer still belong to the kit; only the rows
    /// between are the app's. `Some("")` still claims a rail.
    pub rail_body: Option<&'a str>,
    /// A quiet link at the sidebar's foot (label, target).
    pub footer: Option<(&'a str, &'a str)>,
    /// Vertical gap above the first sidebar item — [`SIDEBAR_ALIGN_GAP`] to
    /// sit level with the first file row, 0 for none.
    pub sidebar_top_gap: u32,
    /// App-specific styles appended after [`STYLES`].
    pub extra_styles: &'a str,
    /// The style the content pane wears. `None` is the kit's own
    /// `content-pane` — a `surface` panel with the standard padding. Name
    /// your own when the body *is* the surface: a terminal grid painting an
    /// opaque panel behind the window's glass reads as a black rectangle
    /// pasted over the desktop, which is exactly what it is.
    pub content_style: Option<&'a str>,
    /// The content pane's children.
    pub body: &'a str,
    /// Whether the content pane rides its own scroll region (the pinned-rail
    /// convention). True for every listing-beside-a-rail app. The terminal
    /// says false: its document *is* the transcript, page scroll is its
    /// scrollback, and follow-the-end lives on the page scroll — a region
    /// would pin the oldest lines instead of the newest.
    pub scroll_content: bool,
}

/// Assemble the full document.
pub fn shell(s: &Shell) -> String {
    let mut kdl = styles(&s.metrics);
    kdl.push_str(s.extra_styles);
    kdl.push('\n');
    kdl.push_str(s.states);
    kdl.push('\n');
    kdl.push_str("column gap=0 padding=0 style=\"window\" {\n");
    if !s.titlebar.is_empty() {
        kdl.push_str("\ttitlebar {\n\t\trow style=\"bar\" {\n");
        kdl.push_str(s.titlebar);
        kdl.push_str("\t\t}\n\t}\n");
    }
    kdl.push_str("\trow gap=0 padding=0 style=\"window\" {\n");
    // No places and no footer means no sidebar. An empty rail still painted
    // its chrome-coloured strip down the side of every app that had nothing
    // to put in it — a terminal, a viewer, anything with one place.
    let rail = !s.places.is_empty() || s.footer.is_some() || s.rail_body.is_some();
    if rail {
    // The rail is its own scroll region too: more places than the window
    // has height scrolls the rail under the pointer, and the content pane
    // beside it stands as still as the rail does when the content scrolls.
    // The scroll node carries the rail's width so the row's slots come out
    // the same as before; the sidebar column inside keeps its styling.
    kdl.push_str("\t\tscroll style=\"sidebar-rail\" {\n");
    kdl.push_str("\t\tcolumn style=\"sidebar\" {\n");
    if s.sidebar_top_gap > 0 {
        kdl.push_str(&format!("\t\t\tspacer size={}\n", s.sidebar_top_gap));
    }
    if let Some(body) = s.rail_body {
        kdl.push_str(body);
    }
    for p in s.places {
        kdl.push_str(&format!(
            "\t\t\trow style=\"{row}\" target={target} {{ icon {icon} style=\"{ico}\"; \
             text {label} style=\"{text}\" }}\n",
            icon = kdl_escape(&p.icon),
            label = kdl_escape(&p.label),
            target = kdl_escape(&p.target),
            row = if p.current { "sidebar-item--active" } else { "sidebar-item" },
            ico = if p.current { "sidebar-ico--active" } else { "sidebar-ico" },
            text = if p.current { "sidebar-label--active" } else { "sidebar-label" },
        ));
    }
    if let Some((label, target)) = s.footer {
        kdl.push_str(&format!(
            "\t\t\tspacer\n\t\t\trow style=\"sidebar-item\" {{ \
             link {} target={} style=\"muted\" }}\n",
            kdl_escape(label),
            kdl_escape(target),
        ));
    }
    kdl.push_str("\t\t}\n\t\t}\n");
    }
    // The content pane is its own scroll region, so a long listing scrolls
    // under a rail that stands still — the wheel over the pane moves the
    // pane, and the sidebar keeps its place the way every desktop's does.
    // (Before regions could scroll independently, the whole document moved
    // and took the rail with it.)
    if s.scroll_content {
        kdl.push_str("\t\tscroll {\n");
    }
    kdl.push_str(&format!(
        "\t\tcolumn style=\"{}\" {{\n",
        s.content_style.unwrap_or("content-pane")
    ));
    kdl.push_str(s.body);
    if s.scroll_content {
        kdl.push_str("\t\t}\n");
    }
    kdl.push_str("\t\t}\n\t}\n}");
    kdl
}

// ---- the window strip --------------------------------------------------

/// The bar segment over the rail: an [`icon_slot`], then usually a
/// [`search_field`].
pub fn sidebar_header(children: &str) -> String {
    format!("\t\t\trow style=\"sidebar-header\" {{\n{children}\t\t\t}}\n")
}

/// The bar segment over the content: a [`location_bar`], then the toolbar's
/// controls. The host draws Close in its own corner beyond this.
pub fn toolbar(children: &str) -> String {
    format!("\t\t\trow style=\"toolbar\" {{\n{children}\t\t\t}}\n")
}

/// The utility slot at the head of the sidebar header — a real icon.
pub fn icon_slot(icon: &str, action: &str) -> String {
    format!("\t\t\t\tbutton icon={} style=\"icon-slot\" {{ {action} }}\n", kdl_escape(icon))
}

/// The bar's title text — the app or current place name ("Files", "Root").
pub fn location_title(name: &str) -> String {
    format!("\t\t\t\ttext {} style=\"location-title\"\n", kdl_escape(name))
}

/// The sidebar search field; Enter fires `action`. Flexes across the rail.
pub fn search_field(bind: &str, placeholder: &str, action: &str) -> String {
    format!(
        "\t\t\t\ttext_input bind={} style=\"search-field\" placeholder={} {{ {action} }}\n",
        kdl_escape(bind),
        kdl_escape(placeholder),
    )
}

/// The current address, editable; Enter fires `action`. Flexes across the
/// toolbar.
pub fn location_bar(bind: &str, action: &str) -> String {
    format!(
        "\t\t\t\ttext_input bind={} style=\"location-bar\" placeholder=\"path\u{2026}\" {{ {action} }}\n",
        kdl_escape(bind),
    )
}

/// A square toolbar control carrying a named icon. `action` is the KDL
/// action child — the button *is* its action. Bar icons carry no plate:
/// the glyph alone, muted at rest and accent on hover — only text verbs
/// get raised backgrounds.
pub fn toolbar_button(icon: &str, action: &str) -> String {
    format!("\t\t\t\tbutton icon={} style=\"toolbar-button\" {{ {action} }}\n", kdl_escape(icon))
}

/// A toolbar control that opens a menu of choices — the "choose one"
/// flavor of the standard presenter (a combobox without inventing one).
/// The button shows the current choice's glyph; the entries submit the
/// alternatives. A bare wrapper row carries the menu, sized by its button,
/// so the menu's hit region is exactly the control.
pub fn menu_button(icon: &str, entries: &[MenuEntry]) -> String {
    format!(
        "\t\t\t\trow style=\"menu-slot\" {{ button icon={} style=\"toolbar-button\" {{ menu }};{menu} }}\n",
        kdl_escape(icon),
        menu = menu(entries),
    )
}

/// The window's close control — a toolbar member like any other, aligned
/// to the same edge as every trailing control. `/~close` is resolved by
/// the host, never sent to the server.
pub fn close_button() -> String {
    toolbar_button("close", "navigate \"/~close\"")
}

/// A text verb on the toolbar or in a panel.
pub fn text_button(label: &str, action: &str) -> String {
    format!("\t\t\t\tbutton {} style=\"text-button\" {{ {action} }}\n", kdl_escape(label))
}

/// The verb that deletes something. Distinct on purpose: destructive actions
/// should never look like their neighbours.
pub fn danger_button(label: &str, action: &str) -> String {
    format!("\t\t\t\tbutton {} style=\"danger\" {{ {action} }}\n", kdl_escape(label))
}

/// The one emphasized verb on a form.
pub fn cta_button(label: &str, action: &str) -> String {
    format!("\t\t\t\tbutton {} style=\"cta\" {{ {action} }}\n", kdl_escape(label))
}

// ---- the content pane --------------------------------------------------

/// The sort bar: equal-width controls filling the strip.
pub fn sort_bar(controls: &str) -> String {
    format!("\t\t\trow style=\"sort-bar\" {{\n{controls}\t\t\t}}\n")
}

/// One sort control. The active one carries a real caret glyph for its
/// direction.
pub fn sort_control(label: &str, active: bool, descending: bool, action: &str) -> String {
    let icon = match (active, descending) {
        (false, _) => String::new(),
        (true, false) => " icon=\"chevron-up\"".to_string(),
        (true, true) => " icon=\"chevron-down\"".to_string(),
    };
    format!(
        "\t\t\t\tbutton {}{icon} style=\"{}\" {{ {action} }}\n",
        kdl_escape(label),
        if active { "sort-control--active" } else { "sort-control" },
    )
}

/// The list view around [`file_row`]s.
pub fn list_view(rows: &str) -> String {
    format!("\t\t\tcolumn style=\"list-view\" {{\n{rows}\t\t\t}}\n")
}

/// One row of a list view: a name cell that goes somewhere, optional data
/// cells inside it, and an optional trailing 28px action.
pub struct FileRow<'a> {
    pub selected: bool,
    /// Leading glyph inside the name cell: (icon name, style).
    pub icon: (&'a str, &'a str),
    pub title: &'a str,
    pub target: &'a str,
    /// `"file-name"` or `"file-name--dir"`.
    pub title_style: &'a str,
    /// Right-hand columns, leftmost first: (text, style) — e.g. cell-kind,
    /// cell-meta.
    pub cells: &'a [(String, &'a str)],
    /// A trailing 28px control: (glyph, action). `"menu"` as the action
    /// opens the row's context menu — the standard pip.
    pub trailing: Option<(&'a str, String)>,
    /// The row's context menu (a ready `menu {...}` block from [`menu`]).
    /// Right-click and the pip both open it, host-presented.
    pub menu: Option<String>,
}

/// One entry for [`menu`]: a labelled verb, or a separator between groups.
pub enum MenuEntry<'a> {
    Item {
        label: &'a str,
        /// Optional glyph name; keep icons for the distinctive verbs.
        icon: Option<&'a str>,
        /// Destructive: styled apart, never looks like its neighbours.
        danger: bool,
        /// `target="/x"` navigation, or a KDL action child ("submit …").
        wire: MenuWire<'a>,
    },
    Separator,
}

/// How a menu item acts: the keyboard/link/button trichotomy, menu-shaped.
pub enum MenuWire<'a> {
    Target(&'a str),
    Action(&'a str),
}

/// The string value of a submitted field, if it was sent and is a string.
///
/// Every app that takes a `submit` needs this and several wrote it out
/// identically. A missing field and a field of the wrong type are the same
/// answer on purpose: both mean "the page did not give me this", and an app
/// that wants to tell them apart is asking a question the action grammar
/// does not answer.
pub fn field<'a>(
    fields: &'a [(String, rill_protocol::ActionValue)],
    name: &str,
) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == name).and_then(|(_, v)| match v {
        rill_protocol::ActionValue::Str(s) => Some(s.as_str()),
        _ => None,
    })
}

/// Compile a generated page, with the design-loop hooks applied.
///
/// Every app ends its page function with the same three steps, and for most of
/// them two were missing — the hooks lived in files-app only, so `RILL_TRACE`
/// against the terminal, the meter, the music player or the studio silently
/// did nothing, which reads as "tracing is broken" rather than "that app
/// doesn't have it".
///
/// * `RILL_TRACE=<path>` — serve every page trace-coloured, each style a
///   unique hue, layout untouched, and write the colour→name legend to
///   `<path>` for a host's inspector. The live UI becomes its own style
///   reference. `RILL_TRACE_MODE=tiers` colours by spacing tier (regions red,
///   controls cyan, content white) instead of per-style hues.
/// * `RILL_DUMP_KDL=<path>` — write the generated source before compiling.
///   The design loop needs the KDL a page was born from, and the wire only
///   ever carries the compiled form.
///
/// `app` names the app in the error line, so a page that fails to compile
/// says which server produced it.
pub fn compile_page(app: &str, kdl: &str) -> Result<Vec<u8>, rill_protocol::Status> {
    let traced;
    let kdl = match std::env::var_os("RILL_TRACE") {
        Some(path) => {
            let mode = match std::env::var("RILL_TRACE_MODE").as_deref() {
                Ok("tiers") => trace::Mode::Tiers,
                _ => trace::Mode::Styles,
            };
            let (page, legend) = trace::apply_with(kdl, mode);
            let _ = std::fs::write(path, trace::legend_lines(&legend));
            traced = page;
            traced.as_str()
        }
        None => kdl,
    };
    if let Some(path) = std::env::var_os("RILL_DUMP_KDL") {
        let _ = std::fs::write(path, kdl);
    }
    rill_doc::compile(kdl).map(|c| c.bytes).map_err(|e| {
        eprintln!("{app}: page generation failed: {e}");
        rill_protocol::Status::Internal
    })
}

// ---- actions -----------------------------------------------------------
//
// The builders above take actions as ready-made KDL source, which leaves the
// endpoint — the one part routinely built by pasting a name into a path — as
// the last unescaped value in a generated page. These two write it properly,
// so a caller never has to reach for `format!` to wire a verb to a subject.

/// A `submit` action. `fields` is the KDL body (`field "x" from="state"`, …);
/// pass `""` for a submit that carries nothing.
///
/// The endpoint is escaped because it is nearly always a path with something
/// external appended — a file name, a record id. Path syntax permits quotes,
/// so an unescaped endpoint ends its own string literal and whatever follows
/// the quote becomes page structure.
pub fn submit(endpoint: &str, fields: &str) -> String {
    match fields.trim() {
        "" => format!("submit {}", kdl_escape(endpoint)),
        f => format!("submit {} {{ {f} }}", kdl_escape(endpoint)),
    }
}

/// A `navigate` action — the same escaping argument as [`submit`].
pub fn navigate(target: &str) -> String {
    format!("navigate {}", kdl_escape(target))
}

/// Build a `menu {...}` block for a container. The host presents it, so it
/// costs the page nothing visually — it is affordance data, like `key`.
pub fn menu(entries: &[MenuEntry]) -> String {
    let mut out = String::from(" menu {");
    for entry in entries {
        match entry {
            MenuEntry::Separator => out.push_str(" separator;"),
            MenuEntry::Item { label, icon, danger, wire } => {
                out.push_str(&format!(" item {}", kdl_escape(label)));
                if let Some(icon) = icon {
                    out.push_str(&format!(" icon={}", kdl_escape(icon)));
                }
                if *danger {
                    out.push_str(" danger=#true");
                }
                match wire {
                    MenuWire::Target(t) => out.push_str(&format!(" target={};", kdl_escape(t))),
                    MenuWire::Action(a) => out.push_str(&format!(" {{ {a} }};")),
                }
            }
        }
    }
    out.push_str(" }");
    out
}

pub fn file_row(r: &FileRow) -> String {
    let mut row = format!(
        "\t\t\t\trow style=\"{style}\" target={target} {{ \
         row style=\"file-cell\" {{ icon {icon} style={tint}; \
         text {title} style={ts}; spacer",
        style = if r.selected { "file-row--selected" } else { "file-row" },
        icon = kdl_escape(r.icon.0),
        tint = kdl_escape(r.icon.1),
        title = kdl_escape(r.title),
        target = kdl_escape(r.target),
        ts = kdl_escape(r.title_style),
    );
    for (text, style) in r.cells {
        row.push_str(&format!("; text {} style={}", kdl_escape(text), kdl_escape(style)));
    }
    row.push_str(" }");
    if let Some((icon, action)) = &r.trailing {
        row.push_str(&format!("; button icon={} style=\"row-action\" {{ {action} }}", kdl_escape(icon)));
    }
    if let Some(menu) = &r.menu {
        row.push(';');
        row.push_str(menu);
    }
    row.push_str(" }\n");
    row
}

/// A single-line input bound to a state slot; Enter fires `action`.
pub fn input(bind: &str, style: &str, placeholder: &str, action: &str) -> String {
    format!(
        "\t\t\t\ttext_input bind={} style={} placeholder={} {{ {action} }}\n",
        kdl_escape(bind),
        kdl_escape(style),
        kdl_escape(placeholder),
    )
}

/// Show `inner` only while a bool state is on.
pub fn when(state: &str, inner: &str) -> String {
    format!("\t\t\twhen {} {{\n{inner}\t\t\t}}\n", kdl_escape(state))
}

/// Show `inner` only while a bool state is off — the other half of a slot
/// that swaps between two occupants (a sort bar and the form that borrows
/// its space).
pub fn unless(state: &str, inner: &str) -> String {
    format!("\t\t\tunless {} {{\n{inner}\t\t\t}}\n", kdl_escape(state))
}

/// A page-declared key binding that performs an action — `combo` is
/// "down", "delete", "ctrl+shift+n"… Emit these for whatever the page's
/// buttons already do: a shortcut is the same affordance without the pixels.
pub fn key_action(combo: &str, action: &str) -> String {
    format!("\t\t\tkey {} {{ {action} }}\n", kdl_escape(combo))
}

/// A key binding that navigates, the keyboard twin of a link.
pub fn key_link(combo: &str, target: &str) -> String {
    format!("\t\t\tkey {} target={}\n", kdl_escape(combo), kdl_escape(target))
}

/// A raised transient panel row — where a mkdir/rename form lives.
pub fn panel_row(inner: &str) -> String {
    format!("\t\t\trow style=\"panel\" {{\n{inner}\t\t\t}}\n")
}

/// One label→value line of a detail panel. Labels share a measure group,
/// so every value starts on the same column — a table track, by content.
pub fn property_row(label: &str, value: &str) -> String {
    format!(
        "\t\t\t\trow style=\"prop-row\" {{ text {} style=\"prop-label\"; text {} style=\"prop-value\" }}\n",
        kdl_escape(label),
        kdl_escape(value),
    )
}

/// A quiet line for an empty pane. Say *why* it is empty.
pub fn empty_note(message: &str) -> String {
    format!("\t\t\ttext {} style=\"muted\"\n", kdl_escape(message))
}

/// The horizontal rule under a header.
pub fn rule() -> String {
    "\t\t\trect height=1 style=\"rule\"\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shell's whole promise: what it emits is a valid document, with
    /// the titlebar claimed and the active place marked.
    #[test]
    fn the_shell_compiles_and_claims_the_bar() {
        let places = [
            Place {
                label: "Home".into(),
                target: "/x".into(),
                icon: "home-fill".into(),
                current: false,
            },
            Place {
                label: "work".into(),
                target: "/x/work".into(),
                icon: "folder-fill".into(),
                current: true,
            },
        ];
        let strip = sidebar_header(
            &(icon_slot("home", "navigate \"/x\"")
                + &search_field("q", "Search\u{2026}", "toggle \"sr\"")),
        ) + &toolbar(
            &(location_bar("loc", "toggle \"sr\"") + &toolbar_button("list", "toggle \"sr\"")),
        );
        let body = sort_bar(
            &(sort_control("Name", true, false, "toggle \"sr\"")
                + &sort_control("Size", false, false, "toggle \"sr\"")),
        ) + &list_view(&file_row(&FileRow {
            selected: true,
            icon: ("folder", "ico"),
            title: "docs",
            target: "/x/docs",
            title_style: "file-name--dir",
            cells: &[],
            trailing: Some(("dots-vertical", "toggle \"sr\"".into())),
            menu: None,
        }));
        let kdl = shell(&Shell {
            metrics: Metrics::default(),
            states: "state \"sr\" initial=#false\nstate \"q\" initial=\"\"\n\
                     state \"loc\" initial=\"/\"\n",
            titlebar: &strip,
            places: &places,
            footer: Some(("About", "/x/about")),
            sidebar_top_gap: Metrics::default().sidebar_align_gap() as u32,
            extra_styles: "",
            content_style: None,
            body: &body,
            rail_body: None,
            scroll_content: true,
        });
        let compiled = rill_doc::compile(&kdl).expect("shell output compiles");
        let doc = rill_doc::decode(&compiled.bytes).expect("and decodes");
        assert!(doc.nodes.iter().any(|n| matches!(n, rill_doc::Node::Chrome { .. })));
        let active = doc.styles.iter().any(|s| doc.string(s.name_idx) == "sidebar-item--active");
        assert!(active, "the active place's style is in the table");
    }

    /// The table really derives from the metrics: change P and every
    /// control's padding moves; the derived heights follow the formula.
    #[test]
    fn the_table_derives_from_the_metrics() {
        let m = Metrics { font_size: 16.0, padding: 8.0, sidebar_width: 240, ..Metrics::default() };
        let t = styles(&m);
        assert!(t.contains("width=240"), "sidebar width is the macro value");
        assert!(t.contains("padding=8"), "P reaches control padding");
        assert!(t.contains("padding-x=8"), "and the region inset");
        let ib = format!("width={}", m.icon_button());
        assert!(t.contains(&ib), "icon buttons are control-height wide: {ib}");
        assert_eq!(m.region_height(), m.line_height() + 4.0 * m.padding, "F + 4P");
        assert_eq!(
            m.sidebar_align_gap(),
            m.control_height(),
            "the align gap is exactly one control: both columns spend P twice, \
             the pane's extra spend is the one-control sort bar"
        );
    }

    /// Only the active control carries a caret, and it points the right way.
    #[test]
    fn sort_controls_carry_the_caret_honestly() {
        assert!(!sort_control("Name", false, true, "toggle \"x\"").contains("icon="));
        assert!(sort_control("Name", true, false, "toggle \"x\"").contains("icon=\"chevron-up\""));
        assert!(sort_control("Name", true, true, "toggle \"x\"").contains("icon=\"chevron-down\""));
    }
}

/// Live style tracing: recolour a page so every style names itself by hue.
///
/// Containers get a distinct mid-bright background, text-only styles the
/// bright form of their hue; layout is untouched, so the traced page's
/// spacing is the real page's. The legend maps colour back to style name —
/// a host with the legend can tell you what's under the cursor.
pub mod trace {
    /// One legend entry: style name, `#rrggbb`, and whether the hue landed
    /// on the background or the text.
    pub struct Entry {
        pub name: String,
        pub color: String,
        pub kind: &'static str,
    }

    fn hsv(h: f32, s: f32, v: f32) -> String {
        let i = (h * 6.0).floor();
        let f = h * 6.0 - i;
        let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
        let (r, g, b) = match (i as i32) % 6 {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            _ => (v, p, q),
        };
        format!("#{:02x}{:02x}{:02x}", (r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
    }

    /// How a trace colours the page.
    #[derive(Clone, Copy, PartialEq)]
    pub enum Mode {
        /// Every style its own hue — "which style is this box?"
        Styles,
        /// The spacing model's tiers, matching the reference diagram:
        /// **regions red, controls cyan, content white**, hues varied
        /// slightly within each family so components stay distinct (and
        /// colours stay unique, so the hover inspector keeps working).
        Tiers,
    }

    /// The region tier: the boxes that group controls. Everything else
    /// boxy is a control; everything textual is content.
    const REGIONS: &[&str] = &[
        "sidebar-header", "toolbar", "sidebar", "content-pane", "sort-bar",
        "list-view", "panel", "card", "grid",
    ];
    /// Spacing-neutral wrappers (§14): geometrically transparent levels
    /// that are not one of the three tiers. Painted structural grey.
    const WRAPPERS: &[&str] = &["window", "bar", "rule"];

    fn tier_color(name: &str, boxy: bool, family_idx: &mut [usize; 4]) -> (String, &'static str) {
        let base = name.split("--").next().unwrap_or(name);
        if WRAPPERS.contains(&base) {
            let i = family_idx[3];
            family_idx[3] += 1;
            (format!("#{0:02x}{0:02x}{0:02x}", 0x4a + i * 8), "wrapper")
        } else if !boxy {
            // Content: whites, faintly tinted apart.
            let i = family_idx[2];
            family_idx[2] += 1;
            (hsv((i as f32 * 0.13).fract(), 0.06, 1.0), "content")
        } else if REGIONS.contains(&base) {
            let i = family_idx[0];
            family_idx[0] += 1;
            // Reds: hue walks a short arc so regions differ but stay red.
            (hsv(((i as f32 * 26.0) % 50.0 - 15.0).rem_euclid(360.0) / 360.0, 0.78, 0.72), "region")
        } else {
            let i = family_idx[1];
            family_idx[1] += 1;
            // Cyans, same idea.
            (hsv((168.0 + (i as f32 * 9.0) % 46.0) / 360.0, 0.70, 0.85), "control")
        }
    }

    /// Recolour `kdl`, returning the traced page and its legend.
    pub fn apply(kdl: &str) -> (String, Vec<Entry>) {
        apply_with(kdl, Mode::Styles)
    }

    /// [`apply`], with a colouring mode.
    pub fn apply_with(kdl: &str, mode: Mode) -> (String, Vec<Entry>) {
        let mut legend = Vec::new();
        let mut out = Vec::new();
        let mut families = [0usize; 4];
        for line in kdl.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("style \"") else {
                out.push(line.to_string());
                continue;
            };
            let Some(q) = rest.find('"') else {
                out.push(line.to_string());
                continue;
            };
            let name = &rest[..q];
            let props: Vec<String> = rest[q + 1..].split_whitespace().map(String::from).collect();
            // Golden-angle hues: adjacent indices never look alike.
            let hue = (legend.len() as f32 * 137.508).rem_euclid(360.0) / 360.0;
            let boxy = props.iter().any(|p| {
                p.starts_with("background=")
                    || p.starts_with("padding")
                    || p.starts_with("gap=")
                    || p.starts_with("width=")
                    || p.starts_with("height=")
                    || p.starts_with("corner=")
            });
            let (color, kind) = match mode {
                Mode::Styles => {
                    if boxy { (hsv(hue, 0.62, 0.55), "background") } else { (hsv(hue, 0.85, 1.0), "text") }
                }
                Mode::Tiers => tier_color(name, boxy, &mut families),
            };
            let mut props = props;
            let set = |props: &mut Vec<String>, key: &str, value: &str| {
                let replacement = format!("{key}\"{value}\"");
                match props.iter_mut().find(|p| p.starts_with(key)) {
                    Some(p) => *p = replacement,
                    None => props.insert(0, replacement),
                }
            };
            match (mode, boxy) {
                (Mode::Styles, true) => set(&mut props, "background=", &color),
                (Mode::Styles, false) => set(&mut props, "color=", &color),
                (Mode::Tiers, true) => {
                    // Every glyph is black; token colours would leak through
                    // and fight the family hues.
                    set(&mut props, "background=", &color);
                    set(&mut props, "color=", "#000000");
                }
                (Mode::Tiers, false) => {
                    // Tier-1 content: black glyphs in their white bounds —
                    // the box *is* the point, it is the F the scale derives
                    // from. Icons get the same treatment now that they
                    // honour a background.
                    set(&mut props, "background=", &color);
                    set(&mut props, "color=", "#000000");
                }
            }
            let indent = &line[..line.len() - trimmed.len()];
            out.push(format!("{indent}style \"{name}\" {}", props.join(" ")));
            legend.push(Entry { name: name.to_string(), color, kind });
        }
        (out.join("\n"), legend)
    }

    /// The legend as the line format hosts parse: `#rrggbb name` per line.
    pub fn legend_lines(legend: &[Entry]) -> String {
        legend.iter().map(|e| format!("{} {}\n", e.color, e.name)).collect()
    }

    #[cfg(test)]
    mod tests {
        /// Traced output must stay a valid page with every style uniquely
        /// coloured — otherwise the inspector's colour→name map is a lie.
        #[test]
        fn traced_pages_compile_and_colours_are_unique() {
            let (kdl, legend) = super::apply(&crate::styles(&crate::Metrics::default()));
            let page = format!("{kdl}\ncolumn style=\"window\" {{ text \"x\" }}");
            rill_doc::compile(&page).expect("traced styles compile");
            let mut seen = std::collections::HashSet::new();
            for e in &legend {
                assert!(seen.insert(e.color.clone()), "duplicate hue {}", e.color);
            }
            assert!(legend.len() > 30, "the whole table is traced");
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    /// Every surface in the kit speaks a theme token — the sizing-phase grey
    /// ladder is gone, so light palettes work. The one allowed literal
    /// family is danger's reds: destruction deliberately does not re-theme.
    #[test]
    fn surfaces_speak_tokens_not_literals() {
        let table = styles(&Metrics::default());
        for line in table.lines() {
            // Danger's reds are deliberate; fully-transparent is "paint
            // nothing", not a color a theme could own.
            if line.contains("\"danger") || line.contains("background=\"#00000000\"") {
                continue;
            }
            assert!(
                !line.contains("background=\"#"),
                "literal background survived the token pass: {line}"
            );
        }
    }
}

#[cfg(test)]
mod theme_path_tests {
    use super::Metrics;

    /// XDG_CONFIG_HOME wins, because the compositor and viewport resolve it
    /// that way and a desktop whose two halves read different theme files is
    /// not one desktop. `bench-device.sh` relies on this to make a run
    /// hermetic; before the fix it only half was.
    #[test]
    fn xdg_config_home_wins_over_home() {
        // The environment is process-global, so this test is deliberately the
        // only one here that touches it.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/rill-xdg-probe");
            std::env::set_var("HOME", "/home/nobody");
        }
        assert_eq!(
            Metrics::theme_path(),
            std::path::PathBuf::from("/tmp/rill-xdg-probe/rill/theme.toml")
        );

        // Unset (or empty) falls back to ~/.config, the historical location.
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(
            Metrics::theme_path(),
            std::path::PathBuf::from("/home/nobody/.config/rill/theme.toml")
        );
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "") };
        assert_eq!(
            Metrics::theme_path(),
            std::path::PathBuf::from("/home/nobody/.config/rill/theme.toml"),
            "an empty XDG_CONFIG_HOME is not a path"
        );
    }
}
