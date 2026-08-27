//! Independent scroll regions, end to end at the AppView level: the wheel
//! over a region moves that region's content and nothing else — the rail
//! beside it stands still, the page holds its place, and what scrolls out
//! of the clip stops being clickable.
//!
//! This was the P2 gap ("nested scroll regions clip but can't"): the kit
//! shell put every app's listing beside a sidebar, the whole document was
//! the only thing that scrolled, and the rail rode away with the content.

use rill_ui::{DrawCommand, LineMetrics, Rect, TextMeasurer};
use rill_viewport::{AppView, Fetcher, Source};

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

/// A shell-shaped page: a rail link on the left, a scroll region of many
/// links filling the rest. The window is 300 tall; the region's content is
/// far taller.
fn view() -> AppView {
    let dir = std::env::temp_dir().join(format!("viewport-region-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let mut src = String::from(
        "style \"rail\" width=100\n\
         row gap=0 padding=0 {\n\
         \tcolumn style=\"rail\" { link \"Rail\" target=\"/rail\" }\n\
         \tscroll {\n\t\tcolumn gap=0 padding=0 {\n",
    );
    for i in 0..40 {
        src.push_str(&format!("\t\t\tlink \"entry {i}\" target=\"/e/{i}\"\n"));
    }
    src.push_str("\t\t}\n\t}\n}\n");
    let bytes = rill_doc::compile(&src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "shell".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view
}

fn link_y(commands: &[DrawCommand], target: &str) -> Option<f32> {
    commands.iter().find_map(|c| match c {
        DrawCommand::LinkArea { rect, target: t } if t == target => Some(rect.y),
        _ => None,
    })
}

#[test]
fn the_region_scrolls_and_the_rail_stands_still() {
    let mut view = view();
    let bounds = Rect { x: 0.0, y: 0.0, w: 500.0, h: 300.0 };
    let mut m = FixedMeasurer;
    let (commands, ..) = view.layout(bounds, &mut m);

    let rail_before = link_y(&commands, "/rail").expect("rail link");
    let e0_before = link_y(&commands, "/e/0").expect("first entry");
    assert!(
        link_y(&commands, "/e/39").is_none(),
        "an entry far below the clip must not be hittable before scrolling"
    );

    // Wheel over the region (x=300 is inside it, past the 100px rail).
    view.scroll_at(300.0, 150.0, -200.0);
    let (commands, ..) = view.layout(bounds, &mut m);
    let e0_after = link_y(&commands, "/e/0");
    let rail_after = link_y(&commands, "/rail").expect("rail link survives");

    assert_eq!(rail_after, rail_before, "the rail moved — the region did not scroll alone");
    assert_eq!(view.scroll_offset(), 0.0, "the page scrolled instead of the region");
    match e0_after {
        None => {} // scrolled fully out and trimmed: fine
        Some(y) => assert!(
            y < e0_before,
            "the region's content did not move (entry 0 at {y}, was {e0_before})"
        ),
    }
    // Something deeper is reachable now.
    let deeper = (0..40).rev().find_map(|i| link_y(&commands, &format!("/e/{i}")));
    assert!(deeper.is_some(), "scrolling revealed nothing");

    // Wheel over the rail: no region there, and the page itself fits, so
    // nothing moves at all.
    view.scroll_at(50.0, 150.0, -200.0);
    let (commands, ..) = view.layout(bounds, &mut m);
    assert_eq!(link_y(&commands, "/rail"), Some(rail_before));
    assert_eq!(view.scroll_offset(), 0.0);
}

/// What the clip hides, the pointer cannot press: a scrolled-away entry's
/// hit rect is dropped rather than left floating over whatever now sits in
/// that space.
#[test]
fn scrolled_away_entries_are_not_clickable() {
    let mut view = view();
    let bounds = Rect { x: 0.0, y: 0.0, w: 500.0, h: 300.0 };
    let mut m = FixedMeasurer;
    let _ = view.layout(bounds, &mut m);

    // Scroll deep: the first entries leave through the top of the clip.
    view.scroll_at(300.0, 150.0, -400.0);
    let (commands, ..) = view.layout(bounds, &mut m);
    assert!(
        link_y(&commands, "/e/0").is_none(),
        "entry 0 scrolled out of the region but kept a hit rect"
    );
}
