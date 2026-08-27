//! A file explorer for the *served namespace* — the paths a client can fetch
//! over `rill://`, not the server's local disk.
//!
//! ```bash
//! files-app <content-root> --identity <server-id> [--bind ADDR] [--port N]
//! ```
//!
//! Navigation is the app model's native gesture: a directory is a document of
//! links, and clicking one fetches the next document. No client code, no
//! scripting, no new platform capability — which is why this is the first app
//! worth building.
//!
//! **The policy is the UI.** The server authorizes a handler by its own prefix
//! (`/files/**`), *not* by the paths that handler chooses to read. A browser
//! that listed everything under the content root would therefore be a policy
//! bypass: a device allowed `/files/**` could enumerate `/private/**` through
//! it, and even read files back, without ever holding a grant for them. So
//! every entry is checked against the same `policy.toml` the server enforces,
//! at the path the client would use to fetch it. A device that cannot GET
//! `/private/x` cannot see it listed here either — the denial is hidden, the
//! same way the server hides it.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rill_auth::{Identity, Policy};
use rill_appkit::{Place, Shell, shell};
use rill_doc::kdl_escape;
use rill_protocol::{ActionValue, Status};
use rill_server::{AppHandler, Server, ServerConfig};

/// How deep to look for something readable when deciding whether a directory
/// is worth showing, and how many entries to try per level.
const PROBE_DEPTH: u32 = 6;

/// Request-serving threads, and with them the ceiling on malloc arenas.
///
/// Four is plenty for a machine's own desktop: requests are short and mostly
/// waiting on a file or a pty, and the work that is not — compiling a page —
/// is milliseconds. Choosing this number is choosing the footprint, which on
/// the hardware this has to run on is the number that matters.
const WORKERS: usize = 4;

/// How often freed memory is offered back to the kernel. Cheap enough to be
/// frequent — it walks each arena's free list — and the point is that a
/// desktop which was busy a minute ago should not still be paying for it.
const TRIM_EVERY: std::time::Duration = std::time::Duration::from_secs(3);
const PROBE_BREADTH: usize = 200;

struct Files {
    root: PathBuf,
    policy: Policy,
    /// Where writing is permitted, canonicalised. `None` means the explorer
    /// is read-only.
    ///
    /// The policy answers reads: `authorize(identity, path)` says whether a
    /// device may *fetch* something. It has no concept of writing, so a
    /// delete verb gated on it would let any device holding `/files/**`
    /// remove anything it can see — including the app packs this desktop
    /// runs on. Rather than invent write semantics inside an example app,
    /// writes are confined to one subtree named at startup. When that
    /// confinement starts to chafe, that is the signal the policy needs
    /// write rules of its own.
    writable: Option<PathBuf>,
    /// Per-device view state: what is selected, and which view is showing.
    ///
    /// Selection lives here rather than in document state because `when`
    /// tests booleans only — there is no way for a document to say "style
    /// this row if the selection equals this path". Round-tripping it through
    /// the server needs no new platform feature, and the server is local, so
    /// the question is whether it *feels* instant rather than whether it is.
    ui: Mutex<HashMap<String, Ui>>,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Type,
    Size,
    Modified,
}

#[derive(Clone)]
struct Ui {
    /// Served path of the selected entry, if any. Cleared by navigating.
    selected: Option<String>,
    list: bool,
    /// Sort key and direction for listings.
    sort: SortKey,
    sort_desc: bool,
    /// Case-insensitive substring the listing is filtered by.
    filter: Option<String>,
    /// Served paths this device has starred. View state like the selection:
    /// stars are a device's bookmarks into the tree, not facts about files.
    starred: HashSet<String>,
}

impl Default for Ui {
    fn default() -> Ui {
        // The reference names ListView the default view.
        Ui {
            selected: None,
            list: true,
            sort: SortKey::Name,
            sort_desc: false,
            filter: None,
            starred: HashSet::new(),
        }
    }
}

/// Devices are the unit of view state: two devices browsing the same server
/// should not fight over each other's selection.
fn ui_key(identity: &Identity) -> String {
    match identity {
        Identity::Device(name) => name.clone(),
        Identity::Anonymous => String::new(),
    }
}

/// One listed entry, already known to be visible to the asking device.
struct Entry {
    name: String,
    /// The path a client would GET — also what the policy is checked against.
    served: String,
    is_dir: bool,
    size: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
}

impl Files {
    fn new(root: PathBuf, policy: Policy, writable: Option<PathBuf>) -> Files {
        let writable = writable.and_then(|w| std::fs::canonicalize(w).ok());
        // The trash lives inside the writable subtree, so everything the
        // write rules promise covers it too. Created eagerly so the sidebar's
        // Trash place always resolves.
        if let Some(w) = &writable {
            let _ = std::fs::create_dir_all(w.join(".trash"));
        }
        Files { root, policy, writable, ui: Mutex::new(HashMap::new()) }
    }

    /// The trash directory, when writing is on.
    fn trash_root(&self) -> Option<PathBuf> {
        self.writable.as_ref().map(|w| w.join(".trash"))
    }

    /// The trash's served path, when it sits under the content root.
    fn trash_served(&self) -> Option<String> {
        let trash = self.trash_root()?;
        let root = std::fs::canonicalize(&self.root).ok()?;
        let rel = trash.strip_prefix(&root).ok()?;
        Some(format!("/{}", rel.to_string_lossy()))
    }

    fn ui(&self, identity: &Identity) -> Ui {
        self.ui.lock().unwrap().get(&ui_key(identity)).cloned().unwrap_or_default()
    }

    fn update_ui(&self, identity: &Identity, f: impl FnOnce(&mut Ui)) {
        let mut map = self.ui.lock().unwrap();
        f(map.entry(ui_key(identity)).or_default());
    }

    /// May this path be written? Canonical containment, so a symlink pointing
    /// out of the writable subtree cannot smuggle a write past the check —
    /// the same discipline the read path uses against the root.
    fn may_write(&self, path: &Path) -> bool {
        let Some(writable) = &self.writable else { return false };
        // The target may not exist yet (creating a file), so check the
        // nearest existing ancestor.
        let mut probe = path;
        loop {
            if let Ok(canonical) = std::fs::canonicalize(probe) {
                return canonical.starts_with(writable);
            }
            match probe.parent() {
                Some(parent) => probe = parent,
                None => return false,
            }
        }
    }

    /// Map a browse path (`/files/a/b`) to a served path (`/a/b`) and a
    /// filesystem path, rejecting anything that tries to leave the root.
    ///
    /// `..` is rejected outright rather than normalized: normalizing invites
    /// the classic mistake where the check and the open disagree. The opened
    /// path is verified against the canonical root afterwards regardless.
    fn resolve(&self, browse: &str) -> Option<(String, PathBuf)> {
        let rel = browse.strip_prefix("/files").unwrap_or("").trim_start_matches('/');
        let mut path = self.root.clone();
        for part in Path::new(rel).components() {
            match part {
                Component::Normal(p) => path.push(p),
                // Nothing else is a legal component of a served path.
                _ => return None,
            }
        }
        let canonical_root = std::fs::canonicalize(&self.root).ok()?;
        let canonical = std::fs::canonicalize(&path).ok()?;
        if !canonical.starts_with(&canonical_root) {
            return None;
        }
        let served = if rel.is_empty() { "/".to_string() } else { format!("/{rel}") };
        Some((served, canonical))
    }

    /// Is this served path visible to this device?
    ///
    /// A file is visible exactly when the policy would serve it. A directory
    /// is visible when it *leads to* something the policy would serve — you
    /// cannot ask a rule like `/apps/**` whether `/apps` is allowed, and the
    /// root matches no rule at all, so asking the tree is the only honest
    /// answer. This is what makes the policy legible as UI: you see the
    /// directories that go somewhere for you, and nothing else.
    fn visible(&self, identity: &Identity, served: &str, path: &Path, is_dir: bool) -> bool {
        if self.policy.authorize(identity, served) {
            return true;
        }
        // Somewhere you may write is somewhere you may go, even when it is
        // empty. Without this an empty writable directory is invisible — you
        // cannot reach the one place you are allowed to create anything,
        // which is how the rule first showed itself.
        if self.may_write(path) {
            return true;
        }
        is_dir && self.leads_somewhere(identity, served, path, PROBE_DEPTH)
    }

    /// Does any readable file live under this directory? Bounded in both
    /// depth and breadth — a deep or wide tree costs a predictable amount to
    /// answer, and answering "no" for something buried past the bound only
    /// ever hides, never leaks.
    fn leads_somewhere(&self, identity: &Identity, served: &str, path: &Path, depth: u32) -> bool {
        if depth == 0 {
            return false;
        }
        let Ok(read) = std::fs::read_dir(path) else { return false };
        let base = served.trim_end_matches('/');
        for entry in read.flatten().take(PROBE_BREADTH) {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let Ok(meta) = entry.metadata() else { continue };
            let child = format!("{base}/{name}");
            let found = if meta.is_dir() {
                self.leads_somewhere(identity, &child, &entry.path(), depth - 1)
            } else {
                self.policy.authorize(identity, &child)
            };
            if found {
                return true;
            }
        }
        false
    }

