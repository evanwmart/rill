//! What the client *holds and sends* as a document gets longer than its window.
//!
//! Run with `cargo test -p rill-viewport --test image_residency -- --ignored --nocapture`.
//!
//! Culling made the frame flat in document length: paint commands outside the
//! window are not emitted. This asks the same question one level down, about
//! the pixels the frame's names refer to. A frame that draws four pictures
//! still arrives at a client holding two hundred, and `ReadyImages` — the list
//! a forwarding host walks to decide what to send — is built from residency
//! rather than from the visible set.

use rill_ui::{DrawCommand, LineMetrics, Rect, TextMeasurer};
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

/// One picture per source, each distinct so nothing dedupes: a photo roll, a
/// chat log with attachments, a directory of thumbnails.
const TILE: (u32, u32) = (256, 192);

/// A photo, in the sense that matters here: larger than the window is wide, so
/// the display-size rule keeps a full-width copy of it rather than a thumbnail.
const PHOTO: (u32, u32) = (1600, 1200);

fn write_tiles_sized(dir: &std::path::Path, n: usize, (w, h): (u32, u32)) {
    for i in 0..n {
        let mut img = image::RgbaImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, (i % 256) as u8, 255]);
        }
        img.save(dir.join(format!("tile-{i:04}.png"))).expect("write png");
    }
}

fn write_tiles(dir: &std::path::Path, n: usize) {
    write_tiles_sized(dir, n, TILE);
}

fn roll(n: usize) -> String {
    let mut s = String::from("column gap=8 padding=16 {\n");
    for i in 0..n {
        s.push_str(&format!("\timage \"/tile-{i:04}.png\"\n"));
    }
    s.push_str("}\n");
    s
}

struct Measured {
    /// Decoded pixels the client is holding, all of them — the visible ones at
    /// the size they are shown and the rest at whatever floor they were
    /// reduced to. This is client RAM.
    held_bytes: usize,
    /// The high-water mark on the way there, while the page was loading.
    peak_bytes: usize,
    /// Pictures the frame offers the host, and their bytes — what a forwarding
    /// host sends onward, and what the compositor then holds.
    offered: usize,
    offered_bytes: usize,
    /// Pictures the frame actually draws, after culling.
    drawn: usize,
    /// The frame itself, for scale.
    frame_bytes: usize,
}

fn measure(dir: &std::path::Path, n: usize, viewport: Rect) -> Measured {
    let bytes = rill_doc::compile(&roll(n)).expect("compiles").bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();
    measure_page(dir, viewport)
}

