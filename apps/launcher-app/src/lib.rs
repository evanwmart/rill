//! The app menu, grown up (the arc's item 5): an icon grid of the published
//! applications, grouped by category, with a search field — all server-side,
//! because the launcher is a document and filtering a document is what a
//! server is for. Launching stays the host's verb: a tile's target is
//! `/~launch/<app_id>`, which the client resolves against its *own* installs
//! — the server proposes, the device disposes.

use std::path::PathBuf;
use std::sync::Mutex;

use rill_auth::Identity;
use rill_doc::kdl_escape;
use rill_protocol::{ActionValue, Status};
use rill_server::AppHandler;

/// One published application, as its manifest tells it.
#[derive(Clone)]
struct Entry {
    app_id: String,
    name: String,
    /// A glyph name from the icon set; manifests may declare `icon = "…"`.
    icon: String,
    /// Freeform grouping; manifests may declare `category = "…"`.
    category: String,
}

pub struct Launcher {
    /// The served content root — the same `apps/<id>/manifest` layout the
    /// installer reads.
    content: PathBuf,
    /// The active search, per device asking.
    query: Mutex<std::collections::HashMap<String, String>>,
}

fn who(identity: &Identity) -> String {
    match identity {
        Identity::Device(name) => name.clone(),
        Identity::Anonymous => String::new(),
    }
}

/// A sensible default glyph per app id, for manifests that name none —
/// the shipped apps get their own faces without every manifest needing
/// editing on day one.
fn default_icon(app_id: &str) -> &'static str {
    match app_id {
        "files" => "folder-fill",
        "term" => "list",
        "edit" => "pencil",
        "studio" => "star-fill",
        "history" => "clock-fill",
        "music" => "music-fill",
        "launcher" => "grid",
        _ => "world",
    }
}

impl Launcher {
    pub fn new(content: PathBuf) -> Launcher {
        Launcher { content, query: Mutex::new(Default::default()) }
    }

    fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        let Ok(read) = std::fs::read_dir(self.content.join("apps")) else { return out };
        for dir in read.flatten() {
            let Ok(text) = std::fs::read_to_string(dir.path().join("manifest")) else {
                continue;
            };
            let Ok(table) = text.parse::<toml::Table>() else { continue };
            let get = |k: &str| table.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let (Some(app_id), Some(name)) = (get("app_id"), get("name")) else { continue };
            let icon = get("icon").unwrap_or_else(|| default_icon(&app_id).to_string());
            let category = get("category").unwrap_or_else(|| "Apps".to_string());
            // The launcher does not list itself: a portal to the portal is
            // a hall of mirrors.
            if app_id == "launcher" {
                continue;
            }
            out.push(Entry { app_id, name, icon, category });
        }
        out.sort_by_key(|e| (e.category.clone(), e.name.clone()));
        out
    }

    fn page(&self, identity: &Identity) -> Result<Vec<u8>, Status> {
        let m = rill_appkit::Metrics::from_theme_file(&rill_appkit::Metrics::theme_path());
        let query = self
            .query
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&who(identity))
            .cloned()
            .unwrap_or_default();
        let needle = query.to_lowercase();
        let entries: Vec<Entry> = self
            .entries()
            .into_iter()
            .filter(|e| {
                needle.is_empty()
                    || e.name.to_lowercase().contains(&needle)
                    || e.app_id.to_lowercase().contains(&needle)
            })
            .collect();

        let f = m.font_size;
        let mut body = String::new();
        if entries.is_empty() {
            body.push_str(
                "\t\t\tspacer\n\t\t\trow { spacer; text \"Nothing matches.\" \
                 style=\"quiet\"; spacer }\n\t\t\tspacer\n",
            );
        }
        let mut current_cat = String::new();
        for e in &entries {
            if e.category != current_cat {
                if !current_cat.is_empty() {
                    body.push_str("\t\t\t}\n");
                }
                current_cat = e.category.clone();
                body.push_str(&format!(
                    "\t\t\ttext {} style=\"cat\"\n\t\t\trow style=\"tiles\" {{\n",
                    kdl_escape(&current_cat.to_uppercase()),
                ));
            }
            // The whole tile is the control, look-card style: icon large,
            // name beneath, launch as the target.
            body.push_str(&format!(
                "\t\t\t\tcolumn style=\"tile\" target={} {{\n\
                 \t\t\t\t\trow gap=0 padding=0 {{ spacer; icon {} style=\"tile-ico\"; spacer }}\n\
                 \t\t\t\t\trow gap=0 padding=0 {{ spacer; text {} style=\"tile-name\"; spacer }}\n\
                 \t\t\t\t}}\n",
                kdl_escape(&format!("/~launch/{}", e.app_id)),
                kdl_escape(&e.icon),
                kdl_escape(&e.name),
            ));
        }
        if !current_cat.is_empty() {
            body.push_str("\t\t\t}\n");
        }

        let titlebar = format!(
            "{}{}",
            rill_appkit::sidebar_header(&rill_appkit::location_title("Apps")),
            rill_appkit::toolbar(&format!(
                "{}{}",
                rill_appkit::search_field(
                    "q",
                    "Search apps…",
                    "submit \"/launcher/actions/search\" { field \"q\" from=\"q\" }",
                ),
                rill_appkit::close_button(),
            )),
        );
        let extra = format!(
            " style \"cat\" color=\"text-muted\" size={} weight=\"bold\"\n\
             style \"tiles\" wrap=#true padding=0 gap={g}\n\
             style \"tile\" background=\"surface-raised\" corner=6 padding={p} gap=6 \
             width={tw} hover=\"tile--hover\"\n\
             style \"tile--hover\" background=\"elevation-lg\" corner=6 padding={p} gap=6 width={tw}\n\
             style \"tile-ico\" color=\"accent\" size={ico}\n\
             style \"tile-name\" color=\"text\" size={f}\n\
             style \"quiet\" color=\"text-muted\" size={f}\n",
            f - 2.0,
            g = m.padding,
            p = m.padding * 1.5,
            tw = (m.control_height() * 3.4).round(),
            ico = (f * 2.2).round(),
        );
        let kdl = rill_appkit::shell(&rill_appkit::Shell {
            metrics: m,
            states: &format!("state \"q\" initial={}\n", kdl_escape(&query)),
            titlebar: &titlebar,
            places: &[],
            footer: None,
            sidebar_top_gap: 0,
            extra_styles: &extra,
            content_style: None,
            body: &body,
            rail_body: None,
            scroll_content: true,
        });
        rill_appkit::compile_page("launcher-app", &kdl)
    }
}

