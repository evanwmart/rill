//! Layout unit tests with a deterministic mock measurer: 8px per char,
//! 16px line height, greedy character wrapping.

use std::collections::HashMap;

use rill_doc::{Color, compile};
use rill_ui::{
    Defaults, DrawCommand, LayoutOptions, LineMetrics, Rect, TextMeasurer, layout_document,
    resolve,
};

struct MockText;

impl TextMeasurer for MockText {
    fn measure(&mut self, text: &str, _size: f32, _weight: u16, _family: &str, max_width: f32) -> LineMetrics {
        let total = text.chars().count() as f32 * 8.0;
        if total <= max_width || max_width <= 8.0 {
            LineMetrics { width: total, height: 16.0 }
        } else {
            let lines = (total / max_width.max(8.0)).ceil();
            LineMetrics { width: max_width, height: lines * 16.0 }
        }
    }
}

fn commands(src: &str, width: f32) -> (Vec<DrawCommand>, f32) {
    commands_sized(src, width, None)
}

fn commands_sized(
    src: &str,
    width: f32,
    height: impl Into<Option<f32>>,
) -> (Vec<DrawCommand>, f32) {
    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let tree = resolve(&doc, Defaults::default());
    layout_document(
        &tree,
        LayoutOptions { viewport_width: width, viewport_height: height.into() },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    )
}

/// Resolve a document against a small named-token theme, optionally enforced.
fn themed(src: &str, enforce: bool) -> Vec<DrawCommand> {
    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let mut color_tokens = HashMap::new();
    color_tokens.insert("accent".to_string(), Color { r: 0x00, g: 0xAA, b: 0xFF, a: 0xFF });
    color_tokens.insert("surface".to_string(), Color { r: 0x10, g: 0x10, b: 0x20, a: 0xFF });
    let theme = Defaults {
        text_color: Color { r: 0xEE, g: 0xEE, b: 0xEE, a: 0xFF },
        color_tokens,
        enforce,
        ..Defaults::default()
    };
    let tree = resolve(&doc, theme);
    let (cmds, _) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 400.0, viewport_height: None },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    );
    cmds
}

fn text_color(cmds: &[DrawCommand], needle: &str) -> Color {
    cmds.iter()
        .find_map(|c| match c {
            DrawCommand::Text { text, color, .. } if text == needle => Some(*color),
            _ => None,
        })
        .expect("text present")
}

#[test]
fn tokens_resolve_and_enforce_reskins_literals() {
    // `hot` uses a literal red; `cool` uses the accent token; the column uses
    // a surface-token background.
    let src = r##"
        style "card" background="surface"
        style "hot" color="#ff0000"
        style "cool" color="accent"
        column style="card" { text "hot" style="hot"; text "cool" style="cool" }
    "##;

    let accent = Color { r: 0x00, g: 0xAA, b: 0xFF, a: 0xFF };
    let red = Color { r: 0xFF, g: 0x00, b: 0x00, a: 0xFF };
    let theme_text = Color { r: 0xEE, g: 0xEE, b: 0xEE, a: 0xFF };
    let surface = Color { r: 0x10, g: 0x10, b: 0x20, a: 0xFF };

    // Cooperative: token follows the theme; the literal keeps its own colour.
    let coop = themed(src, false);
    assert_eq!(text_color(&coop, "cool"), accent, "token resolves to theme accent");
    assert_eq!(text_color(&coop, "hot"), red, "literal is left alone");
    assert!(
        coop.iter().any(|c| matches!(c, DrawCommand::Rect { color, .. } if *color == surface)),
        "surface-token background resolves"
    );

    // Enforced override: the user's theme re-skins even the literal.
    let hard = themed(src, true);
    assert_eq!(text_color(&hard, "cool"), accent, "token still resolves");
    assert_eq!(text_color(&hard, "hot"), theme_text, "literal is re-skinned to the role colour");
}

fn text_rects(cmds: &[DrawCommand]) -> Vec<(String, Rect)> {
    cmds.iter()
        .filter_map(|c| match c {
            DrawCommand::Text { rect, text, .. } => Some((text.clone(), *rect)),
            _ => None,
        })
        .collect()
}

#[test]
fn column_stacks_with_gap_and_padding() {
    let (cmds, height) = commands(
        r#"column gap=10 padding=20 { text "aaaa"; text "bbbb" }"#,
        400.0,
    );
    let texts = text_rects(&cmds);
    assert_eq!(texts.len(), 2);
    // First child at padding offset.
    assert_eq!((texts[0].1.x, texts[0].1.y), (20.0, 20.0));
    // Second child below first (16px) plus gap.
    assert_eq!(texts[1].1.y, 20.0 + 16.0 + 10.0);
    // Total: padding + 16 + gap + 16 + padding.
    assert_eq!(height, 20.0 + 16.0 + 10.0 + 16.0 + 20.0);
    // Children get full inner width.
    assert_eq!(texts[0].1.w, 400.0 - 40.0);
}

#[test]
fn text_wraps_and_grows_height() {
    // 40 chars * 8px = 320px into 100px inner width → 4 lines.
    let (cmds, height) = commands(
        &format!(r#"column padding=0 gap=0 {{ text "{}" }}"#, "x".repeat(40)),
        100.0,
    );
    let texts = text_rects(&cmds);
    assert_eq!(texts[0].1.h, 64.0); // 4 * 16
    assert_eq!(height, 64.0);
}

#[test]
fn row_distributes_px_auto_and_fill() {
    // Row 300 wide: rect fixed 50, text "abcd" auto (32), spacer fills rest.
    let (cmds, _) = commands(
        r#"row padding=0 gap=0 { rect width=50 height=10; text "abcd"; spacer; text "zz" }"#,
        300.0,
    );
    let texts = text_rects(&cmds);
    // "abcd" sits right after the 50px rect.
    assert_eq!(texts[0].1.x, 50.0);
    // "zz" (16px wide) is pushed to the far right edge by the auto spacer.
    assert_eq!(texts[1].1.x, 300.0 - 16.0);
}

#[test]
fn container_background_precedes_children_and_has_final_size() {
    let (cmds, _) = commands(
        r##"
style "card" background="#112233" corner=6
column padding=10 style="card" { text "hi" }
"##,
        200.0,
    );
    // Command 0 is the page background; command 1 must be the card.
    let DrawCommand::Rect { rect, corner_radius, .. } = &cmds[1] else {
        panic!("expected card background, got {:?}", cmds[1]);
    };
    assert_eq!(*corner_radius, 6.0);
    assert_eq!((rect.w, rect.h), (200.0, 10.0 + 16.0 + 10.0));
    // And the text paints after it.
    assert!(matches!(cmds[2], DrawCommand::Text { .. }));
}

#[test]
fn link_emits_text_underline_and_hit_area() {
    let (cmds, _) = commands(r#"column { link "go" target="/public/x" }"#, 300.0);
    let has_area = cmds.iter().any(|c| matches!(
        c,
        DrawCommand::LinkArea { target, .. } if target == "/public/x"
    ));
    assert!(has_area, "link hit area missing");
    // Text + 1px underline rect (beyond the page background).
    let texts = text_rects(&cmds);
    assert_eq!(texts[0].0, "go");
    assert!(cmds.iter().any(|c| matches!(
        c,
        DrawCommand::Rect { rect, .. } if rect.h == 1.0
    )));
}

#[test]
fn scroll_clips_when_viewport_definite() {
    let compiled = compile(r#"scroll { column { text "hi" } }"#).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let tree = resolve(&doc, Defaults::default());
    // Definite viewport: clip commands appear.
    let (cmds, height) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 200.0, viewport_height: Some(120.0) },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    );
    assert!(cmds.iter().any(|c| matches!(c, DrawCommand::PushClip { rect, .. } if rect.h == 120.0)));
    assert!(cmds.iter().any(|c| matches!(c, DrawCommand::PopClip)));
    assert_eq!(height, 120.0);
    // Unbounded flow: no clip.
    let (cmds, _) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 200.0, viewport_height: None },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    );
    assert!(!cmds.iter().any(|c| matches!(c, DrawCommand::PushClip { .. })));
}

