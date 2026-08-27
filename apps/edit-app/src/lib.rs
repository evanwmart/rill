//! A plaintext editor over one directory: the file tree in the rail, the
//! open file in the pane, Save writes it back. With the terminal beside it,
//! this is the working half of "a terminal and an editor are the IDE".
//!
//! Everything here is the platform's existing vocabulary — the kit shell
//! with a custom rail, a multiline `text_input` bound to state, actions
//! that re-serve the page. No editor widget was built for this; the day the
//! input grows syntax colour or soft wrap, every text field on the desktop
//! inherits it.
//!
//! **Scope.** The app edits inside one root named at construction, the same
//! same-machine trust the terminal already extends: a desktop that hands
//! you a shell has already handed you the files. Paths are still resolved
//! component-by-component — the protocol refuses `..` on the wire, and the
//! resolver refuses it again anyway, so escaping the root takes more than a
//! crafted target either way.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use rill_auth::Identity;
use rill_doc::kdl_escape;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

/// Files larger than this are declined rather than opened: the whole body
/// rides in one state value and comes back as one ACTION field, and that is
/// the wire's own ceiling for a field. A bigger file needs the editor to
/// send edits instead of buffers — a real design, not a bigger number.
const MAX_EDIT_BYTES: u64 = rill_protocol::MAX_FIELD_STRING as u64;

/// How much of an oversized file the read-only view shows. A view is not a
/// pager: past this the answer is a real chunked edit protocol, not a
/// bigger page.
const MAX_VIEW_BYTES: u64 = 512 * 1024;

/// Entries listed per directory before the tree says "…and N more" — a
/// node_modules should not cost the rail ten thousand rows.
const MAX_DIR_ENTRIES: usize = 400;

/// Indent per tree depth, px — VS Code's rhythm, tight enough that a deep
/// tree still fits a rail.
const INDENT: u32 = 12;

/// Depths past this share the deepest indent style: the rail is only so
/// wide, and a tree nested past twelve levels has already stopped reading
/// as indentation.
const MAX_DEPTH: u32 = 12;

const EXTRA_STYLES: &str = "\
 style \"editor\" font=\"mono\" size=13 color=\"text\" background=\"surface\"\n\
 style \"code-pane\" font=\"mono\" size=13 color=\"text\"\n\
 style \"quiet-banner\" color=\"text-muted\" size=12\n\
 style \"ro-text\" font=\"mono\" size=13 color=\"text\"\n\
 style \"ro-comment\" font=\"mono\" size=13 color=\"text-muted\"\n\
 style \"ro-string\" font=\"mono\" size=13 color=\"ansi-green\"\n\
 style \"ro-number\" font=\"mono\" size=13 color=\"ansi-cyan\"\n\
 style \"ro-keyword\" font=\"mono\" size=13 color=\"accent\"\n\
 style \"ro-gutter\" font=\"mono\" size=13 color=\"text-muted\"\n\
 style \"ro-ws\" font=\"mono\" size=13 color=\"border\"\n\
 style \"tree\" gap=1 padding=0\n\
 style \"tree-hover\" background=\"elevation-md\" color=\"text\" corner=4\n\
 style \"tree-file--hover\" background=\"elevation-md\" corner=4 valign=\"center\"\n\
 style \"tree-ico\" color=\"text-muted\" size=18\n\
 style \"tree-ico--active\" color=\"accent\" size=18\n\
 style \"tree-label\" color=\"text-muted\" size=13 underline=#false\n\
 style \"tree-label--active\" color=\"text\" size=13 underline=#false\n\
 style \"tree-more\" color=\"text-muted\" size=12\n\
 style \"empty\" color=\"text-muted\" size=14\n\
 style \"filename\" color=\"text-muted\" size=13\n";

/// The per-depth tree styles. Indentation lives in the style rather than in
/// a spacer because a directory row is one full-width flat button — the
/// whole row is the click target, the way every tree view works — and a
/// button's content can only be pushed over by its own padding.
fn tree_styles() -> String {
    let mut kdl = String::new();
    for d in 0..=MAX_DEPTH {
        let pad = 6 + d * INDENT;
        kdl.push_str(&format!(
            " style \"tree-dir-{d}\" color=\"text\" size=13 background=\"#00000000\" \
             width=\"fill\" align=\"left\" corner=4 padding-x={pad} padding-y=2 \
             hover=\"tree-hover\"\n"
        ));
        // Files: label-only, one style family with the directories. The
        // name starts where a directory's name does (past the chevron), so
        // the tree reads as one column of names with chevrons in the
        // margin — the icon that used to sit here was the tallest thing in
        // the row and said nothing the name did not.
        let fpad = pad + 22;
        kdl.push_str(&format!(
            " style \"tree-file-{d}\" color=\"text-muted\" size=13 background=\"#00000000\" \
             width=\"fill\" align=\"left\" corner=4 padding-x={fpad} padding-y=2 \
             hover=\"tree-hover\"\n\
             style \"tree-file-{d}--active\" color=\"text\" size=13 background=\"elevation-lg\" \
             width=\"fill\" align=\"left\" corner=4 padding-x={fpad} padding-y=2 \
             hover=\"tree-hover\"\n"
        ));
    }
    kdl
}

