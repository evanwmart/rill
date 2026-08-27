//! Text selection over a document's painted text — the piece the terminal
//! grid was missing, built where every app inherits it: drag over anything
//! the page drew, the highlight follows, Ctrl+Shift+C (the host's half)
//! copies exactly the glyphs the drag covered, in reading order.

use rill_ui::{LineMetrics, Rect, TextMeasurer};
use rill_viewport::{AppView, Fetcher, Source};

/// 6px per char, 14px lines — a deterministic monospace stand-in.
struct MonoMeasurer;

impl TextMeasurer for MonoMeasurer {
    fn measure(
        &mut self,
        text: &str,
        _size: f32,
        _weight: u16,
        _family: &str,
        _max_width: f32,
    ) -> LineMetrics {
        LineMetrics { width: text.chars().count() as f32 * 6.0, height: 14.0 }
    }
}

/// A terminal-shaped page: rows of runs, zero padding, like the term grid.
fn view() -> AppView {
    let dir = std::env::temp_dir().join(format!("viewport-sel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let src = r##"
column gap=0 padding=0 {
    row gap=0 padding=0 { text "alpha beta gamma" }
    row gap=0 padding=0 { text "delta "; text "epsilon zeta" }
    row gap=0 padding=0 { text "eta theta iota   " }
}
"##;
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "grid".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view
}

fn drag(view: &mut AppView, m: &mut MonoMeasurer, from: (f32, f32), to: (f32, f32)) {
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let _ = view.layout(bounds, m);
    // Press on nothing interactive anchors; motion with the button down
    // grows the head — the exact sequence the host drives.
    assert_eq!(view.on_click(from.0, from.1, m), rill_viewport::ClickResult::Miss);
    view.set_pressing(true);
    view.set_cursor(to.0, to.1);
    view.set_pressing(false);
    let _ = view.layout(bounds, m);
}

/// A drag within one line copies the covered slice, char-accurate.
#[test]
fn a_single_line_drag_copies_the_covered_slice() {
    let mut view = view();
    let mut m = MonoMeasurer;
    // Line 0 is "alpha beta gamma" at y 0..14, 6px per char.
    // From x=36 (char 6, 'b') to x=60 (char 10, end of "beta").
    drag(&mut view, &mut m, (36.0, 7.0), (60.0, 7.0));
    assert!(view.has_selection());
    assert_eq!(view.selection_text(&mut m).as_deref(), Some("beta"));
}

/// A multi-line drag takes the tail of the first line, whole middle lines
/// across run boundaries, and the head of the last — reading order, runs on
/// one line joined, trailing grid padding dropped.
#[test]
fn a_multi_line_drag_reads_in_order_and_joins_runs() {
    let mut view = view();
    let mut m = MonoMeasurer;
    // From mid-"beta" on line 0 (x=42,y=7) down into line 2 (x=60,y=35).
    drag(&mut view, &mut m, (42.0, 7.0), (60.0, 35.0));
    let text = view.selection_text(&mut m).expect("selected");
    assert_eq!(
        text,
        "eta gamma\ndelta epsilon zeta\neta theta",
        "line 0 tail + joined middle runs + line 2 head, trailing pad trimmed"
    );
}

/// The highlight paints where the selection is: over the selected glyphs,
/// absent when nothing is selected, gone after a plain click.
#[test]
fn the_highlight_follows_and_a_click_clears() {
    let mut view = view();
    let mut m = MonoMeasurer;
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let (commands, ..) = view.layout(bounds, &mut m);
    let baseline = commands.len();

    drag(&mut view, &mut m, (0.0, 7.0), (96.0, 7.0));
    let (commands, ..) = view.layout(bounds, &mut m);
    assert!(commands.len() > baseline, "a selection paints highlight rects");

    // A fresh click with no drag: selection replaced by an empty one.
    let _ = view.on_click(200.0, 200.0, &mut m);
    view.set_pressing(false);
    assert!(!view.has_selection(), "a plain click leaves no selection");
}