#[test]
fn layout_is_deterministic() {
    let src = r#"
style "h" size=24 weight="bold"
column gap=8 padding=16 {
    text "Title" style="h"
    row gap=4 { text "left"; spacer; text "right" }
    rect width=100 height=4
}
"#;
    let (a, ha) = commands(src, 640.0);
    let (b, hb) = commands(src, 640.0);
    assert_eq!(a, b);
    assert_eq!(ha, hb);
}

#[test]
fn page_background_covers_everything() {
    let (cmds, height) = commands(r#"column padding=5 { text "x" }"#, 320.0);
    let DrawCommand::Rect { rect, .. } = &cmds[0] else {
        panic!("first command must be the page background");
    };
    assert_eq!((rect.w, rect.h), (320.0, height));
}

/// A page that declares its own background gets it — including a fully
/// transparent one, which is how a document says "the window's own material
/// is my background, do not paint a panel over it".
#[test]
fn a_declared_page_background_replaces_the_theme_colour() {
    let (cmds, _) = commands(
        r##"column padding=5 { page background="#00000000"; text "x" }"##,
        320.0,
    );
    let DrawCommand::Rect { color, .. } = &cmds[0] else {
        panic!("first command must be the page background");
    };
    assert_eq!(color.a, 0, "a clear page paints nothing behind itself");

    // Undeclared, it is still the theme's, opaque as ever.
    let (cmds, _) = commands(r#"column padding=5 { text "x" }"#, 320.0);
    let DrawCommand::Rect { color, .. } = &cmds[0] else { panic!() };
    assert_eq!(color.a, 255);
}

#[test]
fn image_natural_size_scales_to_fit() {
    struct Sized;
    impl rill_ui::ImageSizer for Sized {
        fn natural_size(&mut self, _s: &str) -> Option<(f32, f32)> {
            Some((400.0, 100.0))
        }
    }
    let compiled = compile(r#"column padding=0 gap=0 { image "/public/pic.png" }"#).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let tree = resolve(&doc, Defaults::default());
    // Fits: natural size used as-is.
    let (cmds, _) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 640.0, viewport_height: None },
        &mut MockText,
        &mut Sized,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    );
    let img = cmds.iter().find_map(|c| match c {
        DrawCommand::Image { rect, .. } => Some(*rect),
        _ => None,
    }).unwrap();
    assert_eq!((img.w, img.h), (400.0, 100.0));
    // Constrained: scaled down preserving aspect (200 wide -> 50 tall).
    let (cmds, _) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 200.0, viewport_height: None },
        &mut MockText,
        &mut Sized,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    );
    let img = cmds.iter().find_map(|c| match c {
        DrawCommand::Image { rect, .. } => Some(*rect),
        _ => None,
    }).unwrap();
    assert_eq!((img.w, img.h), (200.0, 50.0));
}

#[test]
fn containers_in_rows_split_leftover_equally() {
    // Two columns in a 400px row: each gets half; their texts wrap inside.
    let (cmds, _) = commands(
        r#"row gap=0 padding=0 { column padding=0 gap=0 { text "aa" }; column padding=0 gap=0 { text "bb" } }"#,
        400.0,
    );
    let texts = text_rects(&cmds);
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0].1.x, 0.0);
    assert_eq!(texts[1].1.x, 200.0, "second pane starts at the midpoint");
    // A fixed rect plus two columns: rect fixed, panes split the rest.
    let (cmds, _) = commands(
        r#"row gap=0 padding=0 { rect width=100 height=4; column padding=0 { text "aa" }; column padding=0 { text "bb" } }"#,
        400.0,
    );
    let texts = text_rects(&cmds);
    assert_eq!(texts[0].1.x, 100.0);
    assert_eq!(texts[1].1.x, 250.0, "150px each after the fixed rect");
}

/// Alignment is resolved at layout time into an x-offset. The mock measurer
/// is 8px/char, so the arithmetic is exact and the assertion is about
/// position, not about a renderer's opinion.
#[test]
fn align_moves_text_within_its_width() {
    let src = r##"
        style "r" align="right"
        style "c" align="center"
        column padding=0 {
            text "abcd"
            text "abcd" style="r"
            text "abcd" style="c"
        }
    "##;
    // 4 chars * 8px = 32px wide inside a 100px viewport → 68px of slack.
    let (cmds, _) = commands(src, 100.0);
    let xs: Vec<f32> = text_rects(&cmds).iter().map(|(_, r)| r.x).collect();
    assert_eq!(xs.len(), 3, "three text runs");
    assert_eq!(xs[0], 0.0, "left is the default");
    assert_eq!(xs[1], 68.0, "right puts all the slack before the text");
    assert_eq!(xs[2], 34.0, "center splits the slack");
}