    fn list(&self, dir: &Path, served: &str, identity: &Identity) -> (Vec<Entry>, usize) {
        let Ok(read) = std::fs::read_dir(dir) else { return (Vec::new(), 0) };
        let base = served.trim_end_matches('/');
        let mut out = Vec::new();
        let mut hidden = 0usize;
        for entry in read.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            // Dot names are hidden, the unix convention — which is also what
            // keeps `.trash` out of the listing it lives beside. (Not counted
            // as "hidden": that number reports policy, not convention.)
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            // Symlinks are followed by metadata(); a link pointing outside the
            // root fails the canonical check when it is opened, so it can be
            // listed but never read. Leave it visible rather than lying.
            let child = format!("{base}/{name}");
            if !self.visible(identity, &child, &entry.path(), meta.is_dir()) {
                hidden += 1;
                continue;
            }
            out.push(Entry {
                name,
                served: child,
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
                modified: meta.modified().ok(),
                created: meta.created().ok(),
            });
        }
        // Directories first, then by name — the ordering every file browser
        // has, because it is the one people can predict.
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        (out, hidden)
    }

    /// The sidebar: the Linux file-manager places. Home, the two virtual
    /// views (Recent, Starred), then the standard folders that actually
    /// exist and are visible — the policy is still the UI, so a Downloads
    /// this device cannot fetch is a Downloads it does not see — and Trash
    /// when there is anywhere to write. Every place carries its conventional
    /// Phosphor fill glyph. Other top-level directories are deliberately
    /// NOT places: the rail is the fixed vocabulary; everything else is
    /// reached by browsing from Home.
    fn places(&self, identity: &Identity, browse: &str) -> Vec<Place> {
        let current = |target: &str| {
            browse == target || (target != "/files" && browse.starts_with(&format!("{target}/")))
        };
        let place = |label: &str, target: String, icon: &str| Place {
            label: label.to_string(),
            current: current(&target),
            target,
            icon: icon.to_string(),
        };
        let mut out = vec![
            place("Home", "/files".into(), "home-fill"),
            place("Recent", "/files/.recent".into(), "clock-fill"),
            place("Starred", "/files/.starred".into(), "star-fill"),
        ];
        const STANDARD: &[(&str, &str)] = &[
            ("Downloads", "download-fill"),
            ("Documents", "file-text-fill"),
            ("Pictures", "image-fill"),
            ("Videos", "film-fill"),
            ("Music", "music-fill"),
        ];
        let (top, _) = self.list(&self.root, "/", identity);
        let dirs: Vec<&Entry> = top.iter().filter(|e| e.is_dir).collect();
        for (name, icon) in STANDARD {
            if let Some(e) = dirs.iter().find(|e| e.name.eq_ignore_ascii_case(name)) {
                out.push(place(&e.name, browse_of(&e.served), icon));
            }
        }
        if let Some(trash) = self.trash_served() {
            out.push(place("Trash", browse_of(&trash), "trash-fill"));
        }
        out
    }
}


/// A single path component the user typed. Anything with a separator, a
/// leading dot, or the usual traversal spellings is refused outright rather
/// than sanitised — a name that needs cleaning is a name worth rejecting.
fn safe_name(raw: &str) -> Result<String, Status> {
    let name = raw.trim();
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name != ".."
        && Path::new(name).components().count() == 1;
    if ok { Ok(name.to_string()) } else { Err(Status::Internal) }
}

fn compile(kdl: &str) -> Result<Vec<u8>, Status> {
    rill_appkit::compile_page("files-app", kdl)
}

/// A timestamp as "YYYY-MM-DD HH:MM" — civil-from-days (Howard Hinnant's
/// algorithm), no calendar dependency.
fn fmt_time(t: std::time::SystemTime) -> String {
    let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) else { return "\u{2014}".into() };
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}")
}

fn human(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// The page's commonest action: a verb applied to one served path, carrying
/// the directory being viewed so the reply can redraw it.
///
/// Served paths end in a name from the disk, and path syntax has no opinion
/// about quotes — so this goes through the appkit builder, which escapes the
/// endpoint, rather than pasting it into KDL source. A file named `a"b` is
/// then just a file, not a broken page.
/// The edit app's address for a served directory, when there is one: the
/// entry's disk path relative to the editor's root ($HOME, matching the
/// edit app's own default). A directory outside that root gets no link —
/// the menu item simply does not appear, the same hiding the policy
/// already practises.
fn edit_at(root: &Path, served: &str) -> Option<String> {
    let disk = root.join(served.trim_start_matches('/'));
    let disk = disk.canonicalize().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let rel = disk.strip_prefix(&home).ok()?;
    let rel = rel.to_str()?;
    if rel.is_empty() {
        return Some("/edit".to_string());
    }
    Some(format!("/edit/at/{rel}"))
}

fn act_on(verb: &str, served: &str) -> String {
    rill_appkit::submit(&format!("/files/actions/{verb}{served}"), r#"field "dir" from="here""#)
}

/// The browse path for a served path — the inverse of `Files::resolve`.
fn browse_of(served: &str) -> String {
    if served == "/" {
        "/files".to_string()
    } else {
        format!("/files{served}")
    }
}

/// Every colour is a theme token, so the window follows the desktop palette
/// instead of fighting it. (The first cut hardcoded a light palette copied
/// from notes-app, which is why it rendered as white slabs on a dark page.)
/// What only the explorer draws: the grid of tiles and their pips.
/// Everything else — the shell, the bar, the rows — is rill-appkit's
/// vocabulary.
/// What only the explorer draws: the tile grid. Derived from the same F/P
/// scale as everything else — a tile is four controls wide, its icon two
/// line boxes tall.
fn extra_styles(m: &rill_appkit::Metrics) -> String {
    let p = m.padding;
    let tile = m.control_height() * 4.0;
    let big = m.line_height() * 2.0;
    let label = m.font_size - 2.0;
    let pip = m.font_size - 1.0;
    format!(
        "\
     style \"grid\" wrap=#true gap={p} padding=0\n\
     style \"tile\" background=\"surface-raised\" width={tile} gap={p} padding={p} corner=0 hover=\"tile-lit\"\n\
     style \"tile-lit\" width={tile} gap={p} padding={p} corner=0 background=\"elevation-lg\"\n\
     style \"tile-sel\" width={tile} gap={p} padding={p} corner=0 background=\"elevation-lg\"\n\
     style \"tilelabel\" color=\"text\" size={label} align=\"center\" ellipsis=#true underline=#false\n\
     style \"dot\" align=\"right\" color=\"text-muted\" background=\"#00000000\" size={pip} corner=0 padding={p} underline=#false\n\
     style \"dot-on\" align=\"right\" color=\"accent\" background=\"#00000000\" size={pip} corner=0 padding={p} underline=#false\n\
     style \"ico-grid\" color=\"accent\" align=\"center\" size={big}\n\
     style \"ico-grid-dim\" color=\"text-muted\" align=\"center\" size={big}\n"
    )
}

/// Header bar + sidebar frame shared by every page. `body` is the KDL for the
/// main pane's children.
fn chrome(
    served: &str,
    // The directory verbs act in — the page itself for a directory, the
    // parent for a file. Every action's "dir" field comes from here, and the
    // action guard requires it to be a directory.
    here: &str,
    // The active filter, so the search field re-serves showing what the
    // listing is being narrowed by.
    query: &str,
    places: &[Place],
    titlebar: &str,
    body: &str,
) -> String {
    // `here` is the directory this page renders; every verb submits it, so an
    // action never has to trust a path the client made up.
    let states = format!(
        "state \"here\" initial={here}\nstate \"name\" initial=\"\"\nstate \"mk\" initial=#false\n\
         state \"loc\" initial={loc}\nstate \"q\" initial={q}\nstate \"qz\" initial=\"\"\n",
        here = kdl_escape(here),
        loc = kdl_escape(served),
        q = kdl_escape(query),
    );
    // Density comes from the theme, so the studio's F/P steppers re-densify
    // the explorer too — every page build re-reads the file (a stat + a few
    // hundred bytes; the theme watcher already costs more).
    let metrics = rill_appkit::Metrics::from_theme_file(&rill_appkit::Metrics::theme_path());
    shell(&Shell {
        metrics,
        states: &states,
        titlebar,
        places,
        footer: None,
        sidebar_top_gap: metrics.sidebar_align_gap() as u32,
        extra_styles: &extra_styles(&metrics),
        content_style: None,
        body,
        rail_body: None,
        scroll_content: true,
    })
}

/// The kind label a listing sorts and displays by: "folder", or the
/// lowercased extension.
fn kind_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        "folder".to_string()
    } else {
        name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
    }
}

