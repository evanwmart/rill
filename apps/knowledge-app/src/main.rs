//! knowledge-app: the search page over a knowledge pack
//! (`specs/knowledge.md` §1.8, §10). A Tier-0 document app: the display
//! GETs a page, submits the search field as an ACTION, and renders the
//! results document. The same results exist at `/knowledge/q/<query>` as a
//! GET resource, every chunk at `/knowledge/c/<id>`, and the pack's own
//! files (`manifest`, `text/…`, `doc/…`, indexes) under `/knowledge/` too,
//! so the tree a query reads is the tree a client can fetch.
//!
//! `knowledge-app <pack-dir> --identity <dir> [--model <dir> [--embed gpu|cpu]] [--bind ADDR] [--port N]`
//!
//! With `--model`, queries are embedded and the semantic run joins the
//! fusion; the model's hash must match the pack's manifest or the app
//! says so and answers lexically.

use std::sync::Arc;

use rill_appkit::{Metrics, Shell, kdl_escape, shell};
use rill_knowledge::query::{Engine, Hit, Mode, Results};
use rill_knowledge_embed::{Embedder, QUERY_PREFIX};
use rill_knowledge::text::Chunk;
use rill_protocol::{ActionValue, Status};
use rill_auth::Identity;
use rill_server::{AppHandler, Server, ServerConfig};

const APP: &str = "knowledge";
const RESULTS: usize = 10;
const SNIPPET_CHARS: usize = 280;
/// Longest query accepted: the ACTION field cap is 1024 bytes and a path
/// segment has to hold it too.
const MAX_QUERY_BYTES: usize = 512;

struct KnowledgeApp {
    engine: Engine,
    pack_dir: std::path::PathBuf,
    embedder: Option<Box<dyn Embedder>>,
}

impl KnowledgeApp {

    fn semantic_status(&self) -> String {
        match (self.engine.semantic_available(), &self.embedder) {
            (true, Some(_)) => "on".into(),
            (true, None) => "unavailable (no query embedder loaded)".into(),
            (false, _) => "unavailable (no vectors in this pack)".into(),
        }
    }
}

impl KnowledgeApp {
    fn page(&self, query: &str, body: &str) -> Result<Vec<u8>, Status> {
        let metrics = Metrics::from_theme_file(&Metrics::theme_path());
        let states = format!("state \"q\" initial={}\n", kdl_escape(query));
        let search = rill_appkit::search_field("q", "Search Simple English Wikipedia…", &rill_appkit::submit("/knowledge/actions/search", "field \"q\" from=\"q\""));
        let titlebar = rill_appkit::sidebar_header(&(rill_appkit::icon_slot("book-fill", &rill_appkit::navigate("/knowledge")) + &rill_appkit::location_title("Knowledge"))) + &rill_appkit::toolbar(&search);
        let extra = "style \"hit-title\" size=17 weight=\"bold\"\nstyle \"snippet\" size=14\nstyle \"chunk-text\" size=15\n";
        let kdl = shell(&Shell {
            metrics,
            states: &states,
            titlebar: &titlebar,
            places: &[],
            footer: None,
            sidebar_top_gap: 0,
            extra_styles: extra,
            content_style: None,
            body,
            rail_body: None,
            scroll_content: true,
        });
        rill_appkit::compile_page(APP, &kdl)
    }

    fn home(&self) -> Result<Vec<u8>, Status> {
        let m = self.engine.pack().manifest();
        let mut body = String::new();
        body.push_str("\t\t\tcolumn gap=10 padding=8 {\n");
        body.push_str("\t\t\t\ttext \"Search a knowledge pack\" style=\"title\"\n");
        body.push_str(&format!(
            "\t\t\t\ttext {} style=\"muted\"\n",
            kdl_escape(&format!(
                "{} chunks from {} articles of {}, stage {}. Lexical search {}; semantic search {}.",
                self.engine.pack().chunk_count(),
                self.engine.pack().doc_count(),
                m.get("source.wiki").unwrap_or("?"),
                m.get("stage").unwrap_or("?"),
                if self.engine.lexical_available() { "on" } else { "off (no postings)" },
                self.semantic_status()
            ))
        ));
        body.push_str("\t\t\t\ttext \"Type in the field above and press Enter. A result opens the chunk; every result page is also a link.\" style=\"muted\"\n");
        body.push_str("\t\t\t}\n");
        self.page("", &body)
    }