/// Alignment must not change wrapping: only the origin moves, so a
/// paragraph breaks in exactly the same places whatever its alignment.
#[test]
fn align_does_not_change_wrapping() {
    let body = "aaaaaaaaaaaaaaaaaaaa";
    let plain = format!("column padding=0 {{ text \"{body}\" }}");
    let right = format!("style \"r\" align=\"right\"\ncolumn padding=0 {{ text \"{body}\" style=\"r\" }}");
    let (a, ha) = commands(&plain, 60.0);
    let (b, hb) = commands(&right, 60.0);
    assert_eq!(ha, hb, "same height means the same number of lines");
    let ra = text_rects(&a)[0].1;
    let rb = text_rects(&b)[0].1;
    assert_eq!(ra.w, rb.w, "the wrap box is unchanged");
    assert_eq!(ra.h, rb.h, "the wrapped height is unchanged");
}

/// A container pinned by style width holds its size while its siblings take
/// the leftover — the sidebar-beside-content shape that containers could not
/// express before, because they always flexed.
#[test]
fn style_width_pins_a_container_in_a_row() {
    let src = r##"
        style "side" width=190
        row gap=0 padding=0 {
            column style="side" padding=0 gap=0 { text "places" }
            column padding=0 gap=0 { text "content" }
        }
    "##;
    let (cmds, _) = commands(src, 900.0);
    let rects = text_rects(&cmds);
    let side = rects.iter().find(|(t, _)| t == "places").expect("sidebar text").1;
    let main = rects.iter().find(|(t, _)| t == "content").expect("content text").1;
    assert_eq!(side.x, 0.0, "sidebar starts at the left edge");
    assert_eq!(main.x, 190.0, "content starts exactly after the pinned width");
    assert_eq!(main.w, 710.0, "content takes all the leftover");
}

/// "fill" weights work the same way spacers already do, so two panes can
/// split space unevenly.
#[test]
fn style_fill_weights_split_the_leftover() {
    let src = r##"
        style "one" width="fill"
        style "two" width=2
        row gap=0 padding=0 {
            column style="one" padding=0 gap=0 { text "a" }
            column style="two" padding=0 gap=0 { text "b" }
        }
    "##;
    // "two" is 2px (a number is pixels), so "one" takes the other 98.
    let (cmds, _) = commands(src, 100.0);
    let rects = text_rects(&cmds);
    let b = rects.iter().find(|(t, _)| t == "b").expect("second pane").1;
    assert_eq!(b.x, 98.0, "the pinned 2px pane sits at the far right");
}

/// A link styled as a list row paints no underline, but stays clickable —
/// the hit region is the point of a link, the decoration is not.
#[test]
fn underline_can_be_suppressed_without_losing_the_hit_area() {
    let src = r##"
        style "row" underline=#false color="#ffffff"
        column padding=0 { link "a.txt" target="/a" style="row" }
    "##;
    let (cmds, _) = commands(src, 200.0);
    assert!(
        cmds.iter().any(|c| matches!(c, DrawCommand::LinkArea { target, .. } if target == "/a")),
        "still a link"
    );
    // The only Rect should be the page background — no underline rule.
    let rects = cmds.iter().filter(|c| matches!(c, DrawCommand::Rect { .. })).count();
    assert_eq!(rects, 1, "no underline painted");

    // Default links keep their underline.
    let plain = r##"column padding=0 { link "a.txt" target="/a" }"##;
    let (cmds, _) = commands(plain, 200.0);
    let rects = cmds.iter().filter(|c| matches!(c, DrawCommand::Rect { .. })).count();
    assert_eq!(rects, 2, "page background plus the underline");
}

/// Naming a scale step puts type and spacing under the theme's control, the
/// way colour tokens already are: swap the table, and every page that named
/// steps re-types and re-spaces without being touched.
#[test]
fn scale_tokens_resolve_against_the_theme() {
    let src = r##"
        style "head" size="lg"
        style "card" padding="lg" gap="sm"
        column style="card" { text "title" style="head"; text "body" }
    "##;

    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();

    // Default scale: lg type is 20, lg space 20, sm space 8.
    let tree = resolve(&doc, Defaults::default());
    let (cmds, _) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 400.0, viewport_height: None },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[], None, 0, (0, 0), None, false,
    );
    let rects = text_rects(&cmds);
    let title = rects.iter().find(|(t, _)| t == "title").unwrap().1;
    let body = rects.iter().find(|(t, _)| t == "body").unwrap().1;
    assert_eq!(title.x, 20.0, "lg padding");
    // MockText is 16px per line regardless of size, so the gap is the
    // difference beyond one line.
    assert_eq!(body.y - (title.y + title.h), 8.0, "sm gap");

    // A denser theme re-spaces the same document, untouched.
    let mut dense = Defaults::default();
    dense.space_tokens.insert("lg".to_string(), 8.0);
    dense.size_tokens.insert("lg".to_string(), 14.0);
    let tree = resolve(&doc, dense);
    let (cmds, _) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 400.0, viewport_height: None },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[], None, 0, (0, 0), None, false,
    );
    let title = text_rects(&cmds).into_iter().find(|(t, _)| t == "title").unwrap().1;
    assert_eq!(title.x, 8.0, "the theme re-spaced it");
}

/// A literal size still means that size — naming a step is opt-in, so pages
/// that want a specific number keep it.
#[test]
fn a_literal_size_is_not_overridden_by_the_scale() {
    let src = r##"
        style "exact" size=17
        column padding=0 { text "x" style="exact" }
    "##;
    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    assert_eq!(doc.styles[0].font_size, Some(17.0));
    assert_eq!(doc.styles[0].size_token, None);
}

/// Saying nothing about spacing gets the system's rhythm; saying zero still
/// means zero. These used to be the same bytes, which is why the theme could
/// never supply anything.
#[test]
fn unstated_spacing_comes_from_the_theme() {
    let stated = r##"column padding=0 gap=0 { text "a"; text "b" }"##;
    let (cmds, _) = commands(stated, 200.0);
    let r = text_rects(&cmds);
    assert_eq!(r[0].1.x, 0.0, "explicit zero padding is honoured");
    assert_eq!(r[1].1.y - (r[0].1.y + r[0].1.h), 0.0, "explicit zero gap too");

    let unstated = r##"column { text "a"; text "b" }"##;
    let (cmds, _) = commands(unstated, 200.0);
    let r = text_rects(&cmds);
    assert_eq!(r[0].1.x, 12.0, "system padding (md)");
    assert_eq!(r[1].1.y - (r[0].1.y + r[0].1.h), 8.0, "system gap (sm)");

    // And the system's idea of a rhythm is the theme's to change.
    let compiled = compile(unstated).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let roomy = Defaults { container_padding: 40.0, ..Defaults::default() };
    let tree = resolve(&doc, roomy);
    let (cmds, _) = layout_document(
        &tree,
        LayoutOptions { viewport_width: 200.0, viewport_height: None },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[], None, 0, (0, 0), None, false,
    );
    assert_eq!(text_rects(&cmds)[0].1.x, 40.0, "the theme re-spaced it");
}