/// The listing exactly as the page presents it: the device's filter, then
/// its sort, over what the policy shows this identity. The chosen trait
/// sorts *everything* — a folder named "m" sits between files "l" and "n",
/// not in a separate lump; ties fall back to name so the order is total and
/// stable. Selection stepping (the arrow keys) walks this same order, which
/// is why it is a function and not inline in the page.
fn page_order(files: &Files, dir: &Path, served: &str, identity: &Identity, ui: &Ui) -> Vec<Entry> {
    let (mut entries, _hidden) = files.list(dir, served, identity);
    if let Some(q) = &ui.filter {
        let q = q.to_lowercase();
        entries.retain(|e| e.name.to_lowercase().contains(&q));
    }
    entries.sort_by(|a, b| {
        let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
        let ord = match ui.sort {
            SortKey::Name => by_name(),
            SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
            SortKey::Modified => a.modified.cmp(&b.modified).then_with(by_name),
            SortKey::Type => {
                kind_of(&a.name, a.is_dir).cmp(&kind_of(&b.name, b.is_dir)).then_with(by_name)
            }
        };
        if ui.sort_desc { ord.reverse() } else { ord }
    });
    entries
}

fn directory_page(files: &Files, served: &str, dir: &Path, identity: &Identity) -> Result<Vec<u8>, Status> {
    let ui = files.ui(identity);
    let entries = page_order(files, dir, served, identity, &ui);
    let writable = files.may_write(dir);
    let here = browse_of(served);
    let places = files.places(identity, &here);

    let mut body = String::new();

    // --- keyboard --------------------------------------------------------
    // The page states what keys mean, like it states what buttons do; keys
    // only fire when no input is focused, so typing never moves the
    // selection. Arrows step the selection in page order; Enter opens what
    // is selected; the rest shadow toolbar verbs under their mainstream
    // combos. Left/Right stay unbound in list view so they keep meaning
    // history in the viewer.
    let nav = |step: &str| {
        format!("submit \"/files/actions/nav/{step}\" {{ field \"dir\" from=\"here\" }}")
    };
    body.push_str(&rill_appkit::key_action("down", &nav("next")));
    body.push_str(&rill_appkit::key_action("up", &nav("prev")));
    body.push_str(&rill_appkit::key_action("home", &nav("first")));
    body.push_str(&rill_appkit::key_action("end", &nav("last")));
    if !ui.list {
        body.push_str(&rill_appkit::key_action("right", &nav("next")));
        body.push_str(&rill_appkit::key_action("left", &nav("prev")));
    }
    if let Some(sel) = &ui.selected {
        body.push_str(&rill_appkit::key_link("enter", &browse_of(sel)));
        body.push_str(&rill_appkit::key_action(
            "escape",
            "submit \"/files/actions/deselect\" { field \"dir\" from=\"here\" }",
        ));
        if writable {
            body.push_str(&rill_appkit::key_action(
                "delete",
                &act_on("delete", sel),
            ));
        }
    }
    if writable {
        body.push_str(&rill_appkit::key_action("ctrl+shift+n", "toggle \"mk\""));
    }

    // --- the window strip ------------------------------------------------
    // Navigation, the view switch and the verbs live in the titlebar, which
    // this window's chrome hands to the document. The app therefore spends no
    // rows of its own on them, and the content pane is only the grid. Verbs
    // appear only when they apply, so the strip states what is possible right
    // now rather than showing controls that fail when pressed.
    // The bar splits at the sidebar seam, like the mock: navigation and the
    // view switch sit over the sidebar, the breadcrumb runs over the content.
    // SidebarHeader: the icon slot is the way home, and the title names
    // where you are. (Search returns here once it earns its field back.)
    let here_name = if served == "/" {
        "Home"
    } else if files.trash_served().as_deref() == Some(served) {
        "Trash"
    } else {
        served.rsplit('/').next().unwrap_or("Home")
    };
    let strip_left = rill_appkit::sidebar_header(
        &(rill_appkit::icon_slot("home", "navigate \"/files\"")
            + &rill_appkit::location_title(here_name)),
    );
    // TopToolbar: the location bar, then the toolbar's verbs. The host draws
    // Close in its own corner. The view switcher shows the current view and
    // opens the standard menu to choose — no more cycling.
    let glyph = if ui.list { "list" } else { "grid" };
    let mut tools = rill_appkit::location_bar(
        "loc",
        "submit \"/files/actions/goto\" { field \"loc\" from=\"loc\"; field \"dir\" from=\"here\" }",
    );
    // Star is view state, so it works in read-only trees too.
    if let Some(sel) = &ui.selected {
        let label = if ui.starred.contains(sel) { "Unstar" } else { "Star" };
        tools.push_str(&rill_appkit::text_button(
            label,
            &act_on("star", sel),
        ));
    }
    if writable {
        tools.push_str(&rill_appkit::text_button("New folder", "toggle \"mk\""));
        if let Some(sel) = &ui.selected {
            tools.push_str(&rill_appkit::danger_button(
                "Delete",
                &act_on("delete", sel),
            ));
        }
    }
    tools.push_str(&rill_appkit::menu_button(
        glyph,
        &[
            rill_appkit::MenuEntry::Item {
                label: "List view",
                icon: Some("list"),
                danger: false,
                wire: rill_appkit::MenuWire::Action(
                    "submit \"/files/actions/view/list\" { field \"dir\" from=\"here\" }",
                ),
            },
            rill_appkit::MenuEntry::Item {
                label: "Grid view",
                icon: Some("grid"),
                danger: false,
                wire: rill_appkit::MenuWire::Action(
                    "submit \"/files/actions/view/grid\" { field \"dir\" from=\"here\" }",
                ),
            },
        ],
    ));
    tools.push_str(&rill_appkit::close_button());
    let bar = strip_left + &rill_appkit::toolbar(&tools);

    // Rename appears only when something is selected.
    if writable && let Some(sel) = &ui.selected {
        let rename = rill_appkit::submit(
            &format!("/files/actions/rename{sel}"),
            r#"field "dir" from="here"; field "name" from="name""#,
        );
        body.push_str(&rill_appkit::panel_row(
            &(rill_appkit::input("name", "field", "Rename to\u{2026}", &rename)
                + &rill_appkit::text_button("Rename", &rename)),
        ));
    }

    // SortBar: four equal-width controls filling the strip. The active one
    // carries the caret. Present in both views — sorting is not a list
    // feature.
    let sort_btn = |key: &str, label: &str, on: bool| {
        rill_appkit::sort_control(
            label,
            on,
            ui.sort_desc,
            &format!("submit \"/files/actions/sort/{key}\" {{ field \"dir\" from=\"here\" }}"),
        )
    };
    // The sort bar and the create-folder form share one slot: toggling New
    // folder swaps the form in where the sort controls were, instead of
    // stacking another band above them.
    let sorts = rill_appkit::sort_bar(
        &(sort_btn("name", "Name", ui.sort == SortKey::Name)
            + &sort_btn("type", "Type", ui.sort == SortKey::Type)
            + &sort_btn("size", "Size", ui.sort == SortKey::Size)
            + &sort_btn("modified", "Modified", ui.sort == SortKey::Modified)),
    );
    if writable {
        let mkdir = "submit \"/files/actions/mkdir\" { field \"dir\" from=\"here\"; \
                     field \"name\" from=\"name\" }";
        body.push_str(&rill_appkit::unless("mk", &sorts));
        body.push_str(&rill_appkit::when(
            "mk",
            &rill_appkit::panel_row(
                &(rill_appkit::input("name", "field", "Folder name\u{2026}", mkdir)
                    + &rill_appkit::cta_button("Create", mkdir)
                    + &rill_appkit::text_button("Cancel", "toggle \"mk\"")),
            ),
        ));
    } else {
        body.push_str(&sorts);
    }
    if let Some(q) = &ui.filter {
        // The filter is visible state: what the listing is being narrowed
        // by, and a clear that submits an always-empty slot.
        body.push_str(&rill_appkit::panel_row(&format!(
            "\t\t\t\ttext {} style=\"muted\"\n{}",
            kdl_escape(&format!("filtering: {q}")),
            rill_appkit::text_button(
                "clear",
                "submit \"/files/actions/filter\" { field \"q\" from=\"qz\"; field \"dir\" from=\"here\" }",
            ),
        )));
    }

    if entries.is_empty() {
        body.push_str(&rill_appkit::empty_note(if ui.filter.is_some() {
            "Nothing matches the filter."
        } else {
            "Nothing here you can see."
        }));
    } else if ui.list {
        let mut rows = String::new();
        for entry in entries.iter() {
            let selected = ui.selected.as_deref() == Some(entry.served.as_str());
            let (glyph, tint) = if entry.is_dir { ("folder-fill", "ico") } else { ("file", "ico-dim") };
            let meta = if entry.is_dir { "\u{2014}".to_string() } else { human(entry.size) };
            let kind = if entry.is_dir {
                "Folder".to_string()
            } else {
                kind_of(&entry.name, false).to_uppercase()
            };
            // The row's context menu: the row's verbs as declared data. The
            // pip and right-click open the same menu (host-presented), and
            // an agent can enumerate these without opening anything.
            let select = act_on("select", &entry.served);
            let star_label =
                if ui.starred.contains(&entry.served) { "Unstar" } else { "Star" };
            let star = act_on("star", &entry.served);
            let delete = act_on("delete", &entry.served);
            let browse = browse_of(&entry.served);
            let in_edit = if entry.is_dir { edit_at(&files.root, &entry.served) } else { None };
            let mut entries_menu = vec![
                rill_appkit::MenuEntry::Item {
                    label: "Open",
                    icon: None,
                    danger: false,
                    wire: rill_appkit::MenuWire::Target(&browse),
                },
            ];
            if let Some(at) = &in_edit {
                entries_menu.push(rill_appkit::MenuEntry::Item {
                    label: "Open folder in Edit",
                    icon: Some("pencil"),
                    danger: false,
                    wire: rill_appkit::MenuWire::Target(at),
                });
            }
            entries_menu.extend([
                rill_appkit::MenuEntry::Item {
                    label: star_label,
                    icon: Some("star"),
                    danger: false,
                    wire: rill_appkit::MenuWire::Action(&star),
                },
                rill_appkit::MenuEntry::Item {
                    label: "Properties",
                    icon: None,
                    danger: false,
                    wire: rill_appkit::MenuWire::Action(&select),
                },
            ]);
            if writable {
                entries_menu.push(rill_appkit::MenuEntry::Separator);
                entries_menu.push(rill_appkit::MenuEntry::Item {
                    label: "Delete",
                    icon: Some("trash"),
                    danger: true,
                    wire: rill_appkit::MenuWire::Action(&delete),
                });
            }
            rows.push_str(&rill_appkit::file_row(&rill_appkit::FileRow {
                selected,
                icon: (glyph, tint),
                title: &entry.name,
                target: &browse,
                title_style: if entry.is_dir { "file-name--dir" } else { "file-name" },
                cells: &[(kind, "cell-kind"), (meta, "cell-meta")],
                trailing: Some(("dots-vertical", "menu".to_string())),
                menu: Some(rill_appkit::menu(&entries_menu)),
            }));
        }
        body.push_str(&rill_appkit::list_view(&rows));
        // Everything knowable about the selection, from the filesystem the
        // server can actually see. ("Open with" needs the client's app
        // registry — the server honestly does not know what is installed.)
        if let Some(sel) = &ui.selected
            && let Some(e) = entries.iter().find(|e| &e.served == sel)
        {
            let mut props = format!(
                "\t\t\t\trow style=\"prop-row\" {{ icon \"{}\" style=\"{}\"; \
                 text {} style=\"prop-title\" }}\n",
                if e.is_dir { "folder-fill" } else { "file" },
                if e.is_dir { "ico" } else { "ico-dim" },
                kdl_escape(&e.name),
            );
            props.push_str(&rill_appkit::property_row(
                "Type",
                &if e.is_dir { "Folder".into() } else { kind_of(&e.name, false).to_uppercase() },
            ));
            if !e.is_dir {
                props.push_str(&rill_appkit::property_row(
                    "Size",
                    &format!("{} bytes ({})", e.size, human(e.size)),
                ));
            }
            let time = |t: &Option<std::time::SystemTime>| {
                t.map(fmt_time).unwrap_or_else(|| "\u{2014}".into())
            };
            props.push_str(&rill_appkit::property_row("Created", &time(&e.created)));
            props.push_str(&rill_appkit::property_row("Modified", &time(&e.modified)));
            props.push_str(&rill_appkit::property_row("Where", &e.served));
            body.push_str(&rill_appkit::panel_row(&format!(
                "\t\t\t\tcolumn gap=4 padding=0 {{\n{props}\t\t\t\t}}\n"
            )));
        }
    } else {
        // Grid: a wrapping row of fixed-width tiles.
        body.push_str("\t\t\trow style=\"grid\" {\n");
        for entry in entries.iter() {
            let selected = ui.selected.as_deref() == Some(entry.served.as_str());
            let (glyph, tint) =
                if entry.is_dir { ("folder-fill", "ico-grid") } else { ("file", "ico-grid-dim") };
            // Every part of a tile aligns itself within the tile's width, so
            // the grid keeps its rhythm without spacer scaffolding.
            let select = act_on("select", &entry.served);
            let delete = act_on("delete", &entry.served);
            let browse = browse_of(&entry.served);
            let mut tile_menu = vec![
                rill_appkit::MenuEntry::Item {
                    label: "Open",
                    icon: None,
                    danger: false,
                    wire: rill_appkit::MenuWire::Target(&browse),
                },
                rill_appkit::MenuEntry::Item {
                    label: "Properties",
                    icon: None,
                    danger: false,
                    wire: rill_appkit::MenuWire::Action(&select),
                },
            ];
            if writable {
                tile_menu.push(rill_appkit::MenuEntry::Separator);
                tile_menu.push(rill_appkit::MenuEntry::Item {
                    label: "Delete",
                    icon: Some("trash"),
                    danger: true,
                    wire: rill_appkit::MenuWire::Action(&delete),
                });
            }
            body.push_str(&format!(
                "\t\t\t\tcolumn style=\"{tile}\" target={target} {{\n\
                 \t\t\t\t\tbutton icon=\"dots-vertical\" style=\"{dot}\" {{ menu }}\n\
                 \t\t\t\t\ticon \"{glyph}\" style=\"{tint}\"\n\
                 \t\t\t\t\ttext {label} style=\"tilelabel\"\n\
                 \t\t\t\t\t{menu}\n\
                 \t\t\t\t}}\n",
                tile = if selected { "tile-sel" } else { "tile" },
                dot = if selected { "dot-on" } else { "dot" },
                label = kdl_escape(&entry.name),
                // The tile's target ends in a name from disk — see `act_on`.
                target = kdl_escape(&browse),
                menu = rill_appkit::menu(&tile_menu),
            ));
        }
        body.push_str("\t\t\t}\n");
    }

    body.push_str("\t\t\tspacer\n");
    let _ = here;
    let q = ui.filter.clone().unwrap_or_default();
    compile(&chrome(served, &browse_of(served), &q, &places, &bar, &body))
}

