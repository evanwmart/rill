//! An in-place regeneration is not a navigation. The dock rebuilds its
//! document every clock minute and hands it over with `reload_keep_focus`;
//! the Pi soak (docs/pi-soak.md, 2026-09-08) measured that each of those
//! pushed the compiled document onto the navigation stack — 1.6 MiB/day,
//! one entry per minute, in both week-long runs. The stack must not grow.

use rill_viewport::{AppView, Fetcher, Source};

fn settle(view: &mut AppView) {
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("page never landed");
}

fn strip(minute: u32) -> Vec<u8> {
    let src = format!("row padding=6 {{ text \"12:{minute:02}\" }}");
    rill_doc::compile(&src).expect("compiles").bytes
}

#[test]
fn in_place_reloads_do_not_grow_the_navigation_stack() {
    let dir = std::env::temp_dir().join(format!("viewport-inplace-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Generated { label: "dock".into(), bytes: strip(0) });
    settle(&mut view);
    assert_eq!(view.history_depth(), (1, 0));

    // A week of clock minutes, compressed.
    for minute in 1..=60 {
        view.reload_keep_focus(Source::Generated { label: "dock".into(), bytes: strip(minute) });
        settle(&mut view);
    }
    assert_eq!(view.history_depth(), (1, 0), "a regeneration pushed onto the stack");

    // And Back has nowhere to go: the regenerated page is the only page.
    view.back();
    settle(&mut view);
    assert_eq!(view.history_depth(), (1, 0));

    // A real navigation still pushes — the stack is not simply frozen.
    view.open(Source::Generated { label: "other".into(), bytes: strip(99) });
    settle(&mut view);
    assert_eq!(view.history_depth(), (2, 1));
    // ...and a regeneration on top of it replaces the top, not the bottom.
    view.reload_keep_focus(Source::Generated { label: "other".into(), bytes: strip(98) });
    settle(&mut view);
    assert_eq!(view.history_depth(), (2, 1));
    view.back();
    settle(&mut view);
    assert_eq!(view.history_depth(), (2, 0));
}