/// Depth, outline and hover: the three things that separate a surface from a
/// printed page. All resolved at layout time; the renderer already knew how
/// to draw the first two.
#[test]
fn boxes_can_have_depth_outline_and_a_hover_state() {
    let src = r##"
        style "card"  background="#202030" corner=10 shadow="md" border=1 border-color="#404060" hover="lit"
        style "lit"   background="#2a2a44" corner=10 shadow="lg" border=1 border-color="#8080ff"
        column padding=0 gap=0 { column style="card" padding=10 { text "x" } }
    "##;
    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let tree = resolve(&doc, Defaults::default());

    let paint = |cursor: Option<(f32, f32)>| {
        layout_document(
            &tree,
            LayoutOptions { viewport_width: 200.0, viewport_height: None },
            &mut MockText,
            &mut rill_ui::NoImages,
            &[], None, 0, (0, 0), cursor, false,
        ).0
    };

    // At rest: shadow under the fill, outline over it.
    let cmds = paint(None);
    let kinds: Vec<&str> = cmds.iter().map(|c| match c {
        DrawCommand::Shadow { .. } => "shadow",
        DrawCommand::Border { .. } => "border",
        DrawCommand::Rect { .. } => "rect",
        _ => "other",
    }).collect();
    let shadow = kinds.iter().position(|k| *k == "shadow").expect("a shadow");
    let border = kinds.iter().position(|k| *k == "border").expect("a border");
    assert!(shadow < border, "shadow is painted under the box, outline over it");

    let blur_at_rest = cmds.iter().find_map(|c| match c {
        DrawCommand::Shadow { blur, .. } => Some(*blur),
        _ => None,
    }).unwrap();
    assert_eq!(blur_at_rest, 18.0, "md elevation");

    // Under the cursor the hover variant paints instead — lifted further.
    let cmds = paint(Some((20.0, 10.0)));
    let blur_hovered = cmds.iter().find_map(|c| match c {
        DrawCommand::Shadow { blur, .. } => Some(*blur),
        _ => None,
    }).unwrap();
    assert_eq!(blur_hovered, 32.0, "lg elevation while hovered");

    // Away from the box, it is at rest again.
    let cmds = paint(Some((199.0, 199.0)));
    let blur_away = cmds.iter().find_map(|c| match c {
        DrawCommand::Shadow { blur, .. } => Some(*blur),
        _ => None,
    }).unwrap();
    assert_eq!(blur_away, 18.0, "not hovered");
}