fn file_page(files: &Files, served: &str, path: &Path, size: u64, identity: &Identity) -> Result<Vec<u8>, Status> {
    let places = files.places(identity, &browse_of(served));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // The strip carries where you are and what this is; the pane is the file.
    // Same split as a directory page: back over the sidebar, path over the
    // content.
    let parent = match served.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => served[..i].to_string(),
    };
    let bar = rill_appkit::sidebar_header(
        &(rill_appkit::icon_slot("home", "navigate \"/files\"")
            + &rill_appkit::location_title(&name)),
    ) + &rill_appkit::toolbar(
        &(rill_appkit::location_bar(
            "loc",
            "submit \"/files/actions/goto\" { field \"loc\" from=\"loc\"; field \"dir\" from=\"here\" }",
        ) + &format!("\t\t\t\ttext {} style=\"muted\"\n", kdl_escape(&human(size)))
            + &rill_appkit::close_button()),
    );
    let mut body = format!("\t\t\ttext {} style=\"title\"\n", kdl_escape(&name));

    // The explorer browses; it does not read. File contents are never opened
    // here — a file's page is its metadata, and viewing/editing belongs to
    // dedicated apps (a text app, an image app) that open the file under
    // their own capability grants.
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    let kind = if ext.is_empty() { "File".to_string() } else { format!("{ext} file") };
    body.push_str(&format!(
        "\t\t\tcolumn gap=6 padding=14 style=\"card\" {{ \
         text {} style=\"muted\"; text {} style=\"muted\" }}\n",
        kdl_escape(&kind),
        kdl_escape(&human(size)),
    ));
    body.push_str(
        "\t\t\ttext \"The explorer only browses \u{2014} this file opens in its own app.\" style=\"muted\"\n",
    );
    body.push_str("\t\t\tspacer\n");
    compile(&chrome(served, &browse_of(&parent), "", &places, &bar, &body))
}

