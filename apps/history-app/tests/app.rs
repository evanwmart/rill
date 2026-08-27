//! The history app against a real corpus: the pages it serves are decodable
//! documents carrying the right text, the right classification, and nothing
//! from tiers this view refuses to show.

use rill_auth::Identity;
use rill_history::crypt::Kek;
use rill_history::event::{Event, Stamped, T0_ROUTINE, T1_SENSITIVE};
use rill_history::segment::{ChunkCodec, Header, SegmentWriter};
use rill_protocol::ActionValue;
use rill_server::AppHandler;

use history_app::History;

fn text(dt: u32, tier: u8, body: &str) -> Stamped {
    Stamped { dt_ms: dt, tier, event: Event::Text { id: 1, text: body.into() } }
}

fn window(title: &str) -> Stamped {
    Stamped {
        dt_ms: 0,
        tier: T0_ROUTINE,
        event: Event::Window(rill_history::event::WindowState {
            id: 1,
            x: 0,
            y: 0,
            w: 100,
            h: 100,
            title: title.into(),
            app: "test".into(),
            vector: true,
            tier: T0_ROUTINE,
        }),
    }
}

/// A corpus of one encrypted segment, plus the identity dir that unlocks it.
fn corpus(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("history-app-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("history");
    let identity = root.join("identity");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&identity).unwrap();
    std::fs::write(identity.join("device-key.pem"), b"-----TEST DEVICE KEY-----").unwrap();
    let kek = Kek::from_identity_dir(&identity).unwrap();

    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        - 60_000;
    let header =
        Header { version: 1, device: "test".into(), wall_start_ms: wall, keyslots: Vec::new() };
    let mut w =
        SegmentWriter::create_with_key(&dir.join("a.rhs"), &header, ChunkCodec::Zstd, 3, Some(&kek))
            .unwrap();
    w.append(&window("Terminal")).unwrap();
    w.append(&text(100, T0_ROUTINE, "compiling the widget grid")).unwrap();
    w.append(&text(200, T1_SENSITIVE, "the recovery phrase words")).unwrap();
    w.append(&text(300, T0_ROUTINE, "tests passed and the build is green")).unwrap();
    w.finish().unwrap();
    (dir, identity)
}