/// An elevation step brings its own surface, because a shadow alone barely
/// reads on a dark page — the thing that actually says "closer" is the
/// lighter surface. A style that names its own background keeps it.
#[test]
fn elevation_lifts_the_surface_not_just_the_shadow() {
    let mut theme = Defaults::default();
    let lifted = Color { r: 0x24, g: 0x24, b: 0x38, a: 0xFF };
    theme.color_tokens.insert("elevation-md".to_string(), lifted);
    theme.color_tokens.insert("surface".to_string(), Color { r: 1, g: 2, b: 3, a: 0xFF });

    let paint = |src: &str, theme: &Defaults| {
        let compiled = compile(src).unwrap();
        let doc = rill_doc::decode(&compiled.bytes).unwrap();
        let tree = resolve(&doc, theme.clone());
        layout_document(
            &tree,
            LayoutOptions { viewport_width: 100.0, viewport_height: None },
            &mut MockText, &mut rill_ui::NoImages, &[], None, 0, (0, 0), None, false,
        ).0
    };

    // shadow alone: the step supplies the surface.
    let cmds = paint(r##"style "c" shadow="md"
        column padding=0 gap=0 { column style="c" { text "x" } }"##, &theme);
    assert!(
        cmds.iter().any(|c| matches!(c, DrawCommand::Rect { color, .. } if *color == lifted)),
        "the elevation step painted its surface"
    );

    // An explicit background wins — elevation only fills the gap.
    let cmds = paint(r##"style "c" shadow="md" background="surface"
        column padding=0 gap=0 { column style="c" { text "x" } }"##, &theme);
    assert!(
        cmds.iter().any(|c| matches!(c, DrawCommand::Rect { color, .. }
            if *color == Color { r: 1, g: 2, b: 3, a: 0xFF })),
        "the style's own background is not overridden"
    );
}

/// Frosted panels are now something a document can ask for — and a page that
/// asks for too many gets fewer, not a frame that cannot be encoded.
#[test]
fn backdrops_are_exposed_and_capped() {
    let src = r##"style "glass" backdrop=20 background="#20203080"
        column padding=0 gap=0 { column style="glass" { text "x" } }"##;
    let (cmds, _) = commands(src, 100.0);
    let backdrop = cmds.iter().position(|c| matches!(c, DrawCommand::Backdrop { .. }));
    let fill = cmds.iter().position(|c| matches!(c, DrawCommand::Rect { color, .. } if color.a < 255));
    assert!(backdrop.is_some(), "the document asked for frost and got it");
    assert!(backdrop < fill, "frost sits under the fill that tints it");

    // Far more panels than the wire allows: the extras simply are not frosted.
    let many = (0..rill_ui::stream::MAX_BACKDROPS + 20)
        .map(|_| r#"column style="glass" { text "x" }"#.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let src = format!(
        r##"style "glass" backdrop=20 background="#20203080"
            column padding=0 gap=0 {{ {many} }}"##
    );
    let (cmds, _) = commands(&src, 100.0);
    let count = cmds.iter().filter(|c| matches!(c, DrawCommand::Backdrop { .. })).count();
    assert_eq!(count, rill_ui::stream::MAX_BACKDROPS, "capped at the wire limit");
    rill_ui::stream::encode(&cmds).expect("and the frame still encodes");
}

/// An icon lays out as filled contours — Phosphor ships pre-outlined
/// glyphs — and an unknown name holds its space rather than collapsing the
/// row it labels.
#[test]
fn icons_lay_out_as_fills() {
    let src = r##"row padding=0 gap=0 { icon "folder" size=16; text "docs" }"##;
    let (cmds, _) = commands(src, 200.0);
    let fills: Vec<&DrawCommand> = cmds
        .iter()
        .filter(|c| matches!(c, DrawCommand::FillPath { .. }))
        .collect();
    assert!(!fills.is_empty(), "the folder glyph drew something");
    for cmd in &fills {
        let DrawCommand::FillPath { points, contours, .. } = cmd else { unreachable!() };
        assert!(!contours.is_empty(), "closed rings, not a blob");
        assert_eq!(
            contours.iter().map(|c| *c as usize).sum::<usize>(),
            points.len(),
            "rings partition the points"
        );
        for p in points {
            assert!(p.x >= -0.5 && p.x <= 16.5, "glyph stays in its 16px box: {p:?}");
        }
    }
    // The label sits after the icon, not on top of it.
    let label = text_rects(&cmds).into_iter().find(|(t, _)| t == "docs").unwrap().1;
    assert!(label.x >= 16.0, "text follows the icon");

    // An unknown glyph still occupies its space.
    let src = r##"row padding=0 gap=0 { icon "no-such-glyph" size=16; text "docs" }"##;
    let (cmds, _) = commands(src, 200.0);
    assert!(!cmds.iter().any(|c| matches!(c, DrawCommand::Path { .. })), "nothing drawn");
    let label = text_rects(&cmds).into_iter().find(|(t, _)| t == "docs").unwrap().1;
    assert!(label.x >= 16.0, "but the space is still reserved");
}

/// A wrapping row is a grid: tiles keep their own width and start a new line
/// when the next will not fit. This is the shape a file manager's default
/// view is built from, and a row could not make it.
#[test]
fn a_wrapping_row_flows_into_lines() {
    let src = r##"
        style "grid" wrap=#true
        style "tile" width=100
        row style="grid" gap=10 padding=0 {
            column style="tile" padding=0 { text "a" }
            column style="tile" padding=0 { text "b" }
            column style="tile" padding=0 { text "c" }
            column style="tile" padding=0 { text "d" }
        }
    "##;
    // 340 wide: two 100px tiles plus a 10px gap fit (210); a third needs 320,
    // which also fits; a fourth needs 430, which does not.
    let (cmds, _) = commands(src, 340.0);
    let at = |label: &str| text_rects(&cmds).into_iter().find(|(t, _)| t == label).unwrap().1;
    let (a, b, c, d) = (at("a"), at("b"), at("c"), at("d"));

    assert_eq!(a.x, 0.0);
    assert_eq!(b.x, 110.0, "second tile after one gap");
    assert_eq!(c.x, 220.0, "third still fits on the line");
    assert_eq!(d.x, 0.0, "fourth wrapped to a new line");
    assert!(d.y > a.y, "and sits below the first");
    assert_eq!(a.y, b.y, "tiles on a line share a baseline");
}

/// Tiles keep their width instead of sharing the line — a folder holding
/// three items must not show three enormous ones.
#[test]
fn grid_tiles_do_not_stretch_to_fill() {
    let src = r##"
        style "grid" wrap=#true
        style "tile" width=80
        row style="grid" gap=0 padding=0 { column style="tile" padding=0 { text "x" }; column style="tile" padding=0 { text "y" } }
    "##;
    let (cmds, _) = commands(src, 900.0);
    let y = text_rects(&cmds).into_iter().find(|(t, _)| t == "y").unwrap().1;
    assert_eq!(y.x, 80.0, "second tile sits right after the first, not half way across");
}

/// A style may say a literal spacing as well as name a step. Zero has no
/// scale step and should not get one, so without this a style could not
/// express "no padding" at all.
#[test]
fn style_spacing_takes_literals_as_well_as_steps() {
    let src = r##"
        style "flush" padding=0 gap=0
        style "roomy" padding="lg"
        column style="flush" { text "a"; text "b"; text "c" style="roomy" }
    "##;
    let (cmds, _) = commands(src, 200.0);
    let r = text_rects(&cmds);
    assert_eq!(r[0].1.x, 0.0, "literal zero padding");
    assert_eq!(r[1].1.y - (r[0].1.y + r[0].1.h), 0.0, "literal zero gap");

    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let roomy = doc.styles.iter().find(|s| doc.string(s.name_idx) == "roomy").unwrap();
    assert!(roomy.padding_token.is_some(), "a name is still a step");
    assert!(roomy.padding_px.is_none());
}

/// Alignment belongs to every leaf that sizes to its own content, not just to
/// text. A grid tile centres an icon over a label; if only the label honoured
/// `align` the two would never line up.
#[test]
fn every_self_sizing_leaf_honours_alignment() {
    let src = r##"
        state "x" initial=#false
        style "mid" align="center"
        style "box" width=200
        column style="box" {
            text "t" style="mid"
            link "l" target="/x" style="mid"
            button "b" style="mid" { toggle "x" }
            icon "folder" size=40 style="mid"
        }
    "##;
    let (cmds, _) = commands(src, 200.0);
    let mid = |x: f32, w: f32| x + w / 2.0;

    let centres: Vec<f32> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCommand::Text { rect, .. } => Some(mid(rect.x, 0.0)),
            _ => None,
        })
        .collect();
    // Text reports its wrap box as full width, so compare the drawn origins:
    // a centred short label must not start at the container's left edge.
    for x in &centres {
        assert!(*x > 10.0, "a centred leaf must not sit at the leading edge: {x}");
    }

    let icon_x = cmds
        .iter()
        .find_map(|c| match c {
            DrawCommand::FillPath { points, .. } => points.first().map(|p| p.x),
            _ => None,
        })
        .expect("icon fill");
    assert!(icon_x > 60.0, "a centred 40px icon starts near x=80, not {icon_x}");

    let btn = cmds
        .iter()
        .find_map(|c| match c {
            DrawCommand::Rect { rect, .. } if rect.h > 20.0 => Some(*rect),
            _ => None,
        })
        .expect("button chrome");
    let off = (mid(btn.x, btn.w) - 100.0).abs();
    assert!(off < 1.0, "button should centre on the container: off by {off}");
}

/// A spacer flexes in a row; it must flex in a column too, or "push this to
/// the bottom" — a sidebar footer, a status line — silently does nothing.
#[test]
fn a_spacer_absorbs_column_slack() {
    let src = r##"
        style "tall" height="fill"
        column style="tall" { text "top"; spacer; text "foot" }
    "##;
    let (cmds, _) = commands_sized(src, 200.0, 400.0f32);
    let r = text_rects(&cmds);
    assert_eq!(r.len(), 2);
    let foot = r[1].1;
    assert!(
        foot.y + foot.h > 380.0,
        "the footer should sit at the bottom of a 400px column, not at {}",
        foot.y
    );
}