/// A directory row's context menu: its verbs as declared data.
fn dir_menu(rel: &str) -> String {
    rill_appkit::menu(&[
        rill_appkit::MenuEntry::Item {
            label: "New file…",
            icon: Some("plus"),
            danger: false,
            wire: rill_appkit::MenuWire::Action(&rill_appkit::submit(
                &format!("/edit/actions/newfile/{rel}"),
                "",
            )),
        },
        rill_appkit::MenuEntry::Item {
            label: "New folder…",
            icon: Some("folder"),
            danger: false,
            wire: rill_appkit::MenuWire::Action(&rill_appkit::submit(
                &format!("/edit/actions/newdir/{rel}"),
                "",
            )),
        },
    ])
}

fn file_menu(rel: &str) -> String {
    rill_appkit::menu(&[
        rill_appkit::MenuEntry::Item {
            label: "Rename…",
            icon: Some("pencil"),
            danger: false,
            wire: rill_appkit::MenuWire::Action(&rill_appkit::submit(
                &format!("/edit/actions/rename-target/{rel}"),
                "",
            )),
        },
        rill_appkit::MenuEntry::Separator,
        rill_appkit::MenuEntry::Item {
            label: "Delete",
            icon: Some("trash"),
            danger: true,
            wire: rill_appkit::MenuWire::Action(&rill_appkit::submit(
                &format!("/edit/actions/delete/{rel}"),
                "",
            )),
        },
    ])
}

/// The in-place rename row a staged rename becomes.
fn rename_row(rel: &str, d: u32) -> String {
    format!(
        "\t\t\trow gap=4 padding=0 {{ spacer size={pad}; \
         text_input bind=\"op-name\" style=\"hexfield\" placeholder=\"new name…\" {{ \
         submit {ep} {{ field \"name\" from=\"op-name\" }} }} \
         button \"✕\" style=\"tree-file-0\" {{ submit \"/edit/actions/dismiss\" }} }}\n",
        pad = 6 + d.min(MAX_DEPTH) * INDENT + 22,
        ep = kdl_escape(&format!("/edit/actions/rename/{rel}")),
    )
}

/// Per-device view state: which directories are unfolded, which file is
/// open. Kept server-side for the same reason the files app keeps its
/// selection here: document state cannot express "style this row if it is
/// the open file", and the server is local, so the round trip is felt as
/// instant.
#[derive(Default, Clone)]
struct Ui {
    expanded: BTreeSet<String>,
    open: Option<String>,
    /// The directory the tree is rooted at, when a hand-off scoped it —
    /// "open this folder in Edit" means *this folder*, not the whole home
    /// with one branch unfolded. `None` is the editor's own root.
    scope: Option<String>,
    /// A file operation waiting on a name: the tree grows an input row at
    /// its site until the name arrives or the op is dismissed. The studio's
    /// rename pattern — a context-menu item cannot carry an input, so it
    /// stages one.
    pending: Option<PendingOp>,
}

#[derive(Clone, PartialEq)]
enum PendingOp {
    /// A new file under this directory (rel; "" is the root).
    NewFile(String),
    /// A new directory under this directory.
    NewDir(String),
    /// Rename this path (file or directory).
    Rename(String),
}

pub struct Edit {
    root: PathBuf,
    ui: Mutex<HashMap<String, Ui>>,
}

/// A submitted file name: one path component, nothing sneaky. The same
/// refusals the resolver makes, made before a path is ever built.
fn clean_name(fields: &[(String, ActionValue)]) -> Result<String, Status> {
    let name = rill_appkit::field(fields, "name").unwrap_or_default().trim().to_string();
    if name.is_empty()
        || name.contains('/')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(Status::Internal);
    }
    Ok(name)
}

fn ui_key(identity: &Identity) -> String {
    match identity {
        Identity::Device(name) => name.clone(),
        Identity::Anonymous => String::new(),
    }
}

impl Edit {
    pub fn new(root: PathBuf) -> Edit {
        let root = root.canonicalize().unwrap_or(root);
        Edit { root, ui: Mutex::new(HashMap::new()) }
    }

    fn ui(&self, identity: &Identity) -> Ui {
        self.ui.lock().ok().and_then(|m| m.get(&ui_key(identity)).cloned()).unwrap_or_default()
    }

    fn update_ui(&self, identity: &Identity, f: impl FnOnce(&mut Ui)) {
        if let Ok(mut m) = self.ui.lock() {
            f(m.entry(ui_key(identity)).or_default());
        }
    }

    /// A relative path from the wire to a real path under the root, or
    /// nothing. The wire already refused `.` and `..` segments; refusing
    /// them again here means this function is safe even if it one day gets
    /// called with a string that never crossed the wire.
    fn resolve(&self, rel: &str) -> Option<PathBuf> {
        if rel.is_empty() {
            return Some(self.root.clone());
        }
        let candidate = Path::new(rel);
        if candidate.components().any(|c| !matches!(c, Component::Normal(_))) {
            return None;
        }
        Some(self.root.join(candidate))
    }