/// The two virtual views, Recent and Starred: pages over the same visible
/// tree, not places in it. Their rows link to the real entries; the trailing
/// control is the star toggle, so Recent is where things get starred and
/// Starred is where they get unstarred.
fn virtual_page(
    files: &Files,
    identity: &Identity,
    title: &str,
    served: &str,
    entries: &[Entry],
    empty_note: &str,
) -> Result<Vec<u8>, Status> {
    let ui = files.ui(identity);
    let browse = browse_of(served);
    let places = files.places(identity, &browse);
    let bar = rill_appkit::sidebar_header(
        &(rill_appkit::icon_slot("home", "navigate \"/files\"")
            + &rill_appkit::location_title(title)),
    ) + &rill_appkit::toolbar(
        &(rill_appkit::location_bar(
            "loc",
            "submit \"/files/actions/goto\" { field \"loc\" from=\"loc\"; field \"dir\" from=\"here\" }",
        ) + &rill_appkit::close_button()),
    );
    let mut body = String::new();
    if entries.is_empty() {
        body.push_str(&rill_appkit::empty_note(empty_note));
    } else {
        let mut rows = String::new();
        for e in entries {
            let (glyph, tint) = if e.is_dir { ("folder-fill", "ico") } else { ("file", "ico-dim") };
            let meta = if e.is_dir { "\u{2014}".to_string() } else { human(e.size) };
            let when = e.modified.map(fmt_time).unwrap_or_else(|| "\u{2014}".into());
            let star = if ui.starred.contains(&e.served) { "star-fill" } else { "star" };
            rows.push_str(&rill_appkit::file_row(&rill_appkit::FileRow {
                selected: false,
                icon: (glyph, tint),
                title: &e.name,
                target: &browse_of(&e.served),
                title_style: if e.is_dir { "file-name--dir" } else { "file-name" },
                cells: &[(when, "cell-kind"), (meta, "cell-meta")],
                trailing: Some((
                    star,
                    act_on("star", &e.served),
                )),
                // Virtual rows link to real entries; their menus can grow
                // once the verbs make sense from here (unstar, open).
                menu: None,
            }));
        }
        body.push_str(&rill_appkit::list_view(&rows));
    }
    body.push_str("\t\t\tspacer\n");
    compile(&chrome(served, &browse, "", &places, &bar, &body))
}

/// Newest visible files anywhere in the tree — a bounded walk with the same
/// visibility rule as every listing, so Recent can never surface something a
/// GET would hide.
fn recent_page(files: &Files, identity: &Identity) -> Result<Vec<u8>, Status> {
    const RECENT_SHOWN: usize = 50;
    const RECENT_SCAN: usize = 4000;
    let mut found: Vec<Entry> = Vec::new();
    let mut stack: Vec<(PathBuf, String, u32)> = vec![(files.root.clone(), "/".into(), 0)];
    let mut visited = 0usize;
    while let Some((dir, served, depth)) = stack.pop() {
        if depth > PROBE_DEPTH {
            continue;
        }
        let (entries, _) = files.list(&dir, &served, identity);
        for e in entries {
            visited += 1;
            if visited > RECENT_SCAN {
                break;
            }
            if e.is_dir {
                stack.push((dir.join(&e.name), e.served.clone(), depth + 1));
            } else {
                found.push(e);
            }
        }
        if visited > RECENT_SCAN {
            break;
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.modified));
    found.truncate(RECENT_SHOWN);
    virtual_page(files, identity, "Recent", "/.recent", &found, "Nothing recent to show.")
}

/// The device's starred entries, resolved fresh — a star on something that
/// has since vanished (or become invisible) simply does not render.
fn starred_page(files: &Files, identity: &Identity) -> Result<Vec<u8>, Status> {
    let ui = files.ui(identity);
    let mut starred: Vec<&String> = ui.starred.iter().collect();
    starred.sort();
    let mut entries: Vec<Entry> = Vec::new();
    for served in starred {
        let Some((served, path)) = files.resolve(&format!("/files{served}")) else { continue };
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !files.visible(identity, &served, &path, meta.is_dir()) {
            continue;
        }
        entries.push(Entry {
            name: served.rsplit('/').next().unwrap_or_default().to_string(),
            served,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            created: meta.created().ok(),
        });
    }
    virtual_page(
        files,
        identity,
        "Starred",
        "/.starred",
        &entries,
        "Nothing starred yet \u{2014} select something and press Star.",
    )
}

impl AppHandler for Files {
    fn get(&self, path: &str, identity: &Identity) -> Option<Vec<u8>> {
        // The virtual views first: dot-led names cannot be created through
        // the app (safe_name refuses them), so these take nothing reachable
        // away from the tree.
        match path {
            "/files/.recent" => return recent_page(self, identity).ok(),
            "/files/.starred" => return starred_page(self, identity).ok(),
            _ => {}
        }
        let (served, target) = self.resolve(path)?;
        let meta = std::fs::metadata(&target).ok()?;
        // The handler's own prefix was authorized by the server; the *target*
        // is a different path and gets its own check. Without this, /files
        // would be a way to read what the policy hides.
        if !self.visible(identity, &served, &target, meta.is_dir()) {
            return None;
        }
        let page = if meta.is_dir() {
            directory_page(self, &served, &target, identity)
        } else {
            file_page(self, &served, &target, meta.len(), identity)
        };
        page.ok()
    }

