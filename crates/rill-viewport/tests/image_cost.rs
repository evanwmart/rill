//! What a page of pictures actually costs, in bytes.
//!
//! Run with `cargo test -p rill-viewport --test image_cost -- --ignored --nocapture`.
//! Ignored because it is a measurement, not an assertion: it prints a table
//! and checks only the one property the design rests on — that the per-frame
//! cost of a page does not grow with the pictures on it.

use rill_ui::{LineMetrics, Rect, TextMeasurer};
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

/// A gallery: a hero image, then a grid of captioned thumbnails.
fn gallery(thumbs: usize) -> String {
    let mut s = String::from(
        "style \"cap\" size=12 color=\"#8a8a99\"\n\
         style \"title\" size=22 weight=\"bold\"\n\
         column gap=12 padding=16 {\n\
         \ttext \"Photos\" style=\"title\"\n\
         \timage \"/photos/hero.jpg\"\n\
         \trow gap=8 {\n",
    );
    for i in 0..thumbs {
        s.push_str(&format!(
            "\t\tcolumn gap=4 {{ image \"/photos/thumb-{i:03}.jpg\"; \
             text \"IMG_{i:04}.jpg\" style=\"cap\" }}\n"
        ));
    }
    s.push_str("\t}\n}\n");
    s
}

fn frame_bytes(source: &str) -> usize {
    let dir = std::env::temp_dir().join(format!("image-cost-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let bytes = rill_doc::compile(source).expect("compiles").bytes;
    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Generated { label: "gallery".into(), bytes });
    for _ in 0..200 {
        view.poll();
        if !view.is_loading() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (cmds, _images, _hint) =
        view.layout(Rect { x: 0.0, y: 0.0, w: 1280.0, h: 800.0 }, &mut FixedMeasurer);
    let encoded = rill_ui::stream::encode(&cmds).expect("encodes");
    let _ = std::fs::remove_dir_all(&dir);
    encoded.len()
}

fn kib(n: usize) -> String {
    format!("{:.1} KiB", n as f64 / 1024.0)
}

fn mib(n: usize) -> String {
    format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
}

#[test]
#[ignore]
fn what_a_page_of_pictures_costs() {
    // The text-only page is the control: same layout, no images at all.
    let text_only = gallery(0).replace("\timage \"/photos/hero.jpg\"\n", "");
    let control = frame_bytes(&text_only);

    println!("\n=== per-frame wire cost (the command stream) ===");
    println!("  {:<28} {}", "text-only control", kib(control));
    let mut last = 0;
    for thumbs in [0usize, 12, 48] {
        let n = frame_bytes(&gallery(thumbs));
        println!("  {:<28} {}", format!("gallery, {thumbs} thumbnails"), kib(n));
        last = n;
    }

    // One-time image payload: raw RGBA, which is what attach_image carries.
    // Two columns, because what a picture *is* and what gets sent are no
    // longer the same number: images are reduced to about the size they are
    // drawn at before they leave.
    println!("\n=== one-time image payload (raw RGBA, sent once per source) ===");
    let rgba = |w: usize, h: usize| w * h * 4;
    println!("  {:<24} {:>12}  {:>12}", "", "at source", "as sent");
    // A phone photo behind a 1248-wide hero slot: 4000x3000 reduced by 2.
    println!(
        "  {:<24} {:>12}  {:>12}",
        "hero, phone photo",
        mib(rgba(4000, 3000)),
        mib(rgba(2000, 1500))
    );
    // The same photo behind a 240px thumbnail: reduced by 16.
    println!(
        "  {:<24} {:>12}  {:>12}",
        "thumbnail, same photo",
        mib(rgba(4000, 3000)),
        kib(rgba(250, 188))
    );
    for thumbs in [12usize, 48] {
        let before = rgba(4000, 3000) * (thumbs + 1);
        let after = rgba(2000, 1500) + thumbs * rgba(250, 188);
        println!(
            "  {:<24} {:>12}  {:>12}",
            format!("hero + {thumbs} thumbs"),
            mib(before),
            mib(after)
        );
    }

    // What the same window would cost as pixels, every frame.
    println!("\n=== the alternative: a pixel window at 1280x800 ===");
    let pixel_frame = 1280 * 800 * 4;
    println!("  {:<28} {}  per frame", "pixel buffer", mib(pixel_frame));
    println!(
        "  {:<28} {:.0}x the vector frame",
        "ratio",
        pixel_frame as f64 / last.max(1) as f64
    );
    println!(
        "  {:<28} {}  per second",
        "at 60fps",
        mib(pixel_frame * 60)
    );

    println!("\n=== steady state: scrolling the 48-thumb gallery for 1s at 60fps ===");
    println!("  {:<28} {}", "vector (frames only)", mib(last * 60));
    println!("  {:<28} {}", "pixel window", mib(pixel_frame * 60));
    println!();

    // The property, not the numbers: pictures do not enter the per-frame cost.
    // A frame names an image in a handful of bytes, so 48 of them must not
    // approach even one thumbnail's worth of pixels.
    assert!(
        last < rgba(240, 160),
        "a 48-image frame ({}) has grown past a single thumbnail's pixels ({}) \
         — images are riding the stream again",
        kib(last),
        kib(rgba(240, 160))
    );
}
