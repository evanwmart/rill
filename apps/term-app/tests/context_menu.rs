//! The terminal's context menu, proven through the real render path: the
//! served page, laid out by the real viewport, right-clicked.

use rill_ui::{LineMetrics, Rect, TextMeasurer};
use rill_viewport::{AppView, ClickResult, Fetcher, Source};

struct MonoMeasurer;
impl TextMeasurer for MonoMeasurer {
    fn measure(&mut self, text: &str, _s: f32, _w: u16, _f: &str, _mw: f32) -> LineMetrics {
        LineMetrics { width: text.chars().count() as f32 * 6.0, height: 14.0 }
    }
}

#[test]
fn right_click_on_the_grid_offers_the_shell_folder() {
    let bytes = term_app::testing::serve_one_page().expect("a page");

    let dir = std::env::temp_dir().join(format!("term-menu-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let mut view = AppView::new(fetcher, Source::Generated { label: "term".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut m = MonoMeasurer;
    let bounds = Rect { x: 0.0, y: 0.0, w: 500.0, h: 300.0 };
    let _ = view.layout(bounds, &mut m);

    // Right-click the middle of the grid: the menu must open.
    assert!(view.context_click(250.0, 100.0), "no menu opened over the terminal grid");
    let _ = view.layout(bounds, &mut m);

    // The reported bug: the terminal re-serves every 50ms, and the menu
    // died within a tick of opening. An in-place refresh must not close
    // the menu the person is aiming at.
    view.reload_keep_focus(Source::Generated {
        label: "term".into(),
        bytes: term_app::testing::serve_one_page().expect("a second page"),
    });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let _ = view.layout(bounds, &mut m);
    assert!(view.menu_open(), "the live tick closed the menu under the pointer");

    // The menu is open; its first item is the folder link. Click it.
    // The item rows sit at the click point, stacked downward.
    let mut hit = ClickResult::Miss;
    for dy in [8.0, 14.0, 20.0, 26.0] {
        match view.on_click(260.0, 100.0 + dy, &mut m) {
            ClickResult::Link(t) => {
                hit = ClickResult::Link(t);
                break;
            }
            _ => {
                // Re-open for the next probe: a miss dismissed it.
                let _ = view.context_click(250.0, 100.0);
                let _ = view.layout(bounds, &mut m);
            }
        }
    }
    match hit {
        ClickResult::Link(t) => {
            assert!(
                t == "/edit" || t.starts_with("/edit/at/"),
                "the menu item links into the editor: {t}"
            );
        }
        _ => panic!("clicking the menu item never produced the link"),
    }
}