    /// The staged-op input row for a directory, when one is pending there.
    fn pending_row(&self, dir_rel: &str, d: u32, ui: &Ui, kdl: &mut String) {
        let (verb, placeholder) = match &ui.pending {
            Some(PendingOp::NewFile(at)) if at == dir_rel => ("create", "new file name…"),
            Some(PendingOp::NewDir(at)) if at == dir_rel => ("mkdir", "new folder name…"),
            _ => return,
        };
        kdl.push_str(&format!(
            "\t\t\trow gap=4 padding=0 {{ spacer size={pad}; \
             text_input bind=\"op-name\" style=\"hexfield\" placeholder={ph} {{ \
             submit {ep} {{ field \"name\" from=\"op-name\" }} }} \
             button \"✕\" style=\"tree-file-0\" {{ submit \"/edit/actions/dismiss\" }} }}\n",
            pad = 6 + d.min(MAX_DEPTH) * INDENT,
            ph = kdl_escape(placeholder),
            ep = kdl_escape(&format!("/edit/actions/{verb}/{dir_rel}")),
        ));
    }

    // ---- the tree ------------------------------------------------------

    /// One directory level into rail rows, recursing into unfolded dirs.
    fn tree_level(&self, dir: &Path, rel: &str, depth: u32, ui: &Ui, kdl: &mut String) {
        let Ok(read) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<(bool, String)> = read
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                (is_dir, name)
            })
            .collect();
        // Directories first, then files, each alphabetical and case-blind —
        // the order every tree a person has used puts them in.
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        let more = entries.len().saturating_sub(MAX_DIR_ENTRIES);
        entries.truncate(MAX_DIR_ENTRIES);
        let d = depth.min(MAX_DEPTH);
        for (is_dir, name) in entries {
            let child_rel =
                if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if is_dir {
                let open = ui.expanded.contains(&child_rel);
                let caret = if open { "chevron-down" } else { "chevron-right" };
                // The whole row is the button: flat until hovered, chevron
                // and name hanging left at the depth's indent. Right-click
                // carries the directory's verbs.
                kdl.push_str(&format!(
                    "\t\t\trow gap=0 padding=0 {{ button {label} icon=\"{caret}\" \
                     style=\"tree-dir-{d}\" {{ submit {ep} }};{menu} }}\n",
                    label = kdl_escape(&name),
                    ep = kdl_escape(&format!("/edit/actions/toggle/{child_rel}")),
                    menu = dir_menu(&child_rel),
                ));
                if open {
                    self.pending_row(&child_rel, d + 1, ui, kdl);
                    self.tree_level(&dir.join(&name), &child_rel, depth + 1, ui, kdl);
                }
            } else {
                let current = ui.open.as_deref() == Some(child_rel.as_str());
                // The kit's sidebar icon and label are sized for places —
                // a 19.6px glyph beside 14px text. Beside a directory row's
                // 13px label that read as a different, bulkier list, so the
                // tree carries its own pair matched to the button's metrics.
                let active = if current { "--active" } else { "" };
                // A button, like the directory rows: opening is an *action*,
                // and an action's refresh is in-place — the tree keeps its
                // scroll instead of snapping to the top on every click,
                // which navigation (a new page, new world) rightly resets.
                if ui.pending == Some(PendingOp::Rename(child_rel.clone())) {
                    kdl.push_str(&rename_row(&child_rel, d));
                } else {
                    kdl.push_str(&format!(
                        "\t\t\trow gap=0 padding=0 {{ button {label} \
                         style=\"tree-file-{d}{active}\" {{ submit {ep} }};{menu} }}\n",
                        label = kdl_escape(&name),
                        ep = kdl_escape(&format!("/edit/actions/open/{child_rel}")),
                        menu = file_menu(&child_rel),
                    ));
                }
            }
        }
        if more > 0 {
            kdl.push_str(&format!(
                "\t\t\trow gap=0 padding=0 {{ spacer size={}; text {} style=\"tree-more\" }}\n",
                6 + d * INDENT,
                kdl_escape(&format!("…and {more} more")),
            ));
        }
    }

    // ---- the pane ------------------------------------------------------

    /// The editor body and its seeded state for the open file, or the
    /// empty-pane invitation. Also returns whether Save belongs in the bar.
    /// `(states, body, savable, readable)`: savable means the input and
    /// Save belong on the page; readable means Edit is even worth
    /// offering — a binary or oversized file gets neither, instead of an
    /// Edit button that leads to the same refusal with more steps.
    fn pane(&self, ui: &Ui) -> (String, String, bool, bool) {
        let Some(rel) = ui.open.as_deref() else {
            return (
                String::new(),
                "\t\t\tspacer\n\t\t\trow { spacer; text \"Select a file to edit\" \
                 style=\"empty\"; spacer }\n\t\t\tspacer\n"
                    .into(),
                false,
                false,
            );
        };
        let Some(path) = self.resolve(rel) else {
            return (String::new(), Edit::notice("That path is not under this root."), false, false);
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_EDIT_BYTES {
            // Read-only, but *readable*: the whole reason the file was
            // opened. Highlighted and numbered like the editor, served as
            // plain runs — no buffer, no Save, and honestly labelled. The
            // real fix is an edit protocol that sends changes instead of
            // whole files; until then, reading must not be hostage to
            // editing's ceiling.
            let shown = std::fs::File::open(&path)
                .ok()
                .and_then(|mut f| {
                    use std::io::Read;
                    let mut buf = vec![0u8; MAX_VIEW_BYTES.min(size) as usize];
                    f.read_exact(&mut buf).ok()?;
                    // Cut on a char boundary, then on a line, so the tail
                    // is never a torn glyph or a half-sentence.
                    let mut text = String::from_utf8_lossy(&buf).into_owned();
                    if (size as usize) > text.len()
                        && let Some(nl) = text.rfind('\n')
                    {
                        text.truncate(nl);
                    }
                    Some(text)
                })
                .unwrap_or_default();
            let mut body = format!(
                "\t\t\ttext {} style=\"quiet-banner\"\n",
                kdl_escape(&format!(
                    "Read-only: {} KiB is past the {} KiB edit buffer{}",
                    size / 1024,
                    MAX_EDIT_BYTES / 1024,
                    if (size as usize) > shown.len() {
                        format!(" — showing the first {} KiB", shown.len() / 1024)
                    } else {
                        String::new()
                    }
                )),
            );
            body.push_str(&Edit::read_runs(rel, &shown));
            return (String::new(), body, false, false);
        }
        match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(body) => {
                    // One mode: the code node is view and editor at once —
                    // highlighted, numbered, and yours to click into. The
                    // lang is just the extension; the client's lexer
                    // decides what it means.
                    //
                    // The state is named *per file*. Opening is an action,
                    // and an action's in-place refresh carries staged
                    // edits by state name — exactly right for tree
                    // toggles (your unsaved work survives), exactly wrong
                    // across a file switch (the old buffer showed up
                    // wearing the new file's lexer). A per-file name makes
                    // the carry miss by construction.
                    let lang = rel.rsplit('.').next().unwrap_or("");
                    let slot = format!("body:{rel}");
                    (
                        format!("state {} initial={}\n", kdl_escape(&slot), kdl_escape(&body)),
                        format!(
                            "\t\t\tcode bind={} lang={} style=\"code-pane\"\n\
                             \t\t\tkey \"ctrl+s\" {{ submit {} {{ field \"body\" from={} }} }}\n",
                            kdl_escape(&slot),
                            kdl_escape(lang),
                            kdl_escape(&format!("/edit/actions/save/{rel}")),
                            kdl_escape(&slot),
                        ),
                        true,
                        true,
                    )
                }
                Err(_) => (
                    String::new(),
                    Edit::notice("Not a text file — nothing here to edit."),
                    false,
                    false,
                ),
            },
            Err(e) => {
                (String::new(), Edit::notice(&format!("Could not read the file: {e}")), false, false)
            }
        }
    }

    /// A file as highlighted read-only runs: gutter numbers, indent dots,
    /// the code surface's own classes and tokens — everything but the
    /// editing.
    fn read_runs(rel: &str, body: &str) -> String {
        use rill_ui::code::{Class, LineState, lang_of, spans};
        fn visible(t: &str) -> String {
            t.chars()
                .map(|c| match c {
                    '\t' => '⇥',
                    '\x1b' => '␛',
                    c if (c as u32) < 0x20 => '·',
                    c => c,
                })
                .collect()
        }
        let lang = lang_of(rel);
        let digits = body.lines().count().max(1).to_string().len().max(3);
        let mut out = String::new();
        let mut state = LineState::default();
        for (n, line) in body.lines().enumerate() {
            out.push_str("\t\t\trow gap=0 padding=0 {\n");
            out.push_str(&format!(
                "\t\t\t\ttext {} style=\"ro-gutter\"\n",
                kdl_escape(&format!("{:>digits$}  ", n + 1)),
            ));
            let indent_end = line.len() - line.trim_start().len();
            if indent_end > 0 {
                let dots: String = line[..indent_end]
                    .chars()
                    .map(|c| if c == '\t' { '⇥' } else { '·' })
                    .collect();
                out.push_str(&format!(
                    "\t\t\t\ttext {} style=\"ro-ws\"\n",
                    kdl_escape(&dots)
                ));
            }
            let rest = &line[indent_end..];
            if rest.is_empty() {
                out.push_str("\t\t\t\ttext \" \" style=\"ro-text\"\n");
            } else {
                match lang {
                    None => out.push_str(&format!(
                        "\t\t\t\ttext {} style=\"ro-text\"\n",
                        kdl_escape(&visible(rest))
                    )),
                    Some(lang) => {
                        for (class, range) in spans(rest, lang, &mut state) {
                            let style = match class {
                                Class::Plain => "ro-text",
                                Class::Comment => "ro-comment",
                                Class::String => "ro-string",
                                Class::Number => "ro-number",
                                Class::Keyword => "ro-keyword",
                            };
                            out.push_str(&format!(
                                "\t\t\t\ttext {} style=\"{style}\"\n",
                                kdl_escape(&visible(&rest[range]))
                            ));
                        }
                    }
                }
            }
            out.push_str("\t\t\t}\n");
        }
        out
    }

    fn notice(msg: &str) -> String {
        format!(
            "\t\t\tspacer\n\t\t\trow {{ spacer; text {} style=\"empty\"; spacer }}\n\t\t\tspacer\n",
            kdl_escape(msg)
        )
    }

    // ---- the page ------------------------------------------------------

    fn page(&self, identity: &Identity) -> Result<Vec<u8>, Status> {
        let ui = self.ui(identity);
        let metrics = rill_appkit::Metrics::from_theme_file(&rill_appkit::Metrics::theme_path());

        // The tree rides in one tight column: the rail's own gap is tuned
        // for places, and a file tree wants rows packed like every tree.
        // A scope roots it at the handed-off folder; the header names the
        // folder and offers the whole tree back.
        let mut rows = String::new();
        let tree_root_rel = ui.scope.clone().unwrap_or_default();
        self.pending_row(&tree_root_rel, 0, &ui, &mut rows);
        match ui.scope.as_deref().and_then(|rel| Some((rel, self.resolve(rel)?))) {
            Some((rel, dir)) if dir.is_dir() => {
                let name = rel.rsplit('/').next().unwrap_or(rel);
                rows.push_str(&format!(
                    "\t\t\tbutton {} icon=\"home\" style=\"tree-dir-0\" \
                     {{ submit \"/edit/actions/unscope\" }}\n",
                    kdl_escape(&format!("{name} — all files")),
                ));
                self.tree_level(&dir, rel, 0, &ui, &mut rows);
            }
            _ => self.tree_level(&self.root.clone(), "", 0, &ui, &mut rows),
        }
        let rail = format!("\t\t\tcolumn style=\"tree\" {{\n{rows}\t\t\t}}\n");

        let (mut states, body, savable, readable) = self.pane(&ui);
        states.push_str("state \"op-name\" initial=\"\"\n");

        let mut bar_controls = String::new();
        if let Some(rel) = ui.open.as_deref() {
            bar_controls.push_str(&format!(
                "\t\t\t\ttext {} style=\"filename\"\n",
                kdl_escape(rel)
            ));
        }
        bar_controls.push_str("\t\t\t\tspacer\n");
        if savable {
            let rel = ui.open.as_deref().unwrap_or_default();
            bar_controls.push_str(&rill_appkit::text_button(
                "Save",
                &rill_appkit::submit(
                    &format!("/edit/actions/save/{rel}"),
                    &format!("field \"body\" from={}", kdl_escape(&format!("body:{rel}"))),
                ),
            ));
        }
        let _ = readable;
        bar_controls.push_str(&rill_appkit::close_button());

        let titlebar = format!(
            "{}{}",
            rill_appkit::sidebar_header(&rill_appkit::location_title("Edit")),
            rill_appkit::toolbar(&bar_controls),
        );

        let kdl = rill_appkit::shell(&rill_appkit::Shell {
            metrics,
            states: &states,
            titlebar: &titlebar,
            places: &[],
            footer: None,
            sidebar_top_gap: metrics.sidebar_align_gap() as u32,
            extra_styles: &format!("{EXTRA_STYLES}{}", tree_styles()),
            content_style: None,
            body: &body,
            rail_body: Some(&rail),
            scroll_content: true,
        });
        rill_appkit::compile_page("edit-app", &kdl).inspect_err(|_| {
            if let Ok(dump) = std::env::var("EDIT_DUMP_KDL") {
                let _ = std::fs::write(dump, &kdl);
            }
        })
    }
}