/// A document can claim the window's own chrome. What it puts there must
/// leave the page flow entirely — a host that lends its titlebar would
/// otherwise draw the toolbar twice, and one that cannot must not have it
/// silently reappear mid-document.
#[test]
fn chrome_leaves_the_document_flow() {
    let src = r##"
        column {
            titlebar { row { link "Root" target="/" } }
            text "body"
        }
    "##;
    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let tree = resolve(&doc, Defaults::default());

    let (cmds, _) = commands(src, 400.0);
    let drawn: Vec<&str> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(drawn, vec!["body"], "chrome must not render in the page");
    assert!(!cmds.iter().any(|c| matches!(c, DrawCommand::LinkArea { .. })));

    let bar = rill_ui::layout_chrome(
        &tree,
        Rect { x: 0.0, y: 0.0, w: 400.0, h: 44.0 },
        &mut MockText,
        &mut rill_ui::NoImages,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    );
    assert!(
        bar.iter().any(|c| matches!(c, DrawCommand::Text { text, .. } if text == "Root")),
        "the chrome subtree renders when the window asks for it"
    );
    assert!(
        bar.iter().any(|c| matches!(c, DrawCommand::LinkArea { .. })),
        "and stays clickable there"
    );
}

/// A row hung every child from its top, so a label beside a taller button
/// floated above it. `valign` is the same three positions turned ninety
/// degrees.
#[test]
fn a_row_can_centre_its_children_vertically() {
    let src = r##"
        state "x" initial=#false
        style "mid" valign="center"
        style "box" width=400
        column style="box" {
            row style="mid" { text "label"; button "B" { toggle "x" } }
        }
    "##;
    let (cmds, _) = commands(src, 400.0);
    let label = text_rects(&cmds).into_iter().find(|(t, _)| t == "label").unwrap().1;
    let button = cmds
        .iter()
        .find_map(|c| match c {
            DrawCommand::Rect { rect, .. } if rect.h > 20.0 => Some(*rect),
            _ => None,
        })
        .expect("button chrome");
    let centre = |r: Rect| r.y + r.h / 2.0;
    assert!(
        (centre(label) - centre(button)).abs() < 1.0,
        "label centres on {} but the button on {}",
        centre(label),
        centre(button)
    );
}

/// A wrapped label in a fixed-width tile pushes the grid out of rhythm; a
/// truncated one does not.
#[test]
fn ellipsis_keeps_overrun_text_on_one_line() {
    let src = r##"
        style "tile" width=80
        style "clip" ellipsis=#true
        column style="tile" { text "a-very-long-filename-indeed.txt" style="clip" }
    "##;
    let (cmds, _) = commands(src, 80.0);
    let (drawn, rect) = text_rects(&cmds).into_iter().next().unwrap();
    assert!(drawn.ends_with('\u{2026}'), "truncated with an ellipsis: {drawn:?}");
    assert!(drawn.len() < "a-very-long-filename-indeed.txt".len());
    assert_eq!(rect.h, 16.0, "one line, not a wrapped block");

    // Text that fits is left exactly alone.
    let (short, _) = commands(
        r##"style "tile" width=400
            style "clip" ellipsis=#true
            column style="tile" { text "fits" style="clip" }"##,
        400.0,
    );
    assert_eq!(text_rects(&short)[0].0, "fits");
}

/// An icon button is square by saying so: a pinned width wins over the
/// label-derived one, and the glyph centres in the box.
#[test]
fn a_button_style_may_pin_its_width() {
    let src = r##"
        state "x" initial=#false
        style "sq" width=28 padding="xs" size=13
        row { button "i" style="sq" { toggle "x" } }
    "##;
    let (cmds, _) = commands(src, 200.0);
    let chrome = cmds
        .iter()
        .find_map(|c| match c {
            // Skip the page background; the button's box is the narrow one.
            DrawCommand::Rect { rect, .. } if rect.h > 10.0 && rect.w < 100.0 => Some(*rect),
            _ => None,
        })
        .expect("button chrome");
    assert_eq!(chrome.w, 28.0, "pinned, not label-derived");
    let label = text_rects(&cmds)[0].1;
    let off = ((label.x + label.w / 2.0) - (chrome.x + chrome.w / 2.0)).abs();
    assert!(off < 1.0, "label centres in the box: off by {off}");
}

/// A toolbar wants wide horizontal insets and a thin vertical profile —
/// uniform padding cannot say that, so the axes are separately styleable
/// and the axis value wins over the uniform one.
#[test]
fn padding_axes_override_the_uniform_value() {
    let src = r##"
        style "bar" padding=4 padding-x=20
        column style="bar" { text "t" }
    "##;
    let (cmds, _) = commands(src, 200.0);
    let t = text_rects(&cmds)[0].1;
    assert_eq!(t.x, 20.0, "x from padding-x");
    assert_eq!(t.y, 4.0, "y from uniform padding");
}

/// Equal controls sharing a bar: fill-width buttons split the row evenly,
/// like any other flex slot.
#[test]
fn fill_width_buttons_share_a_row_equally() {
    let src = r##"
        state "x" initial=#false
        style "ctl" width="fill" padding="xs" size=11
        row gap=0 padding=0 {
            button "a" style="ctl" { toggle "x" }
            button "b" style="ctl" { toggle "x" }
            button "c" style="ctl" { toggle "x" }
            button "d" style="ctl" { toggle "x" }
        }
    "##;
    let (cmds, _) = commands(src, 400.0);
    let boxes: Vec<Rect> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCommand::Rect { rect, .. } if rect.h > 10.0 && rect.w < 200.0 => Some(*rect),
            _ => None,
        })
        .collect();
    assert_eq!(boxes.len(), 4);
    for b in &boxes {
        assert_eq!(b.w, 100.0, "four fill buttons in 400px are 100 each");
    }
}

/// A row that fills a definite height centres its children in that height —
/// not against its tallest child, which leaves all the slack below.
#[test]
fn valign_centres_against_the_rows_own_height() {
    let src = r##"
        style "outer" height="fill"
        style "band" height="fill" valign="center"
        column style="outer" { row style="band" { text "t" } }
    "##;
    let (cmds, _) = commands_sized(src, 200.0, 100.0f32);
    let t = text_rects(&cmds)[0].1;
    let centre = t.y + t.h / 2.0;
    assert!(
        (centre - 50.0).abs() < 1.0,
        "text centres in the 100px band, not at the top: centre {centre}"
    );
}

