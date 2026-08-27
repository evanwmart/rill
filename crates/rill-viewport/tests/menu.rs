//! The host-presented context menu, end to end at the AppView level: click
//! the pip → the menu paints; it survives relayouts (the regression that
//! shipped: the per-frame collection pass cleared it before it ever drew);
//! Escape closes it.

use rill_viewport::{AppView, Fetcher, Source};
use rill_ui::{DrawCommand, LineMetrics, Rect, TextMeasurer};

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

fn view_with_menu() -> AppView {
    let dir = std::env::temp_dir().join(format!("viewport-menu-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let src = r##"
row target="/open" padding=6 gap=6 {
    text "entry"
    spacer
    button icon="dots-vertical" { menu }
    menu {
        item "Open" target="/open"
        item "Delete" danger=#true { navigate "/rm" }
    }
}
"##;
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view =
        AppView::new(fetcher, Source::Generated { label: "test".into(), bytes });
    // The generated page still arrives through the async fetch plumbing;
    // poll until it lands (bounded — a page that never lands is a failure).
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view
}

fn menu_item_texts(commands: &[DrawCommand]) -> Vec<String> {
    commands
        .iter()
        .filter_map(|c| match c {
            DrawCommand::Text { text, .. } if text == "Open" || text == "Delete" => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn the_pip_opens_a_menu_that_survives_relayout_and_escape_closes_it() {
    let mut view = view_with_menu();
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let mut m = FixedMeasurer;
    let (commands, ..) = view.layout(bounds, &mut m);
    assert!(
        menu_item_texts(&commands).is_empty(),
        "a declared menu paints nothing until opened"
    );
    // The pip is the only button; click its centre.
    let pip = commands
        .iter()
        .find_map(|c| match c {
            DrawCommand::ActionArea { rect, .. } => Some(*rect),
            _ => None,
        })
        .expect("pip button area");
    view.on_click(pip.x + pip.w / 2.0, pip.y + pip.h / 2.0, &mut m);
    assert!(view.menu_open(), "the pip's `menu` action opens the enclosing menu");

    // The menu paints — and keeps painting across relayouts (the shipped
    // bug: the collection pass cleared it before the first paint).
    for pass in 0..3 {
        let (commands, ..) = view.layout(bounds, &mut m);
        assert_eq!(
            menu_item_texts(&commands),
            vec!["Open".to_string(), "Delete".to_string()],
            "menu items painted (relayout {pass})"
        );
    }

    // Escape closes; nothing menu-shaped remains.
    view.on_key("escape", None, false, false, false);
    assert!(!view.menu_open());
    let (commands, ..) = view.layout(bounds, &mut m);
    assert!(menu_item_texts(&commands).is_empty(), "closed menu paints nothing");
}

#[test]
fn right_click_opens_the_innermost_menu_and_outside_click_dismisses() {
    let mut view = view_with_menu();
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let mut m = FixedMeasurer;
    let _ = view.layout(bounds, &mut m);
    assert!(view.context_click(50.0, 10.0), "context click inside the row opens");
    let (commands, ..) = view.layout(bounds, &mut m);
    assert_eq!(menu_item_texts(&commands).len(), 2);

    // A click far outside the panel dismisses and never falls through.
    let result = view.on_click(399.0, 299.0, &mut m);
    assert_eq!(result, rill_viewport::ClickResult::Consumed);
    assert!(!view.menu_open());
}


/// Placement follows the native convention: right/down of the point in the
/// open field; within a menu-size of an edge the menu mirrors around the
/// point rather than sliding along the wall.
#[test]
fn menus_mirror_around_the_point_near_edges()
{
    let mut view = view_with_menu();
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let mut m = FixedMeasurer;
    let _ = view.layout(bounds, &mut m);

    let panel_of = |commands: &[DrawCommand]| -> Rect {
        // The panel is the first painted rect after the menu's shadow.
        commands
            .iter()
            .zip(commands.iter().skip(1))
            .find_map(|(a, b)| match (a, b) {
                (DrawCommand::Shadow { .. }, DrawCommand::Rect { rect, .. }) => Some(*rect),
                _ => None,
            })
            .expect("menu panel")
    };

    // Open field: grows right and down from the point.
    assert!(view.context_click(30.0, 10.0));
    let (commands, ..) = view.layout(bounds, &mut m);
    let p = panel_of(&commands);
    assert!((p.x - 30.0).abs() < 0.5 && (p.y - 10.0).abs() < 0.5, "grows right/down: {p:?}");

    // Near the right edge: mirrors left of the point (not slid to the wall).
    assert!(view.context_click(395.0, 10.0));
    let (commands, ..) = view.layout(bounds, &mut m);
    let p = panel_of(&commands);
    assert!(
        (p.x + p.w - 395.0).abs() < 0.5,
        "mirrored: right edge of the panel sits at the point, got {p:?}"
    );
    assert!(p.x + p.w <= bounds.w + 0.5, "stays inside the viewport");
    view.on_key("escape", None, false, false, false);
}

/// The dock case: a strip shorter than the menu. With overflow-up enabled
/// the panel places fully above the point — negative y — instead of being
/// crushed into the strip.
#[test]
fn an_unbounded_menu_escapes_a_short_viewport() {
    let mut view = view_with_menu();
    view.set_menu_unbounded(true);
    let strip = Rect { x: 0.0, y: 0.0, w: 400.0, h: 40.0 };
    let mut m = FixedMeasurer;
    let _ = view.layout(strip, &mut m);
    assert!(view.context_click(30.0, 20.0));
    let (commands, ..) = view.layout(strip, &mut m);
    let panel = commands
        .iter()
        .zip(commands.iter().skip(1))
        .find_map(|(a, b)| match (a, b) {
            (DrawCommand::Shadow { .. }, DrawCommand::Rect { rect, .. }) => Some(*rect),
            _ => None,
        })
        .expect("menu panel");
    assert!(
        panel.y + panel.h > 40.0,
        "panel escapes the 40px strip instead of being crushed into it: {panel:?}"
    );
    assert!((panel.y - 20.0).abs() < 0.5, "it grows from the click point");
    // And the items are hittable out there.
    let first_item_y = panel.y + 5.0;
    assert!(view.on_click(panel.x + 10.0, first_item_y, &mut m) != rill_viewport::ClickResult::Miss);
}

/// Every item of an escaped menu paints its label — including the ones past
/// the window's cull band.
///
/// The frame cull keeps paint within the window plus a screenful of margin,
/// and for a 40px dock strip that band ends 80px down — while its app menu
/// runs as far as the apps do. The panel (one tall rect crossing the band)
/// survived the cull and the far items' *labels* did not: a menu of four
/// apps showed two names and two blank rows. Menus paint after the cull now;
/// an overlay is bounded by its own size and was never the cull's business.
#[test]
fn an_escaped_menus_far_items_keep_their_labels() {
    // Six items, like a dock with six apps — tall enough that the last ones
    // sit far past the band.
    let dir = std::env::temp_dir().join(format!("viewport-menu-tall-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let mut src = String::from("row target=\"/open\" padding=6 { text \"strip\"\n menu {\n");
    for i in 0..6 {
        src.push_str(&format!("  item \"App number {i}\" target=\"/open\"\n"));
    }
    src.push_str(" }\n}");
    let bytes = rill_doc::compile(&src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "tall".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view.set_menu_unbounded(true);
    // A strip much shorter than the menu, so later items sit far outside
    // the cull band (strip + one strip-height of margin = 80px).
    let strip = Rect { x: 0.0, y: 0.0, w: 400.0, h: 40.0 };
    let mut m = FixedMeasurer;
    let _ = view.layout(strip, &mut m);
    assert!(view.context_click(30.0, 20.0));
    let (commands, ..) = view.layout(strip, &mut m);
    let labels: Vec<(String, f32)> = commands
        .iter()
        .filter_map(|c| match c {
            DrawCommand::Text { text, rect, .. } => Some((text.clone(), rect.y)),
            _ => None,
        })
        .collect();
    let deepest = labels.iter().map(|(_, y)| *y).fold(0.0f32, f32::max);
    assert!(
        deepest > 80.0,
        "no label painted past the cull band ({deepest}px deep) — the far items are blank rows"
    );
}

/// Submitting a control must not throw you back to the top: an action's
/// response is the same page re-served, so scroll survives it. Navigating
/// somewhere new still starts at the top.
#[test]
fn an_action_response_keeps_your_scroll_position() {
    let dir = std::env::temp_dir().join(format!("viewport-scroll-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    // A page taller than its viewport, with a control to submit.
    let mut kdl = String::from("state \"n\" initial=#false\ncolumn gap=6 padding=8 {\n");
    for i in 0..60 {
        kdl.push_str(&format!("\ttext \"row {i}\"\n"));
    }
    kdl.push_str("\tbutton \"Step\" { toggle \"n\" }\n}");
    let bytes = rill_doc::compile(&kdl).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "tall".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let bounds = Rect { x: 0.0, y: 0.0, w: 300.0, h: 200.0 };
    let mut m = FixedMeasurer;
    let _ = view.layout(bounds, &mut m);

    view.scroll_by(-400.0); // negative delta scrolls down
    // The easing runs against the clock rather than the poll count — a scroll
    // has to take the same time on a machine drawing 25 frames a second as on
    // one drawing 60 — so settling it means letting time pass, not spinning.
    for _ in 0..120 {
        view.poll();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let parked = view.scroll_offset();
    assert!(parked > 50.0, "scrolled down, got {parked}");

    // A generated page has no server, so drive the same code path the
    // response takes: an in-place reload.
    view.reload();
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let _ = view.layout(bounds, &mut m);
    assert!(
        (view.scroll_offset() - parked).abs() < 1.0,
        "in-place reload kept the scroll: {} vs {parked}",
        view.scroll_offset()
    );
}

/// A close button is a *button* whose action navigates to `/~close`, and
/// buttons perform their actions inside the view — so without care the
/// window tries to fetch `/~close` from the server and shows NOT_FOUND
/// instead of closing. Host paths must always reach the host.
#[test]
fn host_paths_in_button_actions_reach_the_host() {
    let dir = std::env::temp_dir().join(format!("viewport-hostpath-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let src = r##"
column padding=8 gap=8 {
    button "Close" { navigate "/~close" }
    button "Page" { navigate "/elsewhere" }
}
"##;
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "t".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let mut m = FixedMeasurer;
    let (commands, ..) = view.layout(bounds, &mut m);
    let areas: Vec<Rect> = commands
        .iter()
        .filter_map(|c| match c {
            DrawCommand::ActionArea { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect();
    assert_eq!(areas.len(), 2);
    let hit = |r: &Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);

    let (x, y) = hit(&areas[0]);
    assert_eq!(
        view.on_click(x, y, &mut m),
        rill_viewport::ClickResult::Link("/~close".into()),
        "the host resolves /~close, the view never fetches it"
    );
    // An ordinary navigation stays internal.
    let (x, y) = hit(&areas[1]);
    assert_eq!(view.on_click(x, y, &mut m), rill_viewport::ClickResult::Consumed);
}