/// Measure whatever `page.rill` already says — pages that are not a roll.
fn measure_page(dir: &std::path::Path, viewport: Rect) -> Measured {
    let fetcher = Fetcher::new(dir.to_path_buf(), None, dir.to_path_buf()).expect("fetcher");
    let mut view = AppView::new(
        fetcher,
        Source::Local { dir: dir.to_path_buf(), path: "/page.rill".into() },
    );

    // Settle: the page is one fetch, every image another, decoding and scaling
    // a worker thread again, and layout is what discovers all of it — so poll
    // and lay out together until nothing is outstanding. Without the sleep the
    // worker threads never get to run and a longer document simply never
    // finishes loading, which silently measures an empty page.
    //
    // Quiet has to hold for several rounds rather than one: a layout is what
    // *asks* for a rescale, so the poll before it can report nothing
    // outstanding while the layout right after queues a job. Stopping there
    // measures a page mid-preparation, with pictures still at the size they
    // had before the frame that wanted a different one.
    let mut last = None;
    let mut quiet = 0;
    let mut peak = 0;
    for _ in 0..8000 {
        let polled = view.poll();
        let out = view.layout(viewport, &mut FixedMeasurer);
        last = Some(out);
        // The high-water mark on the way to the settled figure. Decoding and
        // reduction happen on one worker thread, so a page whose pictures all
        // arrive together drains rather than collapsing at once — and the
        // depth of that queue is a number a 1 GiB machine cares about.
        peak = peak.max(view.image_bytes_held());
        if polled.pending || view.is_loading() {
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= 5 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let (cmds, ready, _hint) = last.expect("at least one layout");

    let mut offered = 0;
    let mut offered_bytes = 0;
    for (_source, image) in ready.iter() {
        offered += 1;
        offered_bytes += image.rgba.len();
    }

    let drawn = cmds
        .iter()
        .filter(|c| matches!(c, DrawCommand::Image { .. }))
        .count();

    Measured {
        held_bytes: view.image_bytes_held(),
        peak_bytes: peak,
        offered,
        offered_bytes,
        drawn,
        frame_bytes: rill_ui::stream::encode(&cmds).expect("encodes").len(),
    }
}

fn mib(n: usize) -> String {
    format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
}

fn kib(n: usize) -> String {
    format!("{:.1} KiB", n as f64 / 1024.0)
}

/// What a flick through a long roll costs.
///
/// The floor is what makes releasing safe, and the refetch is what it costs: a
/// picture coming back into the window has to read its source again to sharpen.
/// A scroll that passes over a hundred pictures on its way somewhere could
/// therefore pay for a hundred of them, none of which anybody looked at.
#[test]
#[ignore]
fn what_a_fast_scroll_costs() {
    let dir = std::env::temp_dir().join(format!("viewport-thrash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let n = 40;
    write_tiles_sized(&dir, n, (512, 384));

    let bytes = rill_doc::compile(&roll(n)).expect("compiles").bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();
    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });
    let window = Rect { x: 0.0, y: 0.0, w: 1280.0, h: 800.0 };

    let settle = |view: &mut AppView, steps: usize| {
        for _ in 0..steps {
            view.poll();
            view.layout(window, &mut FixedMeasurer);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    settle(&mut view, 600);
    let loaded = view.image_refetches();

    // A flick: wheel notches arriving faster than the easing settles, all the
    // way down and all the way back. Two polls per notch is a scroll that never
    // stops moving, which is the case that could thrash.
    let notches = 30;
    for _ in 0..notches {
        view.scroll_by(-900.0);
        settle(&mut view, 2);
    }
    let down = view.image_refetches();
    for _ in 0..notches {
        view.scroll_by(900.0);
        settle(&mut view, 2);
    }
    let up = view.image_refetches();
    // And then it stops, which is when the picture under the reader's eyes has
    // to be sharp.
    settle(&mut view, 400);
    let rested = view.image_refetches();

    println!("\n=== a flick through {n} pictures, {notches} notches each way ===");
    println!("  {:<28} {}", "refetches while loading", loaded);
    println!("  {:<28} {}", "flicking down", down - loaded);
    println!("  {:<28} {}", "flicking back up", up - down);
    println!("  {:<28} {}", "after it came to rest", rested - up);
    println!(
        "  {:<28} {}  (one per picture, each way)\n",
        "a refetch per picture would be",
        n * 2
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn what_a_roll_of_pictures_keeps_resident() {
    let dir = std::env::temp_dir().join(format!("viewport-residency-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let counts = [4usize, 20, 100];
    write_tiles(&dir, *counts.iter().max().unwrap());

    let viewport = Rect { x: 0.0, y: 0.0, w: 1280.0, h: 800.0 };
    let header = || {
        println!(
            "  {:>7}  {:>11}  {:>11}  {:>7}  {:>11}  {:>7}  {:>10}",
            "in doc", "held (RAM)", "peak", "offered", "sent", "drawn", "frame"
        );
    };
    let row = |n: usize, m: &Measured| {
        println!(
            "  {:>7}  {:>11}  {:>11}  {:>7}  {:>11}  {:>7}  {:>10}",
            n,
            mib(m.held_bytes),
            mib(m.peak_bytes),
            m.offered,
            mib(m.offered_bytes),
            m.drawn,
            kib(m.frame_bytes),
        );
    };

    println!("\n=== a roll of {}x{} pictures, 1280x800 window ===", TILE.0, TILE.1);
    header();
    for n in counts {
        row(n, &measure(&dir, n, viewport));
    }
    println!();

    // The same shape with pictures the size people actually have. A photo
    // wider than the window keeps a full-width copy, which is where the slope
    // stops being an abstraction.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let photo_counts = [4usize, 12, 24];
    write_tiles_sized(&dir, *photo_counts.iter().max().unwrap(), PHOTO);
    println!("=== the same roll, {}x{} photos ===", PHOTO.0, PHOTO.1);
    header();
    let mut first = 0usize;
    let mut last = 0usize;
    for (i, n) in photo_counts.iter().enumerate() {
        let m = measure(&dir, *n, viewport);
        if i == 0 {
            first = m.held_bytes;
        }
        last = m.held_bytes;
        row(*n, &m);
    }
    // The slope is the claim, not the intercept: what does the sixth time as
    // many photos cost?
    println!(
        "\n  {} photos held {}; {} held {} — {:+.1} MiB for {}x the document.\n",
        photo_counts[0],
        mib(first),
        photo_counts[photo_counts.len() - 1],
        mib(last),
        (last as f64 - first as f64) / (1024.0 * 1024.0),
        photo_counts[photo_counts.len() - 1] / photo_counts[0],
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The same pictures as a thumbnail grid — what the style-sized box buys.
///
/// A grid page declares every slot 240x180, so the display-size rule keeps a
/// ~470 KB reduction per visible thumbnail instead of the ~7 MB a full-width
/// photograph costs. This is the gallery case the media plan is really about,
/// and it was not expressible before styles could size an image's box.
#[test]
#[ignore]
fn what_a_grid_of_the_same_pictures_costs() {
    let dir = std::env::temp_dir().join(format!("viewport-grid-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let n = 60;
    write_tiles_sized(&dir, n, PHOTO);

    let grid = {
        let mut s = String::from(
            "style \"thumb\" width=240 height=180\nstyle \"grid\" wrap=#true\n\
             column gap=16 padding=24 {\n\trow style=\"grid\" gap=12 {\n",
        );
        for i in 0..n {
            s.push_str(&format!("\t\timage \"/tile-{i:04}.png\" style=\"thumb\"\n"));
        }
        s.push_str("\t}\n}\n");
        s
    };

    let viewport = Rect { x: 0.0, y: 0.0, w: 1280.0, h: 800.0 };
    println!("\n=== {n} {}x{} photos, roll vs 240x180 grid, 1280x800 ===", PHOTO.0, PHOTO.1);
    println!(
        "  {:<6} {:>11}  {:>11}  {:>7}  {:>11}  {:>10}",
        "", "held (RAM)", "peak", "offered", "sent", "frame"
    );
    for (label, page) in [("roll", roll(n)), ("grid", grid)] {
        let bytes = rill_doc::compile(&page).expect("compiles").bytes;
        std::fs::write(dir.join("page.rill"), &bytes).unwrap();
        let m = measure_page(&dir, viewport);
        println!(
            "  {:<6} {:>11}  {:>11}  {:>7}  {:>11}  {:>10}",
            label,
            mib(m.held_bytes),
            mib(m.peak_bytes),
            m.offered,
            mib(m.offered_bytes),
            kib(m.frame_bytes),
        );
    }
    println!();

    let _ = std::fs::remove_dir_all(&dir);
}