    fn results_page(&self, query: &str) -> Result<Vec<u8>, Status> {
        let query = query.trim();
        if query.is_empty() {
            return self.home();
        }
        if query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
            return Err(Status::NotFound);
        }
        let qvec = self.embedder.as_ref().filter(|_| self.engine.semantic_available()).map(|e| e.embed(&[&format!("{QUERY_PREFIX}{query}")]).remove(0));
        let mode = if qvec.is_some() { Mode::Fused } else { Mode::Lexical };
        let res = self.engine.search_with(query, RESULTS, qvec.as_deref(), mode).map_err(|_| Status::Internal)?;
        let mut body = String::new();
        body.push_str("\t\t\tcolumn gap=10 padding=8 {\n");
        body.push_str(&format!(
            "\t\t\t\trow gap=12 {{ text {} style=\"muted\"; spacer; link \"permalink\" target={} style=\"muted\" }}\n",
            kdl_escape(&summary(&res, query)),
            kdl_escape(&format!("/knowledge/q/{query}")),
        ));
        if res.hits.is_empty() {
            body.push_str("\t\t\t\ttext \"Nothing matched. Fewer or different words usually help; the index is exact terms, not meanings, until the vectors land.\" style=\"muted\"\n");
        }
        for h in &res.hits {
            body.push_str(&self.hit_card(h, &res));
        }
        body.push_str("\t\t\t}\n");
        self.page(query, &body)
    }

    fn hit_card(&self, h: &Hit, res: &Results) -> String {
        let (title, section, snippet) = match (self.engine.doc(h.doc), self.engine.chunk(h.chunk)) {
            (Some(d), Ok(c)) => (d.title.clone(), c.section.clone(), snippet_of(&c.text)),
            (Some(d), Err(_)) => (d.title.clone(), String::new(), String::from("(chunk unreadable)")),
            _ => (format!("chunk {:#x}", h.chunk), String::new(), String::new()),
        };
        let mut why = Vec::new();
        if let Some(r) = h.lexical_rank {
            why.push(format!("terms {}/{} · lexical #{r}", h.matched, res.terms.len()));
        }
        if let Some(r) = h.entity_rank {
            why.push(format!("entity #{r}"));
        }
        if let (Some(r), Some(cos)) = (h.semantic_rank, h.cosine) {
            why.push(format!("semantic #{r} (cos {cos:.3})"));
        }
        if h.title_match {
            why.push("title".into());
        }
        why.push(format!("score {:.4}", h.score));
        let head = if section.is_empty() { title.clone() } else { format!("{title} — {section}") };
        format!(
            "\t\t\t\tcolumn gap=4 padding=12 style=\"card\" {{\n\
             \t\t\t\t\tlink {} target={} style=\"hit-title\"\n\
             \t\t\t\t\ttext {} style=\"snippet\"\n\
             \t\t\t\t\ttext {} style=\"muted\"\n\
             \t\t\t\t}}\n",
            kdl_escape(&head),
            kdl_escape(&format!("/knowledge/c/{:x}", h.chunk)),
            kdl_escape(&snippet),
            kdl_escape(&why.join(" · ")),
        )
    }

    fn chunk_page(&self, id: u64) -> Result<Vec<u8>, Status> {
        let c: Chunk = self.engine.chunk(id).map_err(|_| Status::NotFound)?;
        let d = self.engine.doc(c.doc).ok_or(Status::NotFound)?;
        let mut body = String::new();
        body.push_str("\t\t\tcolumn gap=10 padding=8 {\n");
        body.push_str(&format!("\t\t\t\ttext {} style=\"title\"\n", kdl_escape(&d.title)));
        let mut meta = Vec::new();
        if !c.section.is_empty() {
            meta.push(c.section.clone());
        }
        meta.push(format!("chunk {:#x} of {} in this article", id, d.chunk_count));
        meta.push(format!("page {}", d.page_id));
        if let Some(q) = &d.qid {
            meta.push(q.clone());
        }
        meta.push(format!("simple.wikipedia.org/wiki/{}", d.title.replace(' ', "_")));
        body.push_str(&format!("\t\t\t\ttext {} style=\"muted\"\n", kdl_escape(&meta.join(" · "))));
        for para in c.text.split('\n').filter(|p| !p.trim().is_empty()) {
            body.push_str(&format!("\t\t\t\ttext {} style=\"chunk-text\"\n", kdl_escape(para)));
        }
        body.push_str("\t\t\t\trow gap=16 {\n");
        if id > d.first_chunk {
            body.push_str(&format!("\t\t\t\t\tlink \"← previous chunk\" target={} style=\"muted\"\n", kdl_escape(&format!("/knowledge/c/{:x}", id - 1))));
        }
        if id + 1 < d.first_chunk + u64::from(d.chunk_count) {
            body.push_str(&format!("\t\t\t\t\tlink \"next chunk →\" target={} style=\"muted\"\n", kdl_escape(&format!("/knowledge/c/{:x}", id + 1))));
        }
        body.push_str("\t\t\t\t\tspacer\n\t\t\t\t\tlink \"search\" target=\"/knowledge\" style=\"muted\"\n\t\t\t\t}\n");
        body.push_str("\t\t\t}\n");
        self.page("", &body)
    }

    /// The pack's own resources, served as themselves: the same bytes the
    /// engine seeks into. Only the layout's known top-level names, only
    /// files, only plain segment characters.
    fn pack_file(&self, rest: &str) -> Option<Vec<u8>> {
        let mut segs = rest.split('/');
        let top = segs.next()?;
        let known = matches!(top, "manifest" | "text" | "doc" | "vector" | "semantic" | "lexical" | "entity");
        if !known || !rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-')) || rest.contains("..") {
            return None;
        }
        let path = self.pack_dir.join("knowledge").join(rest);
        if !path.is_file() {
            return None;
        }
        std::fs::read(path).ok()
    }
}

