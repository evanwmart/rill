//! Vertical caret movement in a multiline text input — the piece that turns
//! the bound text field into something a person can edit without reaching
//! for the mouse between every line.

use rill_ui::{LineMetrics, Rect, TextMeasurer};
use rill_protocol::ActionValue;
use rill_viewport::{AppView, Fetcher, KeyResult, Source};

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

fn view() -> AppView {
    let dir = std::env::temp_dir().join(format!("viewport-mlinput-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let src = r##"
state "body" initial="alpha\nbeta longer\ngamma"
column gap=0 padding=0 {
    text_input bind="body" multiline=#true
}
"##;
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "editor".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view
}

/// Down walks the caret through the lines holding its column, clamping on a
/// shorter line; Up walks it back; typing after the walk lands where the
/// caret says it is.
#[test]
fn up_and_down_move_the_caret_between_lines() {
    let mut view = view();
    let mut m = MonoMeasurer;
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let _ = view.layout(bounds, &mut m);
    // Focus the input by clicking in it (first line, after "al" — x≈12+pad).
    let _ = view.on_click(20.0, 10.0, &mut m);

    assert_eq!(view.on_key("down", None, false, false, false), KeyResult::Handled);
    assert_eq!(view.on_key("down", None, false, false, false), KeyResult::Handled);
    assert_eq!(view.on_key("end", None, false, false, false), KeyResult::Handled);
    view.on_key("!", Some("!"), false, false, false);
    let ActionValue::Str(body) = view.state_value("body").expect("bound state") else {
        panic!("body is a string slot")
    };
    assert!(body.ends_with("gamma!"), "typing after two Downs edits line 3: {body:?}");

    assert_eq!(view.on_key("up", None, false, false, false), KeyResult::Handled);
    assert_eq!(view.on_key("end", None, false, false, false), KeyResult::Handled);
    view.on_key("?", Some("?"), false, false, false);
    let ActionValue::Str(body) = view.state_value("body").expect("bound state") else {
        panic!("body is a string slot")
    };
    assert!(body.contains("beta longer?"), "Up then End edits line 2: {body:?}");
}

/// Undo restores the value before the edit; redo restores the undo; a
/// fresh edit kills the undone future — the universal contract.
#[test]
fn undo_and_redo_walk_the_edit_history() {
    let mut view = view();
    let mut m = MonoMeasurer;
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let _ = view.layout(bounds, &mut m);
    let _ = view.on_click(20.0, 10.0, &mut m);

    view.on_key("end", None, false, false, false);
    for ch in ["!", "?"] {
        view.on_key(ch, Some(ch), false, false, false);
    }
    let body = |v: &AppView| match v.state_value("body") {
        Some(ActionValue::Str(s)) => s,
        _ => panic!("body is a string"),
    };
    assert!(body(&view).contains("alpha!?"));

    view.on_key("z", Some("z"), true, false, false);
    assert!(body(&view).contains("alpha!") && !body(&view).contains("alpha!?"), "one step back");
    view.on_key("z", Some("z"), true, false, false);
    assert!(!body(&view).contains("alpha!"), "two steps back");
    view.on_key("y", Some("y"), true, false, false);
    assert!(body(&view).contains("alpha!"), "redo walks forward");
    // A fresh edit ends the redo line.
    view.on_key("x", Some("x"), false, false, false);
    view.on_key("y", Some("y"), true, false, false);
    assert!(!body(&view).contains("alpha!?"), "the undone future died");
}

/// Ctrl+S reaches the page's binding even while an input holds focus —
/// the fix for every editor shortcut being a dead key.
#[test]
fn ctrl_s_reaches_the_page_binding_from_inside_an_input() {
    let dir = std::env::temp_dir().join(format!("viewport-ctrls-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let src = r##"
state "body" initial="hello"
column gap=0 padding=0 {
    text_input bind="body" multiline=#true
    key "ctrl+s" target="/saved"
}
"##;
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "ed".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut m = MonoMeasurer;
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let _ = view.layout(bounds, &mut m);
    let _ = view.on_click(20.0, 10.0, &mut m);
    // Focused input; Ctrl+S must surface the page's binding, not vanish.
    assert_eq!(
        view.on_key("s", Some("s"), true, false, false),
        KeyResult::Link("/saved".into()),
        "the save binding fired from inside the input"
    );
    // And plain typing still types.
    view.on_key("s", Some("s"), false, false, false);
    assert!(matches!(view.state_value("body"), Some(ActionValue::Str(s)) if s.contains('s')));
}

/// Tab in a code surface is four spaces to the next stop; Shift+Tab still
/// walks focus, so the keyboard is never trapped. Ordinary inputs keep
/// Tab-moves-focus — a form is not an editor.
#[test]
fn tab_indents_code_and_walks_forms() {
    let dir = std::env::temp_dir().join(format!("viewport-tab-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let src = r##"
state "body" initial="ab"
state "plain" initial=""
column gap=0 padding=0 {
    code bind="body" lang="txt"
    text_input bind="plain"
}
"##;
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "tab".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut m = MonoMeasurer;
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let _ = view.layout(bounds, &mut m);

    // Click into the code surface after "ab" (col 2): Tab pads to col 4.
    let _ = view.on_click(60.0, 12.0, &mut m);
    view.on_key("end", None, false, false, false);
    view.on_key("tab", None, false, false, false);
    let body = match view.state_value("body") {
        Some(ActionValue::Str(s)) => s,
        _ => panic!("body is a string"),
    };
    assert_eq!(body, "ab  ", "col 2 pads two spaces to the stop: {body:?}");

    // And Tab from the code surface with Shift walks focus instead.
    view.on_key("tab", None, false, true, false);
    view.on_key("x", Some("x"), false, false, false);
    let body = match view.state_value("body") {
        Some(ActionValue::Str(s)) => s,
        _ => panic!(),
    };
    assert!(!body.contains('x'), "shift+tab left the code surface");
}
