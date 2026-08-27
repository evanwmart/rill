//! The slider, end to end at the AppView level: a press on the track writes
//! the pointed-at value into the bound slot, a drag follows the pointer
//! (clamped to the range, however far it strays), and the thumb the next
//! layout paints stands where the value says.

use rill_ui::{DrawCommand, LineMetrics, Rect, TextMeasurer};
use rill_viewport::{AppView, ClickResult, Fetcher, Source};

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

fn slider_view() -> AppView {
    let dir = std::env::temp_dir().join(format!("viewport-slider-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let src = r#"
state "v" initial=0.5
column padding=0 {
    slider bind="v" min=0.0 max=1.0
}
"#;
    let bytes = rill_doc::compile(src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "test".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    view
}

/// The slider's hit region and the thumb the same layout painted. The thumb
/// is the one square rect — width == height — which is a property of the
/// control's drawing, so this breaks loudly if that drawing changes.
fn track_and_thumb(commands: &[DrawCommand]) -> (Rect, Rect) {
    let track = commands
        .iter()
        .find_map(|c| match c {
            DrawCommand::SliderArea { rect, .. } => Some(*rect),
            _ => None,
        })
        .expect("a slider area");
    let thumb = commands
        .iter()
        .find_map(|c| match c {
            DrawCommand::Rect { rect, .. } if (rect.w - rect.h).abs() < 0.01 => Some(*rect),
            _ => None,
        })
        .expect("a square thumb");
    (track, thumb)
}

#[test]
fn a_press_sets_the_value_and_a_drag_follows_the_pointer() {
    let mut view = slider_view();
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let mut m = FixedMeasurer;

    // Initial value 0.5: the thumb stands mid-travel.
    let (commands, ..) = view.layout(bounds, &mut m);
    let (track, thumb) = track_and_thumb(&commands);
    let travel = track.w - thumb.w;
    let at = |t: &Rect| (t.x - track.x) / travel;
    assert!((at(&thumb) - 0.5).abs() < 0.02, "thumb mid-travel, got {}", at(&thumb));

    // Press at three quarters: consumed, and the thumb moves there.
    let y = track.y + track.h / 2.0;
    let result = view.on_click(track.x + track.w * 0.75, y, &mut m);
    assert_eq!(result, ClickResult::Consumed);
    let (commands, ..) = view.layout(bounds, &mut m);
    let (_, thumb) = track_and_thumb(&commands);
    assert!((at(&thumb) - 0.75).abs() < 0.02, "thumb at the press, got {}", at(&thumb));

    // Drag far past the left edge: the value clamps to min rather than
    // following the pointer off the range.
    view.set_pressing(true);
    view.on_drag(track.x - 500.0, y - 200.0, &mut m);
    view.set_pressing(false);
    let (commands, ..) = view.layout(bounds, &mut m);
    let (_, thumb) = track_and_thumb(&commands);
    assert!(at(&thumb) < 0.01, "thumb clamped to min, got {}", at(&thumb));

    // A fresh press-drag lands anywhere it points, monotonically.
    view.on_click(track.x + track.w * 0.25, y, &mut m);
    view.set_pressing(true);
    view.on_drag(track.x + track.w * 0.9, y, &mut m);
    view.set_pressing(false);
    let (commands, ..) = view.layout(bounds, &mut m);
    let (_, thumb) = track_and_thumb(&commands);
    assert!((at(&thumb) - 0.9).abs() < 0.03, "thumb followed the drag, got {}", at(&thumb));
}