fn texts_of(bytes: &[u8]) -> String {
    let doc = rill_doc::decode(bytes).expect("a served page decodes");
    doc.nodes
        .iter()
        .filter_map(|n| match n {
            rill_doc::Node::Text { value, .. } => Some(doc.string(*value).to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The timeline serves the routine transcript, declares itself T1 (the
/// mirror problem: a history viewer's content is transcripts, and recording
/// it at T0 would echo everything back into the index it reads), and keeps
/// sensitive tiers out.
#[test]
fn the_timeline_shows_t0_and_classifies_itself_t1() {
    let (dir, identity) = corpus("timeline");
    let app = History::new(dir, identity);
    let bytes = app.get("/history", &Identity::Anonymous).expect("page served");
    let doc = rill_doc::decode(&bytes).unwrap();

    let tier = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            rill_doc::Node::Sensitive { tier } => Some(*tier),
            _ => None,
        })
        .expect("the page classifies itself");
    assert_eq!(tier, 1, "the mirror records at T1");

    let text = texts_of(&bytes);
    assert!(text.contains("build is green"), "routine transcript shown: {text}");
    assert!(
        !text.contains("recovery phrase"),
        "sensitive text leaked into the casual view: {text}"
    );
}

/// Search is an action that serves a results page.
#[test]
fn search_returns_hits_as_a_page() {
    let (dir, identity) = corpus("search");
    let app = History::new(dir, identity);
    let fields = vec![("q".to_string(), ActionValue::Str("widget".into()))];
    let bytes = app
        .action("/history/actions/search", &fields, &Identity::Anonymous)
        .expect("search serves");
    let text = texts_of(&bytes);
    assert!(text.contains("1 hit"), "counted: {text}");
    assert!(text.contains("compiling the widget grid"), "the hit is shown: {text}");

    let none = vec![("q".to_string(), ActionValue::Str("zebra".into()))];
    let bytes = app.action("/history/actions/search", &none, &Identity::Anonymous).unwrap();
    assert!(texts_of(&bytes).contains("0 hit"), "an honest miss");
}

/// The agent surface, both wearings: the context page decodes like any
/// document, and /history/data is the same tail with no drawing at all —
/// the first tool call an agent makes.
#[test]
fn the_agent_reads_context_and_raw_data() {
    let (dir, identity) = corpus("agent");
    let app = History::new(dir, identity);

    let page = app.get("/history/context", &Identity::Anonymous).expect("context serves");
    let text = texts_of(&page);
    assert!(text.contains("build is green"));
    assert!(!text.contains("recovery phrase"));

    let data = app.get("/history/data", &Identity::Anonymous).expect("data serves");
    let data = String::from_utf8(data).unwrap();
    assert!(data.contains("compiling the widget grid"), "raw tail: {data}");
    assert!(data.contains('\t'), "tab-separated, trivially parseable");
    assert!(!data.contains("recovery phrase"), "tiering holds on the raw path too");
}

/// The live-poll escape hatch: the revision holds still while the corpus
/// does, and moves when it grows — a 0.5 Hz widget must not rebuild the
/// page to learn nothing changed.
#[test]
fn the_revision_moves_only_with_the_corpus() {
    let (dir, identity) = corpus("rev");
    let app = History::new(dir.clone(), identity.clone());
    let r1 = app.revision("/history", &Identity::Anonymous).expect("stamped");
    let r2 = app.revision("/history", &Identity::Anonymous).expect("stamped");
    assert_eq!(r1, r2, "an unchanged corpus holds its revision");

    // Another segment lands.
    let kek = Kek::from_identity_dir(&identity).unwrap();
    let header =
        Header { version: 1, device: "test".into(), wall_start_ms: 1, keyslots: Vec::new() };
    let mut w =
        SegmentWriter::create_with_key(&dir.join("b.rhs"), &header, ChunkCodec::Zstd, 3, Some(&kek))
            .unwrap();
    w.append(&text(1, T0_ROUTINE, "fresh")).unwrap();
    w.finish().unwrap();
    let r3 = app.revision("/history", &Identity::Anonymous).expect("stamped");
    assert_ne!(r2, r3, "a grown corpus moves the revision");
}

/// The mirror must not excite itself: the page is a pure function of what
/// the corpus *shows at T0* plus sealed-segment stats — so its own recorded
/// reflection (T1 events landing in the open segment) changes nothing, the
/// live tick answers NOT_MODIFIED, and an idle desktop with History open
/// records nothing at all.
///
/// The first version failed this three ways at once: "55s ago" stamps aged
/// every refresh, the stats line counted the page's own frames as they
/// landed, and each refresh therefore committed a fresh frame — measured at
/// 104 KiB/minute of an idle desktop recording its own reflection.
#[test]
fn the_mirror_does_not_excite_itself() {
    let (dir, identity) = corpus("stable");
    let app = History::new(dir.clone(), identity.clone());
    let first = app.get("/history", &Identity::Anonymous).expect("page");
    let second = app.get("/history", &Identity::Anonymous).expect("page");
    assert_eq!(first, second, "an unchanged corpus must serve identical bytes");

    // The reflection lands: an OPEN segment carrying only T1 events — the
    // shape of the History window's own recording arriving live.
    let kek = Kek::from_identity_dir(&identity).unwrap();
    let header =
        Header { version: 1, device: "test".into(), wall_start_ms: 2, keyslots: Vec::new() };
    let mut w = SegmentWriter::create_with_key(
        &dir.join("open.rhs"),
        &header,
        ChunkCodec::Zstd,
        3,
        Some(&kek),
    )
    .unwrap();
    w.append(&text(1, T1_SENSITIVE, "History 1 sealed segment(s) reflection")).unwrap();
    w.flush().unwrap();
    drop(w); // open, unsealed: exactly what a live writer's segment is

    let third = app.get("/history", &Identity::Anonymous).expect("page");
    assert_eq!(
        first, third,
        "the page changed because the page was shown — the feedback loop is back"
    );
}