    fn action(
        &self,
        path: &str,
        fields: &[(String, ActionValue)],
        identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        // Every verb names the directory it acts in, and every one of them
        // re-checks that directory the same way a GET would: the ACTION
        // prefix being authorized says nothing about the path in the fields.
        let dir_browse = rill_appkit::field(fields, "dir").ok_or(Status::NotFound)?;

        // Star and goto run before that guard: they can originate from the
        // virtual pages (Recent, Starred), whose "here" is not a filesystem
        // directory. Each validates what it actually touches and returns
        // whatever page "dir" names — virtual or real — through the same
        // GET path a link would take.
        if let Some(target) = path.strip_prefix("/files/actions/star") {
            let (served, fs_path) =
                self.resolve(&format!("/files{target}")).ok_or(Status::NotFound)?;
            let meta = std::fs::metadata(&fs_path).map_err(|_| Status::NotFound)?;
            if !self.visible(identity, &served, &fs_path, meta.is_dir()) {
                return Err(Status::NotFound);
            }
            self.update_ui(identity, |ui| {
                if !ui.starred.remove(&served) {
                    ui.starred.insert(served.clone());
                }
            });
            return self.get(dir_browse, identity).ok_or(Status::NotFound);
        }
        if path == "/files/actions/goto" {
            // Type-a-path navigation. The typed address gets exactly the
            // checks a GET would run — resolve, then visibility — so the
            // field can never see more than a link could. Anything invalid
            // or invisible falls back to the page you were on: a wrong guess
            // costs nothing, and reveals nothing.
            if let Some(loc) = rill_appkit::field(fields, "loc") {
                let loc = loc.trim();
                let browse = if loc.starts_with("/files") {
                    loc.to_string()
                } else {
                    format!("/files/{}", loc.trim_start_matches('/'))
                };
                if let Some(page) = self.get(&browse, identity) {
                    return Ok(page);
                }
            }
            return self.get(dir_browse, identity).ok_or(Status::NotFound);
        }

        let (served, dir) = self.resolve(dir_browse).ok_or(Status::NotFound)?;
        if !self.visible(identity, &served, &dir, true) || !dir.is_dir() {
            return Err(Status::NotFound);
        }

        match path {
            // View state: no filesystem effect, so these run before the
            // write checks and work in a read-only tree too.
            p if p.starts_with("/files/actions/select/") => {
                // The row submits the entry's browse path verbatim; storing
                // anything else breaks the match. (A prepended "/files" here
                // once double-prefixed every selection into never matching.)
                let target = Some(p["/files/actions/select".len()..].to_string());
                // Selecting the same thing twice clears it, so a click can
                // undo itself without a separate control.
                self.update_ui(identity, |ui| {
                    ui.selected = if ui.selected == target { None } else { target };
                });
                return directory_page(self, &served, &dir, identity);
            }
            p if p.starts_with("/files/actions/nav/") => {
                // Arrow keys: step the selection through the page's own
                // order. Stateless in the URL — the order is recomputed the
                // way the page computes it, so what you see step is what
                // steps.
                let ui = self.ui(identity);
                let order = page_order(self, &dir, &served, identity, &ui);
                let cur = ui
                    .selected
                    .as_deref()
                    .and_then(|sel| order.iter().position(|e| e.served == sel));
                let idx = match (&p["/files/actions/nav/".len()..], cur) {
                    _ if order.is_empty() => None,
                    ("next", Some(i)) => Some((i + 1).min(order.len() - 1)),
                    ("prev", Some(i)) => Some(i.saturating_sub(1)),
                    // Nothing selected yet: either arrow starts at that end.
                    ("next", None) | ("first", _) => Some(0),
                    ("prev", None) | ("last", _) => Some(order.len() - 1),
                    _ => return Err(Status::NotFound),
                };
                if let Some(i) = idx {
                    let target = Some(order[i].served.clone());
                    self.update_ui(identity, |ui| ui.selected = target);
                }
                return directory_page(self, &served, &dir, identity);
            }
            "/files/actions/deselect" => {
                self.update_ui(identity, |ui| ui.selected = None);
                return directory_page(self, &served, &dir, identity);
            }
            p if p.starts_with("/files/actions/sort/") => {
                // Same key again flips direction; a new key starts ascending.
                let key = match p.rsplit('/').next() {
                    Some("size") => SortKey::Size,
                    Some("type") => SortKey::Type,
                    Some("modified") => SortKey::Modified,
                    _ => SortKey::Name,
                };
                self.update_ui(identity, |ui| {
                    if ui.sort == key {
                        ui.sort_desc = !ui.sort_desc;
                    } else {
                        ui.sort = key;
                        ui.sort_desc = false;
                    }
                });
                return directory_page(self, &served, &dir, identity);
            }
            "/files/actions/filter" => {
                let q = rill_appkit::field(fields, "q").unwrap_or_default();
                let q = q.trim();
                let filter = (!q.is_empty()).then(|| q.to_string());
                self.update_ui(identity, |ui| ui.filter = filter);
                return directory_page(self, &served, &dir, identity);
            }
            p if p.starts_with("/files/actions/view/") => {
                let list = p.ends_with("/list");
                self.update_ui(identity, |ui| {
                    ui.list = list;
                    ui.selected = None;
                });
                return directory_page(self, &served, &dir, identity);
            }
            "/files/actions/mkdir" => {
                let name = safe_name(rill_appkit::field(fields, "name").unwrap_or_default())?;
                let target = dir.join(&name);
                if !self.may_write(&target) {
                    return Err(Status::NotFound);
                }
                std::fs::create_dir(&target).map_err(|_| Status::Internal)?;
            }
            p if p.starts_with("/files/actions/rename/") => {
                let from = &format!("/files{}", &p["/files/actions/rename".len()..]);
                let (_, source) = self.resolve(from).ok_or(Status::NotFound)?;
                let name = safe_name(rill_appkit::field(fields, "name").unwrap_or_default())?;
                let target = dir.join(&name);
                if !self.may_write(&source) || !self.may_write(&target) {
                    return Err(Status::NotFound);
                }
                if target.exists() {
                    return Err(Status::Internal);
                }
                std::fs::rename(&source, &target).map_err(|_| Status::Internal)?;
            }
            p if p.starts_with("/files/actions/delete/") => {
                let victim = &format!("/files{}", &p["/files/actions/delete".len()..]);
                let (_, target) = self.resolve(victim).ok_or(Status::NotFound)?;
                if !self.may_write(&target) {
                    return Err(Status::NotFound);
                }
                let meta = std::fs::metadata(&target).map_err(|_| Status::NotFound)?;
                let trash = self.trash_root();
                let in_trash = trash.as_ref().is_some_and(|t| target.starts_with(t));
                match trash.filter(|_| !in_trash) {
                    // Linux-like delete: a move into the trash, whole trees
                    // included — the trash is the undo. Name collisions get a
                    // numeric suffix rather than clobbering what is there.
                    Some(trash) => {
                        std::fs::create_dir_all(&trash).map_err(|_| Status::Internal)?;
                        let name = target
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .ok_or(Status::NotFound)?;
                        let mut dest = trash.join(&name);
                        let mut n = 1u32;
                        while dest.exists() {
                            dest = trash.join(format!("{name}-{n}"));
                            n += 1;
                        }
                        std::fs::rename(&target, &dest).map_err(|_| Status::Internal)?;
                    }
                    // Deleting *in* the trash is the permanent one.
                    // Directories only when empty: recursive removal from a
                    // click can wait until there is an undo to catch it.
                    None if meta.is_dir() => {
                        std::fs::remove_dir(&target).map_err(|_| Status::Internal)?
                    }
                    None => std::fs::remove_file(&target).map_err(|_| Status::Internal)?,
                }
            }
            _ => return Err(Status::NotFound),
        }
        // The selection named something that may no longer exist.
        self.update_ui(identity, |ui| ui.selected = None);
        directory_page(self, &served, &dir, identity)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root: Option<String> = None;
    let mut identity: Option<String> = None;
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 7332;
    let mut writable: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--writable" => {
                writable = args.get(i + 1).cloned();
                i += 2;
            }
            "--identity" => {
                identity = args.get(i + 1).cloned();
                i += 2;
            }
            "--bind" => {
                bind = args[i + 1].clone();
                i += 2;
            }
            "--port" => {
                port = args[i + 1].parse().expect("port");
                i += 2;
            }
            other if root.is_none() => {
                root = Some(other.to_string());
                i += 1;
            }
            other => {
                eprintln!("files-app: unexpected argument {other}");
                std::process::exit(1);
            }
        }
    }
    let (Some(root), Some(identity)) = (root, identity) else {
        eprintln!(
            "usage: files-app <content-root> --identity <dir> [--writable DIR] \
             [--bind ADDR] [--port N]"
        );
        std::process::exit(1);
    };

    // The same policy file the server enforces. Loaded once, like the server
    // does — if that ever gains hot-reload, this needs it too or the listing
    // and the fetch will disagree.
    let policy_path = PathBuf::from(&identity).join("policy.toml");
    let policy = match std::fs::read_to_string(&policy_path).map(|t| Policy::parse(&t)) {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            eprintln!("files-app: {}: {e}", policy_path.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("files-app: {}: {e}", policy_path.display());
            std::process::exit(1);
        }
    };

    // Two caps, both about the same thing: how many places freed memory can
    // go to sit and not come back.
    //
    // glibc gives a thread that contends for the heap an arena of its own,
    // up to 64 MiB, and never returns one to the system. One worker per core
    // therefore means one arena per core: measured on a 32-core desktop,
    // serving a few dozen concurrent pages took this process from 16 MiB to
    // 311 MiB and left it there, none of it live. A desktop's app server is
    // not compute-bound — it reads a little, formats a document, writes it —
    // so the parallelism was buying nothing and costing everything.
    //
    // SAFETY: mallopt before any worker thread exists, which is the only
    // time the arena limit can still be set.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, WORKERS as libc::c_int);
    }
    // And hand the free memory back. Capping the arenas bounds how high a
    // burst can push this process; trimming is what brings it down again.
    // Without it a desktop that was busy once stays expensive for as long
    // as it is left running, which on a machine with a gigabyte is the
    // difference between idling and swapping.
    std::thread::Builder::new()
        .name("trim".into())
        .spawn(|| {
            loop {
                std::thread::sleep(TRIM_EVERY);
                // SAFETY: no arguments, no invariants; returns freed pages
                // at the top of each arena to the kernel.
                unsafe {
                    libc::malloc_trim(0);
                }
            }
        })
        .expect("trim thread");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let cfg = ServerConfig::new(root.clone(), identity);
        let mut server = Server::bind(&bind, port, cfg).await.expect("bind");
        let writable = writable.map(PathBuf::from);
        match &writable {
            Some(dir) => eprintln!("files-app: writes confined to {}", dir.display()),
            None => eprintln!("files-app: read-only (no --writable)"),
        }
        // The app menu as an app: the published applications, searchable.
        server.dynamic("/launcher", Arc::new(launcher_app::Launcher::new(PathBuf::from(&root))));
        server.dynamic("/files", Arc::new(Files::new(root.into(), policy, writable)));
        // The theme studio rides the same server (an app composing with it,
        // like /files itself). It edits the desktop's theme.toml, which only
        // means anything when server and desktop share a machine — true for
        // the demo appliance, and stated in the studio's own docs.
        // One resolver, shared with the compositor and the viewport. This
        // used to compute the path inline from HOME, which ignored
        // XDG_CONFIG_HOME — so a benchmark that set it configured the
        // compositor from its own workload theme and these apps from the
        // developer's `~/.config`, and reported the run as hermetic.
        let theme_path = rill_appkit::Metrics::theme_path();
        server.dynamic("/studio", Arc::new(studio_app::Studio::new(theme_path.clone())));
        // The terminal rides here too. It is a served app like the others —
        // the shell it spawns is the *server's*, which is the same machine on
        // this desktop and deliberately not assumed to be anywhere else.
        let login_shell =
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        server.dynamic("/term", Arc::new(term_app::Term::new(&login_shell, theme_path.clone())));
        // The resource meter: a widget's document, served like everything
        // else. It reads this machine's /proc, which is the same assumption
        // the terminal makes about whose shell it is spawning.
        server.dynamic("/meter", Arc::new(meter_app::Meter::new(theme_path.clone())));
        // The machine's memory as an app — the first client of the history
        // query surface, and the same read path the agent surface will use.
        // History lives in XDG data, the unlock in the device identity dir;
        // both follow the same defaults the compositor and CLI use.
        let history_dir = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".local/share"))
            })
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("rill/history");
        let history_identity = std::env::var("RILL_IDENTITY")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                std::path::Path::new(&home).join(".config").join("rill")
            });
        server.dynamic(
            "/history",
            Arc::new(history_app::History::new(history_dir, history_identity)),
        );
        // The editor: the tree in the rail, the file in the pane. Its root
        // is the home directory (or RILL_EDIT_ROOT) — the same same-machine
        // trust the terminal's shell already extends.
        let edit_root = std::env::var("RILL_EDIT_ROOT").map(PathBuf::from).unwrap_or_else(|_| {
            std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
        });
        server.dynamic("/edit", Arc::new(edit_app::Edit::new(edit_root)));
        // And the art: the other half of what a desktop puts in its corners.
        server.dynamic("/ascii", Arc::new(ascii_app::Ascii::new(theme_path.clone())));
        // The music player. Playback happens in this process, out this
        // machine's default sink — the same same-machine assumption the
        // terminal makes about whose shell it spawns. The library root is
        // RILL_MUSIC_ROOT, or ~/Music.
        let music_root = std::env::var("RILL_MUSIC_ROOT").map(PathBuf::from).unwrap_or_else(
            |_| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join("Music"))
                    .unwrap_or_else(|_| PathBuf::from("/srv/music"))
            },
        );
        server.dynamic("/music", Arc::new(music_app::Music::new(music_root, theme_path)));
        eprintln!(
            "files-app: serving /files, /studio, /term, /meter, /edit, /ascii and /music on {bind}:{port}"
        );
        server.run().await.expect("run");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = "default_access = \"deny\"\n\
        [[rule]]\npath = \"/public/**\"\nallow = [\"anonymous\"]\n\
        [[rule]]\npath = \"/apps/**\"\nallow = [\"phone\"]\n";

    /// A tree with one public branch, one granted branch, and one nobody may
    /// read. Each test gets its own directory: they run in parallel and every
    /// one of them deletes its tree on the way out.
    fn tree(name: &str) -> (PathBuf, Files) {
        let root = std::env::temp_dir()
            .join(format!("files-app-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["public", "apps/reader", "private/deep"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("public/notice.txt"), "hello").unwrap();
        std::fs::write(root.join("apps/reader/manifest"), "app_id = \"reader\"").unwrap();
        std::fs::write(root.join("private/deep/secret.txt"), "secret").unwrap();
        let policy = Policy::parse(POLICY).unwrap();
        (root.clone(), Files::new(root, policy, None))
    }

    fn phone() -> Identity {
        Identity::Device("phone".into())
    }

    /// The property the whole app rests on: a device sees a directory only
    /// when it leads to something that device may actually read.
    #[test]
    fn listings_show_only_what_the_policy_would_serve() {
        let (root, files) = tree("listing");
        let (entries, hidden) = files.list(&root, "/", &phone());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"apps"), "granted branch listed: {names:?}");
        assert!(names.contains(&"public"), "anonymous branch listed: {names:?}");
        assert!(!names.contains(&"private"), "denied branch leaked: {names:?}");
        assert_eq!(hidden, 1, "the hidden one is counted, not named");

        // Anonymous sees only the public branch.
        let (entries, _) = files.list(&root, "/", &Identity::Anonymous);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["public"], "anonymous sees only /public");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The handler must re-check the target. The server only authorized the
    /// `/files/**` prefix, so without this the explorer would hand out
    /// exactly what the policy hides.
    #[test]
    fn hidden_paths_cannot_be_fetched_directly() {
        let (root, files) = tree("fetch");
        assert!(files.get("/files/private", &phone()).is_none(), "denied directory served");
        assert!(
            files.get("/files/private/deep/secret.txt", &phone()).is_none(),
            "denied file served"
        );
        assert!(files.get("/files/apps/reader/manifest", &phone()).is_some(), "granted file");
        assert!(
            files.get("/files/public/notice.txt", &Identity::Anonymous).is_some(),
            "public file, anonymous"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Sort and filter are view state: same key flips, a new key starts
    /// ascending, and the filter narrows what a device already sees — never
    /// what it does not.
    #[test]
    fn sort_and_filter_are_per_device_view_state() {
        let (root, files) = tree("sortfilter");
        let dir = [("dir".to_string(), ActionValue::Str("/files".into()))];
        files.action("/files/actions/sort/size", &dir, &phone()).unwrap();
        assert!(files.ui(&phone()).sort == SortKey::Size && !files.ui(&phone()).sort_desc);
        files.action("/files/actions/sort/size", &dir, &phone()).unwrap();
        assert!(files.ui(&phone()).sort_desc, "same key again flips direction");
        files.action("/files/actions/sort/type", &dir, &phone()).unwrap();
        assert!(
            files.ui(&phone()).sort == SortKey::Type && !files.ui(&phone()).sort_desc,
            "a new key starts ascending"
        );

        let mut fields = dir.to_vec();
        fields.push(("q".to_string(), ActionValue::Str("app".into())));
        files.action("/files/actions/filter", &fields, &phone()).unwrap();
        assert_eq!(files.ui(&phone()).filter.as_deref(), Some("app"));
        let mut clear = dir.to_vec();
        clear.push(("q".to_string(), ActionValue::Str("  ".into())));
        files.action("/files/actions/filter", &clear, &phone()).unwrap();
        assert_eq!(files.ui(&phone()).filter, None, "whitespace clears");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Design-loop helper: write the page bytes for a primed selection to
    /// RILL_DUMP_PAGE, for the preview harness. Not a test of anything.
    #[test]
    #[ignore = "writes a page for previewing; run explicitly"]
    fn dump_selected_page() {
        let Ok(out) = std::env::var("RILL_DUMP_PAGE") else { return };
        let (_root, files) = tree("dump-selected");
        let fields = [("dir".to_string(), ActionValue::Str("/files".into()))];
        let page = files.action("/files/actions/select/public", &fields, &phone()).unwrap();
        std::fs::write(out, page).unwrap();
    }

    /// A selected entry surfaces everything the server can actually know
    /// about it; sort by name intermixes folders and files alphabetically.
    #[test]
    fn selection_shows_properties_and_sort_intermixes() {
        let (root, files) = tree("props");
        let fields = [("dir".to_string(), ActionValue::Str("/files".into()))];
        let page = files.action("/files/actions/select/public", &fields, &phone()).unwrap();
        let doc = rill_doc::decode(&page).unwrap();
        let text: Vec<&str> =
            doc.strings.iter().map(|s| s.as_str()).collect();
        for needed in ["Created", "Modified", "Where", "/public"] {
            assert!(text.contains(&needed), "properties panel lacks {needed:?}");
        }
        // Name sort: "apps" < "public" alphabetically regardless of kind —
        // and files would interleave among them, not lump.
        let _ = std::fs::remove_dir_all(&root);
    }

    /// File names are the one page input nobody in this app chose. A quote in
    /// a name used to end the KDL string it was pasted into — the mild version
    /// being a listing that 500s, the sharp version being a name that closes
    /// its own value and writes page structure after it, so a row's "Open"
    /// could be made to submit something else entirely.
    ///
    /// Both views and both the list and grid paths build rows, so the test
    /// walks each: the name must arrive as *text*, and the page must compile.
    #[test]
    fn hostile_file_names_stay_data() {
        // A quote to close the value it lands in, then KDL that would be a
        // button wired to something the page never offered — the shape of the
        // attack. (No '/': a name cannot contain one, which is the only reason
        // the injected action has to be a toggle rather than a submit.)
        let nasty = r#"a" style="x" }; button "Gotcha" { toggle "mk" } text ""#;
        let names = [nasty, r#"back\slash"#, "brace{}s"];

        // The property is that a name changes text and nothing else, so the
        // comparison is against the same listing with ordinary names.
        let shape = |dir_name: &str, names: [&str; 3], list_view: bool| {
            let (root, files) = tree(dir_name);
            let dir = root.join("public");
            for name in names {
                std::fs::write(dir.join(name), "x").unwrap();
            }
            let fields = [("dir".to_string(), ActionValue::Str("/files/public".into()))];
            let verb =
                if list_view { "/files/actions/view/list" } else { "/files/actions/view/grid" };
            files.action(verb, &fields, &phone()).expect("view switch");
            let page = files.get("/files/public", &phone()).expect("listing served");
            let doc = rill_doc::decode(&page).expect("names still compile to a document");

            let mut kinds: Vec<&'static str> =
                doc.nodes.iter().map(|n| n.type_name()).collect();
            kinds.sort_unstable();
            let strings: Vec<String> = doc.strings.clone();
            let _ = std::fs::remove_dir_all(&root);
            (kinds, strings)
        };

        for list_view in [true, false] {
            let (hostile_kinds, hostile_strings) = shape("hostile", names, list_view);
            let (plain_kinds, _) = shape("benign", ["one", "two", "three"], list_view);

            assert_eq!(
                hostile_kinds, plain_kinds,
                "a file name changed the page's structure (list_view={list_view})"
            );
            // And it arrived whole, rather than truncated at the quote.
            assert!(
                hostile_strings.iter().any(|s| s.contains(nasty)),
                "the name should survive intact as data (list_view={list_view})"
            );
        }
    }

    /// The location field runs the same gate a link does: a typed path that
    /// resolves and is visible navigates; anything else quietly re-serves
    /// the page you were on — a wrong guess costs nothing, reveals nothing.
    #[test]
    fn goto_navigates_visible_paths_and_swallows_the_rest() {
        let (root, files) = tree("goto");
        let go = |loc: &str| {
            let fields = [
                ("dir".to_string(), ActionValue::Str("/files".into())),
                ("loc".to_string(), ActionValue::Str(loc.into())),
            ];
            files.action("/files/actions/goto", &fields, &phone()).expect("goto never errors")
        };
        let apps = go("/files/apps");
        assert_eq!(apps, files.get("/files/apps", &phone()).unwrap(), "typed = clicked");
        // The /files prefix is optional: people type filesystem-looking paths.
        assert_eq!(go("apps"), apps, "bare form reaches the same page");
        let home = files.get("/files", &phone()).unwrap();
        assert_eq!(go("/files/no-such-thing"), home, "a miss re-serves where you were");
        assert_eq!(go("/files/private"), home, "a hidden path answers exactly like a miss");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn writable_tree(name: &str) -> (PathBuf, Files) {
        let (root, files) = tree(name);
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::write(root.join("work/notes.txt"), "hello").unwrap();
        let policy = Policy::parse(POLICY).unwrap();
        let files = Files::new(files.root.clone(), policy, Some(root.join("work")));
        (root, files)
    }

    fn act(files: &Files, verb: &str, fields: &[(&str, &str)]) -> Result<Vec<u8>, Status> {
        let fields: Vec<(String, ActionValue)> = fields
            .iter()
            .map(|(k, v)| (k.to_string(), ActionValue::Str(v.to_string())))
            .collect();
        files.action(verb, &fields, &phone())
    }

    /// The verbs work where writing is allowed.
    #[test]
    fn verbs_act_inside_the_writable_subtree() {
        let (root, files) = writable_tree("verbs");

        act(&files, "/files/actions/mkdir", &[("dir", "/files/work"), ("name", "reports")])
            .expect("mkdir");
        assert!(root.join("work/reports").is_dir(), "folder created");

        act(&files, "/files/actions/rename/work/notes.txt", &[
            ("dir", "/files/work"),
            ("name", "renamed.txt"),
        ])
        .expect("rename");
        assert!(root.join("work/renamed.txt").is_file(), "renamed");
        assert!(!root.join("work/notes.txt").exists(), "old name gone");

        act(&files, "/files/actions/delete/work/renamed.txt", &[("dir", "/files/work")])
            .expect("delete");
        assert!(!root.join("work/renamed.txt").exists(), "deleted");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// And nowhere else. The policy authorizes *reads*; without confinement a
    /// device holding /files/** could delete the packs this desktop runs on.
    #[test]
    fn verbs_refuse_to_touch_anything_outside_it() {
        let (root, files) = writable_tree("confined");
        let pack = root.join("apps/reader/manifest");
        assert!(pack.is_file(), "fixture");

        assert!(
            act(&files, "/files/actions/delete/apps/reader/manifest", &[
                ("dir", "/files/apps/reader"),
            ])
            .is_err(),
            "deleting a readable-but-not-writable file must be refused"
        );
        assert!(pack.is_file(), "and it is still there");

        assert!(
            act(&files, "/files/actions/mkdir", &[("dir", "/files/public"), ("name", "x")])
                .is_err(),
            "creating outside the writable subtree must be refused"
        );
        assert!(!root.join("public/x").exists());

        // A name that tries to climb out is refused rather than sanitised.
        for bad in ["..", "../escape", "a/b", ".hidden", ""] {
            assert!(
                act(&files, "/files/actions/mkdir", &[("dir", "/files/work"), ("name", bad)])
                    .is_err(),
                "name {bad:?} must be refused"
            );
        }
        assert!(!root.join("escape").exists(), "nothing climbed out");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A read-only explorer offers no verbs at all, rather than verbs that
    /// fail when pressed.
    #[test]
    fn without_a_writable_subtree_nothing_writes() {
        let (root, files) = tree("readonly");
        assert!(
            act(&files, "/files/actions/mkdir", &[("dir", "/files/public"), ("name", "x")])
                .is_err(),
            "no writable subtree means no writing"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    /// Delete is a move into the trash; deleting in the trash is permanent;
    /// colliding names get a suffix instead of clobbering what is there.
    #[test]
    fn delete_moves_to_trash_then_deletes_for_real() {
        let (root, files) = writable_tree("trash");
        act(&files, "/files/actions/delete/work/notes.txt", &[("dir", "/files/work")])
            .expect("delete");
        assert!(!root.join("work/notes.txt").exists(), "gone from its place");
        assert!(root.join("work/.trash/notes.txt").is_file(), "landed in the trash");

        std::fs::write(root.join("work/notes.txt"), "second").unwrap();
        act(&files, "/files/actions/delete/work/notes.txt", &[("dir", "/files/work")])
            .expect("second delete");
        assert!(root.join("work/.trash/notes.txt-1").is_file(), "collision suffixed");

        act(&files, "/files/actions/delete/work/.trash/notes.txt", &[(
            "dir",
            "/files/work/.trash",
        )])
        .expect("delete in trash");
        assert!(!root.join("work/.trash/notes.txt").exists(), "permanent in the trash");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dot names stay out of listings — the unix convention, and what keeps
    /// `.trash` invisible beside the files it holds.
    #[test]
    fn dot_names_are_hidden_from_listings() {
        let (root, files) = writable_tree("dots");
        assert!(root.join("work/.trash").is_dir(), "trash exists (created eagerly)");
        let (entries, _) = files.list(&root.join("work"), "/work", &phone());
        assert!(
            entries.iter().all(|e| !e.name.starts_with('.')),
            "dot entries listed: {:?}",
            entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Stars are per-device view state: toggling, the Starred page, and the
    /// same policy line every other view obeys.
    #[test]
    fn star_toggles_per_device_and_respects_policy() {
        let (root, files) = writable_tree("star");
        // Star from the Starred page itself: "dir" names a virtual page and
        // the action still round-trips.
        act(&files, "/files/actions/star/work/notes.txt", &[("dir", "/files/.starred")])
            .expect("star");
        assert!(files.ui(&phone()).starred.contains("/work/notes.txt"), "starred");
        let page = files.get("/files/.starred", &phone()).expect("starred page");
        assert!(contains(&page, "notes.txt"), "starred page lists it");
        // Another device has its own stars.
        assert!(
            files.ui(&Identity::Device("laptop".into())).starred.is_empty(),
            "stars are per-device"
        );
        // Toggling again clears it.
        act(&files, "/files/actions/star/work/notes.txt", &[("dir", "/files/work")])
            .expect("unstar");
        assert!(files.ui(&phone()).starred.is_empty(), "unstarred");
        // What the policy hides cannot be starred.
        assert!(
            act(&files, "/files/actions/star/private/deep/secret.txt", &[("dir", "/files")])
                .is_err(),
            "starring a hidden path must be refused"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The virtual pages serve for every identity and show only what the
    /// policy would serve — Recent is a view over listings, so it inherits
    /// their visibility rule.
    #[test]
    fn virtual_pages_respect_the_policy() {
        let (root, files) = writable_tree("virtual");
        let phone_recent = files.get("/files/.recent", &phone()).expect("recent");
        assert!(contains(&phone_recent, "notice.txt"), "public file in recent");
        assert!(!contains(&phone_recent, "secret.txt"), "hidden file leaked into recent");
        let anon_recent = files.get("/files/.recent", &Identity::Anonymous).expect("anon recent");
        assert!(contains(&anon_recent, "notice.txt"), "anonymous sees public in recent");
        assert!(!contains(&anon_recent, "manifest"), "granted-only file leaked to anonymous");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Nothing outside the root is reachable, however the path is spelled.
    #[test]
    fn traversal_is_refused() {
        let (root, files) = tree("traversal");
        for attempt in ["/files/../etc/passwd", "/files/apps/../../etc", "/files/./../.."] {
            assert!(files.resolve(attempt).is_none(), "escaped via {attempt}");
        }
        assert!(files.resolve("/files/apps/reader").is_some(), "a normal path still resolves");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Arrow keys step the selection through exactly the order the page
    /// shows — the same sort the listing uses — and clamp at the ends
    /// instead of wrapping.
    #[test]
    fn selection_steps_in_page_order_and_clamps() {
        let (root, files) = tree("nav");
        let dir = [("dir".to_string(), ActionValue::Str("/files".into()))];
        let order: Vec<String> = {
            let ui = files.ui(&phone());
            page_order(&files, &root, "/", &phone(), &ui)
                .into_iter()
                .map(|e| e.served)
                .collect()
        };
        assert!(order.len() >= 2, "fixture has multiple visible entries");

        // Nothing selected: Down selects the first entry.
        files.action("/files/actions/nav/next", &dir, &phone()).unwrap();
        assert_eq!(files.ui(&phone()).selected.as_deref(), Some(order[0].as_str()));
        // Down again walks the visible order.
        files.action("/files/actions/nav/next", &dir, &phone()).unwrap();
        assert_eq!(files.ui(&phone()).selected.as_deref(), Some(order[1].as_str()));
        // Up walks back; at the top it stays put rather than wrapping.
        files.action("/files/actions/nav/prev", &dir, &phone()).unwrap();
        files.action("/files/actions/nav/prev", &dir, &phone()).unwrap();
        assert_eq!(files.ui(&phone()).selected.as_deref(), Some(order[0].as_str()));
        // End and Home jump; Escape's deselect clears.
        files.action("/files/actions/nav/last", &dir, &phone()).unwrap();
        assert_eq!(
            files.ui(&phone()).selected.as_deref(),
            Some(order.last().unwrap().as_str())
        );
        files.action("/files/actions/deselect", &dir, &phone()).unwrap();
        assert_eq!(files.ui(&phone()).selected, None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
