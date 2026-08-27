//! Lay out a compiled document with the *real* text engine and print what
//! came out — resolved family, font size, and the rect of every text run.
//!
//! ```bash
//! cargo run -p rill-gpu --example page-probe -- page.rill [WxH]
//! ```
//!
//! The viewport tests use a fixed measurer, which is right for testing layout
//! logic and useless for questions like "why is this line 44 pixels tall" or
//! "did `font=\"mono\"` actually reach a mono face". Those need the engine
//! that ships.

use rill_gpu::text::{EngineMeasurer, TextEngine};
use rill_ui::{DrawCommand, TextMeasurer};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("a compiled .rill document");
    let size = args.next().unwrap_or_else(|| "900x700".into());
    let (w, h) = size.split_once('x').expect("WxH");
    let (w, h): (f32, f32) = (w.parse().unwrap(), h.parse().unwrap());

    // `--fonts` answers the other half of the question: what a family token
    // actually resolved to, and whether it advances like a mono face.
    if path == "--fonts" {
        let engine = TextEngine::new();
        let mut m = EngineMeasurer(&engine);
        // Whitespace is the thing a terminal cannot get wrong: every run of
        // spaces has to be exactly its character count wide, wherever it is.
        let cell = m.measure("x", 14.0, 400, "mono", f32::MAX).width;
        for probe in ["x", "xx", " x", "  x", "        ", " ", "x ", "x  ", "a b"] {
            let w = m.measure(probe, 14.0, 400, "mono", f32::MAX).width;
            println!(
                "  {probe:?}: {w:7.2} = {:5.2} cells (want {})",
                w / cell,
                probe.chars().count()
            );
        }
        for (family, weight) in [("mono", 400u16), ("mono", 200), ("mono", 700), ("DejaVu Sans Mono", 400)] {
            let narrow = m.measure("iiiiiiiiii", 14.0, weight, family, f32::MAX).width;
            let wide = m.measure("WWWWWWWWWW", 14.0, weight, family, f32::MAX).width;
            println!(
                "family={family:?} weight={weight}: i×10={narrow:.1} W×10={wide:.1} {}",
                if (narrow - wide).abs() < 0.5 { "MONO" } else { "proportional" }
            );
        }
        return;
    }

    let bytes = std::fs::read(&path).expect("read document");
    let doc = rill_doc::decode(&bytes).expect("decode");
    let tree = rill_ui::resolve(&doc, rill_ui::Defaults::default());

    let engine = TextEngine::new();
    let mut measurer = EngineMeasurer(&engine);
    let (commands, total) = rill_ui::layout_document(
        &tree,
        rill_ui::LayoutOptions { viewport_width: w, viewport_height: Some(h) },
        &mut measurer,
        &mut rill_ui::NoImages,
        &[],
        None,
        0,
        (0, 0),
        None,
        false,
    );
    println!("content height {total:.1}");

    // An optional third argument renders the page, because "why does this
    // look wrong" is not a question a list of rectangles answers.
    if let Some(out) = args.next() {
        use rill_gpu::{Renderer, SceneLayer};
        use rill_ui::Color;
        let r = Renderer::new_headless().expect("wgpu adapter");
        let target = r.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("page"),
            size: wgpu::Extent3d {
                width: w as u32,
                height: h as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        r.composite_scene(
            &view,
            w as u32,
            h as u32,
            // A deliberately non-black ground: anything the page does not
            // paint shows up as this, which is how you tell "transparent"
            // from "painted black".
            Color { r: 40, g: 30, b: 60, a: 255 },
            &[SceneLayer::commands(&commands)],
            Default::default(),
        );
        let rgba = r.read_texture_rgba(&target, w as u32, h as u32);
        let mut ppm = format!("P6\n{} {}\n255\n", w as u32, h as u32).into_bytes();
        for px in rgba.as_chunks::<4>().0 {
            ppm.extend_from_slice(&px[..3]);
        }
        std::fs::write(&out, ppm).expect("write ppm");
        println!("wrote {out}");
    }

    println!("{} commands", commands.len());
    // Fills first: "what is painting behind this" is answered by rects and
    // backdrops, not by the text on top of them.
    for c in &commands {
        match c {
            DrawCommand::Rect { rect, color, .. } => println!(
                "  RECT   {:6.1},{:6.1} {:7.1}x{:7.1}  rgba({},{},{},{})",
                rect.x, rect.y, rect.w, rect.h, color.r, color.g, color.b, color.a
            ),
            DrawCommand::Backdrop { rect, .. } => println!(
                "  BACKDROP {:6.1},{:6.1} {:7.1}x{:7.1}",
                rect.x, rect.y, rect.w, rect.h
            ),
            _ => {}
        }
    }
    let mut shown = 0;
    for c in &commands {
        if let DrawCommand::Text { rect, text, font_size, font_family, .. } = c {
            println!(
                "  y={:7.2} h={:6.2} size={font_size:5.1} family={font_family:?} {:?}",
                rect.y,
                rect.h,
                text.chars().take(28).collect::<String>()
            );
            shown += 1;
            if shown >= 12 {
                break;
            }
        }
    }
}