/// Table tracks without hand-sizing: elements sharing a measure group are
/// laid out at the width of the group's widest member, so a right-anchored
/// column starts at the same x in every row regardless of what the row's
/// flexible side contains.
#[test]
fn a_measure_group_sizes_the_column_by_its_widest_member() {
    let src = r##"
        style "cell" group="col"
        column {
            row gap=0 padding=0 { text "a" ; spacer; text "x" style="cell" }
            row gap=0 padding=0 { text "bbbb" ; spacer; text "wwwwww" style="cell" }
        }
    "##;
    let (cmds, _) = commands(src, 400.0);
    let cells: Vec<Rect> = text_rects(&cmds)
        .into_iter()
        .filter(|(t, _)| t == "x" || t == "wwwwww")
        .map(|(_, r)| r)
        .collect();
    assert_eq!(cells.len(), 2);
    assert!(
        (cells[0].x - cells[1].x).abs() < 1.0,
        "the column starts at one x in every row: {} vs {}",
        cells[0].x,
        cells[1].x
    );
}

/// A closed `when` is geometrically absent: it costs no gap. Before this,
/// a hidden form left a phantom double-gap above whatever followed it.
#[test]
fn a_hidden_child_costs_no_gap() {
    let with_when = r##"
        state "off" initial=#false
        style "box" gap=10 padding=0
        column style="box" { when "off" { text "form" }; text "content" }
    "##;
    let without = r##"
        style "box" gap=10 padding=0
        column style="box" { text "content" }
    "##;
    let a = text_rects(&commands(with_when, 200.0).0)[0].1;
    let b = text_rects(&commands(without, 200.0).0)[0].1;
    assert_eq!(a.y, b.y, "hidden when shifts nothing: {} vs {}", a.y, b.y);
}

/// A container may be a click target: the whole row opens the file, not
/// only the label — while an interactive child inside it still wins,
/// because hit-testing is document order and children come first.
#[test]
fn a_container_target_covers_the_whole_row() {
    let src = r##"
        state "x" initial=#false
        style "pick" width=28 padding="xs"
        row target="/open" padding=0 gap=0 {
            text "name"
            spacer
            button icon="dots-vertical" style="pick" { toggle "x" }
        }
    "##;
    let (cmds, _) = commands(src, 400.0);
    let link = cmds
        .iter()
        .find_map(|c| match c {
            DrawCommand::LinkArea { rect, target } if target == "/open" => Some(*rect),
            _ => None,
        })
        .expect("row-wide link area");
    assert_eq!(link.x, 0.0);
    assert_eq!(link.w, 400.0, "the area is the row, not the label");
    // The button's area precedes the row's in the list = wins hit-testing.
    let btn_pos = cmds.iter().position(|c| matches!(c, DrawCommand::ActionArea { .. })).unwrap();
    let row_pos = cmds
        .iter()
        .position(|c| matches!(c, DrawCommand::LinkArea { target, .. } if target == "/open"))
        .unwrap();
    assert!(btn_pos < row_pos, "child controls stay clickable inside the row");
}

