//! The mono grid, proven through the whole editor stack: a real document
//! with a multiline `text_input` in the mono face, laid out and hit-tested
//! by the real text engine. If measurement, placement or caret mapping ever
//! disagree about what a cell is, the click below lands on the wrong
//! character and this fails.

use rill_gpu::text::{EngineMeasurer, TextEngine};
use rill_ui::Rect;
use rill_viewport::{AppView, Fetcher, Source};

#[test]
fn a_click_in_a_box_drawing_line_lands_on_its_cell() {
    let engine = TextEngine::new();
    let mut m = EngineMeasurer(&engine);
    // Twenty box glyphs then letters: at natural fallback advances the
    // boxes drift 8px by the twentieth column — more than half a cell —
    // so a click computed from cells discriminates gridded from not.
    let line = "││││││││││││││││││││abcdef";
    let src = format!(
        "style \"editor\" font=\"mono\" size=13\n\
         state \"body\" initial={}\n\
         column gap=0 padding=0 {{\n\
         \tcode bind=\"body\" lang=\"txt\" style=\"editor\"\n\
         }}\n",
        rill_doc::kdl_escape(line),
    );
    let dir = std::env::temp_dir().join(format!("editor-grid-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let fetcher = Fetcher::new(dir.clone(), None, dir).expect("fetcher");
    let bytes = rill_doc::compile(&src).expect("compiles").bytes;
    let mut view = AppView::new(fetcher, Source::Generated { label: "editor".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let bounds = Rect { x: 0.0, y: 0.0, w: 600.0, h: 300.0 };
    let _ = view.layout(bounds, &mut m);

    // One cell, from the same source everything else uses: the engine's
    // own measure of a single mono character.
    let cell = m.measure_cell();

    // Click just past the twentieth box: between box 20 and 'a'. The code
    // surface pads text by its gutter (digits+2 cells) plus 8px; the grid
    // puts the boundary at exactly 20 cells past that.
    let gutter = (3.0 + 2.0) * cell;
    let x = gutter + 8.0 + 20.0 * cell + 1.0;
    let _ = view.on_click(x, 14.0, &mut m);
    // Type at the caret: it must land between the boxes and the letters.
    view.on_key("x", Some("x"), false, false, false);
    let Some(rill_ui::ActionValue::Str(body)) = view.state_value("body") else {
        panic!("body is a string slot");
    };
    assert!(
        body.contains("││││││││││││││││││││xabcdef"),
        "the caret drifted off its cell: {body:?}"
    );
}

/// The cell, measured the way the test needs it — via the public measurer.
trait CellOf {
    fn measure_cell(&mut self) -> f32;
}
impl CellOf for EngineMeasurer<'_> {
    fn measure_cell(&mut self) -> f32 {
        use rill_ui::TextMeasurer;
        self.measure("0", 13.0, 400, "mono", f32::MAX).width
    }
}
