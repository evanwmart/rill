//! A text input never holds more than the wire can carry.
//!
//! The P0 "field-string limit mismatch" fix (TODO.md) made the *input* refuse
//! bytes past `MAX_FIELD_STRING`, so a value that cannot be submitted cannot
//! be typed or pasted in the first place. The helper that enforces it has been
//! reworked since (`append_capped` is gone; `replace_selection` carries the cap
//! now), so this pins the property at the view's own edge, where a host calls
//! it, rather than on any one helper: at the cap the value is intact, one byte
//! over is refused whole, and a multi-byte character that would straddle the
//! cap is refused rather than split.

use rill_protocol::{ActionValue, MAX_FIELD_STRING};
use rill_ui::{LineMetrics, Rect, TextMeasurer};
use rill_viewport::{AppView, Fetcher, KeyResult, Source};

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

const PAGE: &str = r##"
state "body" initial=""
column {
    text_input bind="body" multiline=#true
}
"##;

fn view(name: &str) -> AppView {
    let dir = std::env::temp_dir().join(format!("viewport-input-cap-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let bytes = rill_doc::compile(PAGE).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "test".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view.layout(Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, &mut FixedMeasurer);
    // Tab lands on the first focusable, which is the input.
    assert!(matches!(view.on_key("tab", None, false, false, false), KeyResult::Handled));
    view
}

fn body_len(view: &AppView) -> usize {
    match view.state_value("body") {
        Some(ActionValue::Str(s)) => s.len(),
        other => panic!("body is not a string: {other:?}"),
    }
}

#[test]
fn a_paste_at_the_cap_lands_and_one_byte_over_is_refused_whole() {
    let mut v = view("paste");
    assert!(v.insert_text(&"x".repeat(MAX_FIELD_STRING)), "a focused input consumes a paste");
    assert_eq!(body_len(&v), MAX_FIELD_STRING, "a value exactly at the cap is intact");

    // Over the cap: nothing is inserted, and nothing already there is lost.
    v.insert_text("y");
    assert_eq!(body_len(&v), MAX_FIELD_STRING, "one byte over the cap was accepted");
    v.insert_text("yy");
    assert_eq!(body_len(&v), MAX_FIELD_STRING, "a two-byte paste over the cap was accepted");

    // The typed path shares the cap with paste.
    assert!(matches!(v.on_key("z", Some("z"), false, false, false), KeyResult::Handled));
    assert_eq!(body_len(&v), MAX_FIELD_STRING, "typing past the cap grew the value");
}

#[test]
fn a_character_that_would_straddle_the_cap_is_refused_not_split() {
    let mut v = view("multibyte");
    v.insert_text(&"x".repeat(MAX_FIELD_STRING - 1));
    assert_eq!(body_len(&v), MAX_FIELD_STRING - 1);

    // One byte of room; "é" is two bytes. All-or-nothing means nothing.
    v.on_key("é", Some("é"), false, false, false);
    assert_eq!(body_len(&v), MAX_FIELD_STRING - 1, "a multi-byte char was split or squeezed past the cap");

    // A one-byte char still fits exactly.
    v.insert_text("z");
    assert_eq!(body_len(&v), MAX_FIELD_STRING);
    match v.state_value("body") {
        Some(ActionValue::Str(s)) => assert!(s.ends_with('z') && s.is_char_boundary(MAX_FIELD_STRING)),
        _ => unreachable!(),
    }
}