fn summary(res: &Results, query: &str) -> String {
    let mut s = format!("{} result{} for “{}” in {:.1} ms", res.hits.len(), if res.hits.len() == 1 { "" } else { "s" }, query, res.elapsed_ms);
    if res.any_term {
        s.push_str(" · no chunk had every term, showing partial matches");
    }
    if !res.skipped_terms.is_empty() {
        s.push_str(&format!(" · ignored: {}", res.skipped_terms.join(", ")));
    }
    if !res.semantic_available {
        s.push_str(" · lexical only");
    } else {
        s.push_str(&format!(" · fused lexical + semantic ({} vector candidates)", res.semantic_candidates));
    }
    s
}

fn snippet_of(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SNIPPET_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(SNIPPET_CHARS).collect();
    if let Some(sp) = cut.rfind(' ') {
        cut.truncate(sp);
    }
    cut.push('…');
    cut
}

impl AppHandler for KnowledgeApp {
    fn get(&self, path: &str, _identity: &Identity) -> Option<Vec<u8>> {
        match path {
            "/knowledge" | "/knowledge/" => self.home().ok(),
            _ => {
                let rest = path.strip_prefix("/knowledge/")?;
                if let Some(q) = rest.strip_prefix("q/") {
                    return self.results_page(q).ok();
                }
                if let Some(hex) = rest.strip_prefix("c/") {
                    return u64::from_str_radix(hex, 16).ok().and_then(|id| self.chunk_page(id).ok());
                }
                self.pack_file(rest)
            }
        }
    }