/// A key node is an affordance with no body: it emits its binding and takes
/// no space — the column around it lays out as if it weren't there.
#[test]
fn key_bindings_emit_and_take_no_space() {
    let bare = r##"
        column padding=0 gap=10 { text "a"; text "b" }
    "##;
    let with_keys = r##"
        column padding=0 gap=10 {
            key "down" { submit "/nav/next" }
            text "a"
            key "enter" target="/open"
            text "b"
        }
    "##;
    let (plain, plain_h) = commands(bare, 400.0);
    let (keyed, keyed_h) = commands(with_keys, 400.0);
    assert_eq!(plain_h, keyed_h, "bindings charge no height and no gap");
    let binds: Vec<_> = keyed
        .iter()
        .filter_map(|c| match c {
            DrawCommand::KeyBind { key, target, action } => {
                Some((key.clone(), target.clone(), action.is_some()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(binds.len(), 2);
    assert_eq!(binds[0], ("down".into(), None, true));
    assert_eq!(binds[1], ("enter".into(), Some("/open".into()), false));
    assert_eq!(
        keyed.len(),
        plain.len() + 2,
        "the only difference in the frame is the two bindings"
    );
}

/// Rounded clips ride the wire: radius 0 keeps the original tag byte (old
/// recordings decode unchanged), radius > 0 takes the new tag and
/// round-trips.
#[test]
fn rounded_clips_roundtrip_and_square_stays_legacy() {
    use rill_ui::stream::{decode, encode};
    let rounded = vec![
        DrawCommand::PushClip {
            rect: rill_ui::Rect { x: 1.0, y: 2.0, w: 30.0, h: 20.0 },
            radius: 7.5,
        },
        DrawCommand::PopClip,
    ];
    let bytes = encode(&rounded).unwrap();
    assert_eq!(decode(&bytes).unwrap(), rounded);

    let square = vec![
        DrawCommand::PushClip {
            rect: rill_ui::Rect { x: 1.0, y: 2.0, w: 30.0, h: 20.0 },
            radius: 0.0,
        },
        DrawCommand::PopClip,
    ];
    let bytes = encode(&square).unwrap();
    assert_eq!(bytes[8], 5, "square clip keeps the pre-radius tag byte");
    assert_eq!(decode(&bytes).unwrap(), square);
}

/// A container's menu covers the whole container, and a child's menu area
/// precedes its parent's in the list — first hit under a point is the
/// innermost element's menu.
#[test]
fn menu_areas_cover_containers_innermost_first() {
    let src = r##"
        column target="/outer" padding=0 gap=0 {
            menu { item "Outer" target="/outer" }
            row target="/inner" padding=0 gap=0 {
                menu { item "Inner" target="/inner" }
                text "row"
            }
            text "below"
        }
    "##;
    let (cmds, _) = commands(src, 400.0);
    let areas: Vec<(String, f32)> = cmds
        .iter()
        .filter_map(|c| match c {
            DrawCommand::MenuArea { rect, items } => {
                Some((items[0].label.clone(), rect.w))
            }
            _ => None,
        })
        .collect();
    assert_eq!(areas.len(), 2);
    assert_eq!(areas[0].0, "Inner", "child's area comes first = wins hit-test");
    assert_eq!(areas[1].0, "Outer");
    assert_eq!(areas[0].1, 400.0, "the area is the whole container");
}

/// Menu areas ride the wire with their items intact.
#[test]
fn menu_areas_roundtrip_the_stream() {
    use rill_ui::stream::{decode, encode};
    use rill_ui::{MenuItem, UiAction};
    let cmds = vec![DrawCommand::MenuArea {
        rect: rill_ui::Rect { x: 1.0, y: 2.0, w: 300.0, h: 30.0 },
        items: vec![
            MenuItem {
                label: "Open".into(),
                icon: None,
                target: Some("/open".into()),
                action: None,
                danger: false,
                separator: false,
            },
            MenuItem {
                label: "".into(),
                icon: None,
                target: None,
                action: None,
                danger: false,
                separator: true,
            },
            MenuItem {
                label: "Delete".into(),
                icon: Some("trash".into()),
                target: None,
                action: Some(UiAction::OpenMenu),
                danger: true,
                separator: false,
            },
        ],
    }];
    let bytes = encode(&cmds).unwrap();
    assert_eq!(decode(&bytes).unwrap(), cmds);
}

// ---------------------------------------------------------------- image sizing

/// An [`rill_ui::ImageSizer`] that knows every picture is 1600x1200 — a phone
/// photo, the shape most of these tests care about.
struct PhotoSizer;

impl rill_ui::ImageSizer for PhotoSizer {
    fn natural_size(&mut self, _source: &str) -> Option<(f32, f32)> {
        Some((1600.0, 1200.0))
    }
}

fn commands_with_photos(src: &str, width: f32) -> Vec<DrawCommand> {
    let compiled = compile(src).unwrap();
    let doc = rill_doc::decode(&compiled.bytes).unwrap();
    let tree = resolve(&doc, Defaults::default());
    layout_document(
        &tree,
        LayoutOptions { viewport_width: width, viewport_height: None },
        &mut MockText,
        &mut PhotoSizer,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    )
    .0
}

fn image_rects(cmds: &[DrawCommand]) -> Vec<Rect> {
    cmds.iter()
        .filter_map(|c| match c {
            DrawCommand::Image { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect()
}

/// A style sizes the box; the picture sits inside it at its own shape.
///
/// This is the gap that blocked thumbnail grids: an image took its natural
/// size clamped to the available width, so the only way to get a small
/// picture was to build a narrow frame around it. A 4:3 photograph in a
/// square slot letterboxes — centred, aspect intact — rather than squashing,
/// because a distorted photograph is a bug you can see from across the room.
#[test]
fn a_style_sizes_an_image_and_the_picture_keeps_its_shape() {
    let src = r#"
style "thumb" width=200 height=200
column padding=0 gap=0 { image "/p.jpg" style="thumb" }
"#;
    let rects = image_rects(&commands_with_photos(src, 800.0));
    assert_eq!(rects.len(), 1);
    let r = rects[0];
    // Contained in a 200x200 box: 1600x1200 scales to 200x150, centred.
    assert_eq!((r.w, r.h), (200.0, 150.0), "aspect was not preserved");
    assert_eq!(r.y, 25.0, "not centred in the declared box ({})", r.y);
    let ratio = r.w / r.h;
    assert!((ratio - 4.0 / 3.0).abs() < 0.01, "squashed to {ratio:.2}");
}

/// One declared axis, the other follows the picture.
#[test]
fn width_alone_follows_the_aspect() {
    let src = r#"
style "half" width=400
column { image "/p.jpg" style="half" }
"#;
    let rects = image_rects(&commands_with_photos(src, 800.0));
    assert_eq!((rects[0].w, rects[0].h), (400.0, 300.0));
}

/// A declared box is the same box before and after the picture loads.
///
/// The old behaviour laid an unloaded image out at a placeholder size and
/// reflowed the page when the real size arrived. A declared box makes the
/// reflow zero: the reader's scroll position survives the pictures loading,
/// which for a long gallery is the difference between a page and a slot
/// machine.
#[test]
fn a_declared_box_does_not_reflow_when_the_picture_arrives() {
    let src = r#"
style "thumb" width=200 height=200
column { image "/p.jpg" style="thumb" ; text "caption" }
"#;
    // Before: nothing knows the picture's size.
    let (before, _) = commands(src, 800.0);
    // After: the picture is a 1600x1200 photo.
    let after = commands_with_photos(src, 800.0);
    let text_y = |cmds: &[DrawCommand]| {
        cmds.iter()
            .find_map(|c| match c {
                DrawCommand::Text { rect, .. } => Some(rect.y),
                _ => None,
            })
            .expect("caption laid out")
    };
    assert_eq!(
        text_y(&before),
        text_y(&after),
        "the caption moved when the picture arrived — the declared box did not hold"
    );
}

/// Sized images in a wrapping row are a grid — the case that motivated this.
#[test]
fn a_wrapping_row_of_sized_images_is_a_grid() {
    let mut src = String::from(
        "style \"thumb\" width=240 height=180\nstyle \"grid\" wrap=#true\nrow style=\"grid\" gap=10 {\n",
    );
    for i in 0..6 {
        src.push_str(&format!("\timage \"/p{i}.jpg\" style=\"thumb\"\n"));
    }
    src.push_str("}\n");
    let rects = image_rects(&commands_with_photos(&src, 800.0));
    assert_eq!(rects.len(), 6);
    let rows: std::collections::BTreeSet<i32> = rects.iter().map(|r| r.y as i32).collect();
    assert!(
        rows.len() >= 2,
        "six 240px thumbnails in an 800px row stayed on one line — the row did not wrap"
    );
    // Every drawn picture is thumbnail-sized, which is what keeps the frame's
    // wanted sizes (and so the pixels sent) small.
    for r in &rects {
        // Half a pixel of float noise is not a size violation.
        assert!(r.w <= 240.5 && r.h <= 180.5, "a thumbnail drew at {}x{}", r.w, r.h);
    }
}

// ---------------------------------------------------------------- tier

/// `sensitive tier=N` reaches the tree, where the host reads it to classify
/// the frames it attaches (specs/history.md decision 4). Two declarations
/// compose by ratchet — raising is the only move in the vocabulary.
#[test]
fn a_document_declares_its_tier_and_two_claims_ratchet() {
    let one = compile("column { text \"hello\" }").unwrap();
    let tree = resolve(&rill_doc::decode(&one.bytes).unwrap(), Defaults::default());
    assert_eq!(tree.tier, 0, "an undeclared page is routine");

    let sealed = compile("column { text \"phrase\"; sensitive tier=2 }").unwrap();
    let tree = resolve(&rill_doc::decode(&sealed.bytes).unwrap(), Defaults::default());
    assert_eq!(tree.tier, 2);

    let both = compile(
        "column { sensitive tier=2; row { text \"x\"; sensitive tier=1 } }",
    )
    .unwrap();
    let tree = resolve(&rill_doc::decode(&both.bytes).unwrap(), Defaults::default());
    assert_eq!(tree.tier, 2, "the higher claim wins whatever the order");
}
