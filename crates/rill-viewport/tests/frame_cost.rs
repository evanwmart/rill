//! What a frame costs as a document gets longer than its window.
//!
//! Run with `cargo test -p rill-viewport --test frame_cost -- --ignored --nocapture`.
//!
//! The question behind it: nothing in the pipeline culls to the visible
//! region, so a frame carries the whole document rather than the part on
//! screen. That is free for a page that fits and unbounded for one that does
//! not — and the stream has a hard cap (`MAX_STREAM_SIZE`, 4 MiB) which a
//! long enough document would reach, at which point the window cannot be
//! displayed at all rather than merely being expensive.

use rill_ui::{DrawCommand, LineMetrics, Rect, TextMeasurer};
use rill_viewport::{AppView, Fetcher, Source};

const VIEWPORT: Rect = Rect { x: 0.0, y: 0.0, w: 1280.0, h: 800.0 };

struct FixedMeasurer;

impl TextMeasurer for FixedMeasurer {
    fn measure(
        &mut self,
        text: &str,
        font_size: f32,
        _weight: u16,
        _family: &str,
        _max_width: f32,
    ) -> LineMetrics {
        LineMetrics { width: text.chars().count() as f32 * font_size * 0.6, height: font_size * 1.4 }
    }
}

/// A chat log or a terminal transcript: one text run per line.
fn log(rows: usize) -> String {
    let mut s = String::from("column gap=2 padding=8 {\n");
    for i in 0..rows {
        s.push_str(&format!(
            "\ttext \"{i:05}  2026-08-20T11:42:{:02}Z  connection established, 14 frames\"\n",
            i % 60
        ));
    }
    s.push_str("}\n");
    s
}

/// A file listing: the shape files-app actually emits — a row per entry with
/// an icon and several cells.
fn listing(rows: usize) -> String {
    let mut s = String::from(
        "style \"cell\" size=13\nstyle \"dim\" size=12 color=\"#8a8a99\"\n\
         column gap=1 padding=8 {\n",
    );
    for i in 0..rows {
        s.push_str(&format!(
            "\trow gap=8 {{ icon \"file\" size=16; text \"document-{i:04}.txt\" style=\"cell\"; \
             spacer; text \"12.4 KB\" style=\"dim\"; text \"2026-08-19 14:02\" style=\"dim\" }}\n"
        ));
    }
    s.push_str("}\n");
    s
}

fn frame(source: &str) -> (Result<usize, String>, usize, usize) {
    let dir = std::env::temp_dir().join(format!("frame-cost-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let bytes = rill_doc::compile(source).expect("compiles").bytes;
    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view = AppView::new(fetcher, Source::Generated { label: "m".into(), bytes });
    for _ in 0..2000 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(!view.is_loading(), "document never finished loading");
    let (cmds, _i, _h) = view.layout(VIEWPORT, &mut FixedMeasurer);
    // A frame that will not encode is a window that cannot be displayed, so
    // the failure is a result to report rather than a panic.
    let encoded = rill_ui::stream::encode(&cmds);

    // How much of what was emitted could possibly be seen. Only commands
    // that carry a rect are counted, so the ratio compares like with like.
    let placed: Vec<Rect> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCommand::Text { rect, .. }
            | DrawCommand::Rect { rect, .. }
            | DrawCommand::Image { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect();
    let onscreen = placed.len();
    let visible =
        placed.iter().filter(|r| r.y + r.h >= 0.0 && r.y <= VIEWPORT.h).count();
    let _ = std::fs::remove_dir_all(&dir);
    (encoded.map(|b| b.len()).map_err(|e| e.to_string()), onscreen, visible)
}

fn kib(n: usize) -> String {
    format!("{:.1} KiB", n as f64 / 1024.0)
}

#[test]
#[ignore]
fn what_a_long_document_costs_per_frame() {
    println!("\nViewport {}x{}. Nothing culls to it.", VIEWPORT.w, VIEWPORT.h);

    for (name, build) in
        [("log lines", log as fn(usize) -> String), ("file rows", listing as fn(usize) -> String)]
    {
        println!("\n=== {name} ===");
        println!("  {:>8}  {:>12}  {:>12}  {:>10}", "rows", "frame", "per row", "on screen");
        let mut per_row = 0.0f64;
        for rows in [10usize, 100, 1_000, 10_000] {
            let (bytes, drawn, visible) = frame(&build(rows));
            let seen = 100.0 * visible as f64 / drawn.max(1) as f64;
            match bytes {
                Ok(n) => {
                    per_row = n as f64 / rows as f64;
                    println!(
                        "  {rows:>8}  {:>12}  {:>12}  {seen:>9.1}%",
                        kib(n),
                        format!("{per_row:.0} B")
                    );
                }
                Err(e) => println!("  {rows:>8}  {:>12}  {:>12}  {seen:>9.1}%   <- {e}", "—", "—"),
            }
        }
        let cap = rill_ui::stream::MAX_STREAM_SIZE;
        println!(
            "  size cap ({}) reached at ~{:.0} rows, if nothing else gives out first",
            kib(cap),
            cap as f64 / per_row
        );
    }
    println!();
}

/// A frame costs what the window shows, not what the document holds.
///
/// The property the whole command-stream argument rests on, and it was not
/// true: a frame described the entire document, so a long one grew linearly
/// until it stopped encoding. A ten-thousand-row listing exceeded the frame's
/// path-point budget — the window could not be drawn at all.
#[test]
fn frame_cost_does_not_grow_with_the_document() {
    for (name, build) in
        [("log", log as fn(usize) -> String), ("listing", listing as fn(usize) -> String)]
    {
        let (short, _, _) = frame(&build(100));
        let (long, _, _) = frame(&build(10_000));
        let short = short.unwrap_or_else(|e| panic!("{name}: 100 rows must encode: {e}"));
        let long = long.unwrap_or_else(|e| panic!("{name}: 10000 rows must encode: {e}"));

        // A hundred times the rows must not be meaningfully more bytes. Not
        // asserted equal: the visible band shifts slightly with content, and
        // the point is the absence of growth, not a fixed number.
        assert!(
            long <= short * 2,
            "{name}: 10000 rows encodes to {} against {} for 100 — the frame is \
             still carrying the document rather than the view",
            kib(long),
            kib(short)
        );
    }
}

/// Culling must not reach past the window and take something on screen.
#[test]
fn what_the_window_shows_survives_culling() {
    // Short enough to fit entirely, so nothing is off-screen to drop.
    let (bytes, drawn, visible) = frame(&listing(10));
    assert!(bytes.is_ok(), "a page that fits must encode");
    assert_eq!(drawn, visible, "a page that fits had commands culled from it");
    assert!(drawn > 10, "the listing drew almost nothing: {drawn} commands");
}