impl AppHandler for Launcher {
    fn get(&self, path: &str, identity: &Identity) -> Option<Vec<u8>> {
        match path {
            "/launcher" | "/launcher/" => self.page(identity).ok(),
            _ => None,
        }
    }

    fn action(
        &self,
        path: &str,
        fields: &[(String, ActionValue)],
        identity: &Identity,
    ) -> Result<Vec<u8>, Status> {
        if path != "/launcher/actions/search" {
            return Err(Status::NotFound);
        }
        let q = rill_appkit::field(fields, "q").unwrap_or_default().trim().to_string();
        self.query
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(who(identity), q);
        self.page(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> (PathBuf, Launcher) {
        let root =
            std::env::temp_dir().join(format!("launcher-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (id, nm, cat) in
            [("term", "Terminal", ""), ("edit", "Edit", ""), ("studio", "Studio", "Ricing")]
        {
            let dir = root.join("apps").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let cat_line =
                if cat.is_empty() { String::new() } else { format!("category = \"{cat}\"\n") };
            std::fs::write(
                dir.join("manifest"),
                format!(
                    "manifest_version = 1\napp_id = \"{id}\"\nname = \"{nm}\"\n\
                     entry = \"/x\"\npack = \"/apps/{id}/app.rillpack\"\n{cat_line}"
                ),
            )
            .unwrap();
        }
        (root.clone(), Launcher::new(root))
    }

    fn strings(bytes: &[u8]) -> Vec<String> {
        let doc = rill_doc::decode(bytes).unwrap();
        (0..doc.strings.len() as u16).map(|i| doc.string(i).to_string()).collect()
    }

    /// The grid: every published app as a tile whose target is the host's
    /// launch verb by app id, grouped under its category.
    #[test]
    fn the_grid_lists_apps_under_their_categories() {
        let (root, l) = setup("grid");
        let s = strings(&l.get("/launcher", &Identity::Anonymous).expect("serves"));
        assert!(s.contains(&"/~launch/term".to_string()));
        assert!(s.contains(&"/~launch/studio".to_string()));
        assert!(s.contains(&"APPS".to_string()), "the default category heads the group");
        assert!(s.contains(&"RICING".to_string()), "a declared category is its own group");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Search narrows to matches and is itself stateful per device; an
    /// empty query brings everything back.
    #[test]
    fn search_filters_and_clears() {
        let (root, l) = setup("search");
        let dev = Identity::Device("laptop".into());
        let q = |v: &str| vec![("q".to_string(), ActionValue::Str(v.into()))];
        let s = strings(&l.action("/launcher/actions/search", &q("term"), &dev).unwrap());
        assert!(s.contains(&"/~launch/term".to_string()));
        assert!(!s.contains(&"/~launch/edit".to_string()), "filtered out");
        let s = strings(&l.action("/launcher/actions/search", &q(""), &dev).unwrap());
        assert!(s.contains(&"/~launch/edit".to_string()), "cleared");
        let _ = std::fs::remove_dir_all(&root);
    }
}