    fn revision(&self, path: &str, _identity: &Identity) -> Option<u64> {
        // The pack is immutable, so a pack file never changes; pages read
        // the theme too, so they stay unstamped.
        let rest = path.strip_prefix("/knowledge/")?;
        (!rest.starts_with("q/") && !rest.starts_with("c/") && !rest.is_empty()).then_some(1)
    }

    fn action(&self, path: &str, fields: &[(String, ActionValue)], _identity: &Identity) -> Result<Vec<u8>, Status> {
        if path != "/knowledge/actions/search" {
            return Err(Status::NotFound);
        }
        let q = rill_appkit::field(fields, "q").unwrap_or_default();
        self.results_page(q)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pack_dir: Option<String> = None;
    let mut identity: Option<String> = None;
    let mut model: Option<String> = None;
    let mut embed_backend = "cpu".to_string();
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 7450;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--identity" => {
                identity = args.get(i + 1).cloned();
                i += 2;
            }
            "--model" => {
                model = args.get(i + 1).cloned();
                i += 2;
            }
            "--embed" => {
                embed_backend = args.get(i + 1).cloned().unwrap_or(embed_backend);
                i += 2;
            }
            "--bind" => {
                bind = args.get(i + 1).cloned().unwrap_or(bind);
                i += 2;
            }
            "--port" => {
                port = args.get(i + 1).and_then(|p| p.parse().ok()).unwrap_or(port);
                i += 2;
            }
            other if pack_dir.is_none() => {
                pack_dir = Some(other.to_string());
                i += 1;
            }
            other => {
                eprintln!("knowledge-app: unexpected argument {other}");
                std::process::exit(2);
            }
        }
    }
    let (Some(pack_dir), Some(identity)) = (pack_dir, identity) else {
        eprintln!("usage: knowledge-app <pack-dir> --identity <dir> [--model <dir> [--embed gpu|cpu]] [--bind ADDR] [--port N]");
        std::process::exit(2);
    };
    let pack_dir = std::path::PathBuf::from(pack_dir);
    let engine = Engine::open(pack_dir.join("knowledge")).unwrap_or_else(|e| {
        eprintln!("knowledge-app: opening pack {}: {e}", pack_dir.display());
        std::process::exit(1)
    });
    let embedder: Option<Box<dyn Embedder>> = model.and_then(|dir| {
        let t = std::time::Instant::now();
        match rill_knowledge_embed::open(std::path::Path::new(&dir), &embed_backend) {
            Ok(e) => {
                let want = engine.pack().manifest().get("embedding.model_hash").unwrap_or("");
                if !engine.semantic_available() {
                    eprintln!("knowledge-app: model loaded but the pack has no vectors; answering lexically");
                    None
                } else if e.model_hash().to_string() != want {
                    eprintln!("knowledge-app: model {} does not match the pack's {want}; answering lexically", e.model_hash());
                    None
                } else {
                    eprintln!("knowledge-app: query embedder {} on {embed_backend} in {:?}", e.model_name(), t.elapsed());
                    Some(e)
                }
            }
            Err(err) => {
                eprintln!("knowledge-app: model {dir}: {err}; answering lexically");
                None
            }
        }
    });
    eprintln!(
        "knowledge-app: pack {} — {} chunks, {} documents, stage {}, lexical {}, semantic {}",
        pack_dir.display(),
        engine.pack().chunk_count(),
        engine.pack().doc_count(),
        engine.pack().manifest().get("stage").unwrap_or("?"),
        engine.lexical_available(),
        embedder.is_some() && engine.semantic_available()
    );
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    runtime.block_on(async move {
        let cfg = ServerConfig::new(pack_dir.clone(), std::path::PathBuf::from(identity));
        let mut server = Server::bind(&bind, port, cfg).await.expect("bind");
        server.dynamic("/knowledge", Arc::new(KnowledgeApp { engine, pack_dir, embedder }));
        server.run().await.expect("run");
    });
}