impl AppHandler for Edit {
    fn get(&self, path: &str, identity: &Identity) -> Option<Vec<u8>> {
        if path == "/edit" || path == "/edit/" {
            return self.page(identity).ok();
        }
        if let Some(rest) = path.strip_prefix("/edit/at/") {
            // Land the tree on a directory: every ancestor unfolded, the
            // pane left as it was. This is the address other apps hand a
            // person — "open this folder in Edit" is a link, because apps
            // here are paths, not silos.
            let dir = self.resolve(rest)?;
            if !dir.is_dir() {
                return None;
            }
            let rel = rest.to_string();
            self.update_ui(identity, |ui| ui.scope = Some(rel));
            return self.page(identity).ok();
        }
        if let Some(rel) = path.strip_prefix("/edit/open/") {
            // Opening resolves before it sticks: a dangling target must not
            // wedge the view on a file that is not there.
            let file = self.resolve(rel)?;
            if !file.is_file() {
                return None;
            }
            let rel = rel.to_string();
            self.update_ui(identity, |ui| ui.open = Some(rel));
            return self.page(identity).ok();
        }
        None
    }

    fn action(
        &self,
        path: &str,
        fields: &[(String, ActionValue)],
        identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        if let Some(rel) = path.strip_prefix("/edit/actions/newfile/") {
            let rel = rel.to_string();
            self.update_ui(identity, |ui| ui.pending = Some(PendingOp::NewFile(rel)));
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/newdir/") {
            let rel = rel.to_string();
            self.update_ui(identity, |ui| ui.pending = Some(PendingOp::NewDir(rel)));
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/rename-target/") {
            let rel = rel.to_string();
            self.update_ui(identity, |ui| ui.pending = Some(PendingOp::Rename(rel)));
            return self.page(identity);
        }
        if path == "/edit/actions/dismiss" {
            self.update_ui(identity, |ui| ui.pending = None);
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/create/") {
            let name = clean_name(fields)?;
            let dir = self.resolve(rel).filter(|d| d.is_dir()).ok_or(Status::NotFound)?;
            let target = dir.join(&name);
            if target.exists() {
                return Err(Status::Internal);
            }
            std::fs::write(&target, "").map_err(|_| Status::Internal)?;
            let opened =
                if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            self.update_ui(identity, |ui| {
                ui.pending = None;
                ui.open = Some(opened);
            });
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/mkdir/") {
            let name = clean_name(fields)?;
            let dir = self.resolve(rel).filter(|d| d.is_dir()).ok_or(Status::NotFound)?;
            std::fs::create_dir(dir.join(&name)).map_err(|_| Status::Internal)?;
            let made = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            self.update_ui(identity, |ui| {
                ui.pending = None;
                ui.expanded.insert(made);
            });
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/rename/") {
            let name = clean_name(fields)?;
            let from = self.resolve(rel).filter(|p| p.exists()).ok_or(Status::NotFound)?;
            let to = from.with_file_name(&name);
            if to.exists() {
                // Renaming onto something that exists silently destroys it;
                // refusing is the only honest answer without a dialog.
                return Err(Status::Internal);
            }
            std::fs::rename(&from, &to).map_err(|_| Status::Internal)?;
            let new_rel = match rel.rsplit_once('/') {
                Some((parent, _)) => format!("{parent}/{name}"),
                None => name.clone(),
            };
            self.update_ui(identity, |ui| {
                ui.pending = None;
                if ui.open.as_deref() == Some(rel) {
                    ui.open = Some(new_rel);
                }
            });
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/delete/") {
            let target = self.resolve(rel).ok_or(Status::NotFound)?;
            if !target.is_file() {
                // Files only: a recursive directory delete from a context
                // menu with no confirmation is a footgun, not a feature.
                return Err(Status::NotFound);
            }
            std::fs::remove_file(&target).map_err(|_| Status::Internal)?;
            let rel = rel.to_string();
            self.update_ui(identity, |ui| {
                if ui.open.as_deref() == Some(rel.as_str()) {
                    ui.open = None;
                }
            });
            return self.page(identity);
        }
        if path == "/edit/actions/unscope" {
            self.update_ui(identity, |ui| ui.scope = None);
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/open/") {
            let file = self.resolve(rel).ok_or(Status::NotFound)?;
            if !file.is_file() {
                return Err(Status::NotFound);
            }
            let rel = rel.to_string();
            self.update_ui(identity, |ui| ui.open = Some(rel));
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/toggle/") {
            let rel = rel.to_string();
            self.update_ui(identity, |ui| {
                if !ui.expanded.remove(&rel) {
                    ui.expanded.insert(rel);
                }
            });
            return self.page(identity);
        }
        if let Some(rel) = path.strip_prefix("/edit/actions/save/") {
            let body = rill_appkit::field(fields, "body").ok_or(Status::Internal)?;
            let file = self.resolve(rel).ok_or(Status::NotFound)?;
            if !file.is_file() {
                // Save writes to files that exist: creation is a different
                // verb this app does not have yet, and a save landing on a
                // deleted file surfacing as NOT_FOUND is the honest answer.
                return Err(Status::NotFound);
            }
            std::fs::write(&file, body).map_err(|_| Status::Internal)?;
            return self.page(identity);
        }
        Err(Status::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workshop(name: &str) -> (PathBuf, Edit) {
        let root =
            std::env::temp_dir().join(format!("edit-app-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("README.md"), "hello\nworld\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        (root.clone(), Edit::new(root))
    }

    fn me() -> Identity {
        Identity::Device("laptop".into())
    }

    /// The whole loop a person actually does: open the tree, unfold a
    /// directory, open a file, change it, save, and find the change on disk.
    #[test]
    fn open_edit_save_lands_on_disk() {
        let (root, edit) = workshop("roundtrip");
        assert!(edit.get("/edit", &me()).is_some());
        edit.action("/edit/actions/toggle/src", &[], &me()).unwrap();
        assert!(edit.get("/edit/open/src/main.rs", &me()).is_some());
        edit.action(
            "/edit/actions/save/src/main.rs",
            &[("body".into(), ActionValue::Str("fn main() { println!(\"hi\") }\n".into()))],
            &me(),
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(root.join("src/main.rs")).unwrap();
        assert!(on_disk.contains("println"), "the save reached the disk");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The bug a person sees as "the contents view doesn't work": a file
    /// of any real size must arrive in the page as the editor's initial
    /// value. A 1 KiB cap on state values meant every file but a toy one
    /// failed to compile and the window served nothing at all.
    #[test]
    fn a_real_file_arrives_in_the_page() {
        let (root, edit) = workshop("contents");
        let body: String =
            (0..300).map(|i| format!("line {i} of a file worth opening\n")).collect();
        assert!(body.len() > 4096, "the fixture is past the old cap");
        std::fs::write(root.join("big.txt"), &body).unwrap();

        let bytes = edit.get("/edit/open/big.txt", &me()).expect("the page serves");
        let doc = rill_doc::decode(&bytes).expect("a valid document");
        let seeded = doc
            .states
            .iter()
            .find_map(|v| match &v.initial {
                ActionValue::Str(s) if s.contains("line 299") => Some(s.clone()),
                _ => None,
            })
            .expect("the file body is the editor's initial value");
        assert_eq!(seeded, body, "the whole file, byte for byte");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One mode: opening a file serves a `code` node — view and editor at
    /// once — with the body seeded into its bound state and the language
    /// named by extension. No Edit step, no second surface.
    #[test]
    fn a_file_opens_as_one_editable_code_surface() {
        let (root, edit) = workshop("one-mode");
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        edit.action("/edit/actions/open/src/main.rs", &[], &me()).unwrap();
        let bytes = edit.get("/edit", &me()).expect("serves");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        let code = doc
            .nodes
            .iter()
            .find_map(|n| match n {
                rill_doc::Node::Code { lang, bind, .. } => {
                    Some((doc.string(*lang).to_string(), *bind))
                }
                _ => None,
            })
            .expect("the page carries the code surface");
        assert_eq!(code.0, "rs", "the language rides as the extension");
        let seeded = doc
            .states
            .iter()
            .any(|v| matches!(&v.initial, ActionValue::Str(s) if s.contains("let x = 1")));
        assert!(seeded, "the body is the node's bound state");
        assert!(
            !doc.nodes.iter().any(|n| matches!(n, rill_doc::Node::TextInput { .. })),
            "one surface, not two"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The file-switch bug, pinned: the body slot is named per file, so
    /// the in-place carry that rightly preserves unsaved edits across a
    /// tree toggle cannot smuggle one file's buffer into another's page.
    #[test]
    fn switching_files_switches_the_state_slot() {
        let (root, edit) = workshop("switch");
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.toml"), "key = 1\n").unwrap();

        let slot_of = |bytes: &[u8]| -> String {
            let doc = rill_doc::decode(bytes).unwrap();
            doc.states
                .iter()
                .map(|v| doc.string(v.name_idx).to_string())
                .find(|n| n.starts_with("body:"))
                .expect("a per-file body slot")
        };
        edit.action("/edit/actions/open/a.rs", &[], &me()).unwrap();
        let a = slot_of(&edit.get("/edit", &me()).unwrap());
        edit.action("/edit/actions/open/b.toml", &[], &me()).unwrap();
        let b = slot_of(&edit.get("/edit", &me()).unwrap());
        assert_eq!(a, "body:a.rs");
        assert_eq!(b, "body:b.toml");
        assert_ne!(a, b, "two files, two slots — the carry cannot cross");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The address other apps hand a person: /edit/at/<dir> roots the tree
    /// at that folder — the folder is the world until unscoped.
    #[test]
    fn edit_at_scopes_the_tree_to_the_folder() {
        let (root, edit) = workshop("at-route");
        std::fs::create_dir_all(root.join("src/deep/nest")).unwrap();
        std::fs::write(root.join("src/deep/nest/x.rs"), "fn x() {}\n").unwrap();
        std::fs::write(root.join("elsewhere.txt"), "no\n").unwrap();

        let bytes = edit.get("/edit/at/src/deep", &me()).expect("lands");
        let doc = rill_doc::decode(&bytes).unwrap();
        let strings: Vec<&str> = (0..doc.strings.len() as u16).map(|i| doc.string(i)).collect();
        assert!(strings.iter().any(|s| s.contains("nest")), "the folder's children show");
        assert!(
            !strings.contains(&"elsewhere.txt"),
            "the rest of the world does not"
        );
        assert!(
            strings.iter().any(|s| s.contains("deep — all files")),
            "the header names the scope and the way out"
        );

        // Unscope: the whole tree returns.
        edit.action("/edit/actions/unscope", &[], &me()).unwrap();
        let bytes = edit.get("/edit", &me()).unwrap();
        let doc = rill_doc::decode(&bytes).unwrap();
        let strings: Vec<&str> = (0..doc.strings.len() as u16).map(|i| doc.string(i)).collect();
        assert!(strings.contains(&"elsewhere.txt"), "the world came back");

        // A file is not a place the tree can land.
        assert!(edit.get("/edit/at/src/deep/nest/x.rs", &me()).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An oversized file opens *readable*: highlighted runs, numbered,
    /// honestly labelled — no buffer, no Save. Reading is not hostage to
    /// editing's ceiling.
    #[test]
    fn an_oversized_file_opens_read_only_and_highlighted() {
        let (root, edit) = workshop("bigfile");
        let line = "// a line of commentary that repeats for size\n";
        let body: String = line.repeat(1 + MAX_EDIT_BYTES as usize / line.len());
        std::fs::write(root.join("src/big.rs"), &body).unwrap();

        edit.action("/edit/actions/open/src/big.rs", &[], &me()).unwrap();
        let bytes = edit.get("/edit", &me()).expect("serves");
        let doc = rill_doc::decode(&bytes).expect("decodes");
        assert!(
            !doc.nodes.iter().any(|n| matches!(
                n,
                rill_doc::Node::Code { .. } | rill_doc::Node::TextInput { .. }
            )),
            "read-only means no editing surface"
        );
        let strings: Vec<&str> =
            (0..doc.strings.len() as u16).map(|i| doc.string(i)).collect();
        assert!(
            strings.iter().any(|t| t.contains("Read-only")),
            "the label says what this is"
        );
        assert!(strings.contains(&"ro-comment"), "and the colours still came");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The tree's verbs, end to end: stage a new file, name it, it opens;    /// The tree's verbs, end to end: stage a new file, name it, it opens;
    /// rename moves it and follows the open file; delete takes files only.
    #[test]
    fn tree_verbs_create_rename_and_delete() {
        let (root, edit) = workshop("verbs");
        let name = |v: &str| vec![("name".to_string(), ActionValue::Str(v.into()))];

        // New file in src: staged, then named.
        edit.action("/edit/actions/newfile/src", &[], &me()).unwrap();
        edit.action("/edit/actions/create/src", &name("notes.md"), &me()).unwrap();
        assert!(root.join("src/notes.md").is_file());
        assert_eq!(edit.ui(&me()).open.as_deref(), Some("src/notes.md"), "created opens");

        // Rename follows the open file.
        edit.action("/edit/actions/rename/src/notes.md", &name("notes2.md"), &me()).unwrap();
        assert!(root.join("src/notes2.md").is_file());
        assert!(!root.join("src/notes.md").exists());
        assert_eq!(edit.ui(&me()).open.as_deref(), Some("src/notes2.md"));

        // Renaming onto an existing file refuses.
        std::fs::write(root.join("src/other.md"), "x").unwrap();
        assert!(
            edit.action("/edit/actions/rename/src/notes2.md", &name("other.md"), &me())
                .is_err(),
            "a rename must not silently destroy"
        );

        // New folder, then delete a file; directories refuse deletion.
        edit.action("/edit/actions/newdir/src", &[], &me()).unwrap();
        edit.action("/edit/actions/mkdir/src", &name("sub"), &me()).unwrap();
        assert!(root.join("src/sub").is_dir());
        edit.action("/edit/actions/delete/src/other.md", &[], &me()).unwrap();
        assert!(!root.join("src/other.md").exists());
        assert!(edit.action("/edit/actions/delete/src/sub", &[], &me()).is_err());

        // Sneaky names refuse before a path is built.
        assert!(edit.action("/edit/actions/create/src", &name("../evil"), &me()).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The wire already refuses `..`; the resolver refuses it independently,    /// The wire already refuses `..`; the resolver refuses it independently,
    /// so no caller of resolve can be talked past the root.
    #[test]
    fn the_root_is_a_wall() {
        let (root, edit) = workshop("wall");
        assert!(edit.resolve("../elsewhere").is_none());
        assert!(edit.resolve("src/../..").is_none());
        assert!(
            edit.action(
                "/edit/actions/save/../escape.txt",
                &[("body".into(), ActionValue::Str("x".into()))],
                &me(),
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Saving over a file that vanished is an honest NOT_FOUND, not a
    /// resurrection: creation is a verb this app does not have yet.
    #[test]
    fn save_does_not_create() {
        let (root, edit) = workshop("nocreate");
        let r = edit.action(
            "/edit/actions/save/ghost.txt",
            &[("body".into(), ActionValue::Str("boo".into()))],
            &me(),
        );
        assert!(r.is_err());
        assert!(!root.join("ghost.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A binary file is shown as not-editable rather than mangled: reading
    /// it must not panic and must not offer a Save.
    #[test]
    fn binary_files_are_declined_politely() {
        let (root, edit) = workshop("binary");
        std::fs::write(root.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        assert!(edit.get("/edit/open/blob.bin", &me()).is_some(), "the page still serves");
        let _ = std::fs::remove_dir_all(&root);
    }
}
