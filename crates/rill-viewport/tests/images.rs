//! Images, from a document that names one to pixels a host can hand onward.
//!
//! This is the client half of image transport. A `.rill` document refers to
//! an image by path and never carries it; the viewport fetches it, decodes it,
//! and returns the pixels alongside the draw commands so the host can deliver
//! them (for the vector client, over `rill_stream_v1::attach_image`).
//!
//! It is tested because for a long time it silently did not matter: the
//! viewport did all of this and every host dropped the result on the floor, so
//! document images painted as placeholder boxes everywhere while the fetch and
//! decode were paid for on every page.

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

/// Poll and lay out until nothing is outstanding.
///
/// Decoding and rescaling run on a worker thread, so a picture is not finished
/// being prepared when `layout` returns — it is finished when `poll` stops
/// reporting work in flight. Judging the pixels straight after a layout reads a
/// picture mid-preparation, which is what these tests used to do back when
/// layout did the scaling itself and blocked on it.
/// Quiet has to be observed more than once. A layout is what *asks* for a
/// rescale, so the poll before it can report nothing outstanding while the
/// layout immediately after queues a job — stopping there reads the picture at
/// the size it had before the frame that wanted a different one.
fn settle_quiet(view: &mut AppView, window: Rect) {
    let mut quiet = 0;
    for _ in 0..2000 {
        let polled = view.poll();
        view.layout(window, &mut FixedMeasurer);
        // `changed` covers the glide: a scroll eases toward its target against
        // the clock, and a picture is only sharpened once the view has stopped
        // travelling — so a helper that waited on outstanding *work* alone
        // would return mid-glide, before anything had asked for detail.
        if polled.changed || polled.pending || view.is_loading() {
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= 3 {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("the view never settled");
}

/// A 4x3 PNG with a known first pixel, written where a `Local` source serves.
fn write_png(dir: &std::path::Path, name: &str) -> (u32, u32) {
    let (w, h) = (4u32, 3u32);
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba([(x * 60) as u8, (y * 80) as u8, 200, 255]);
    }
    img.save(dir.join(name)).expect("write png");
    (w, h)
}

#[test]
fn a_document_image_arrives_as_pixels_a_host_can_send() {
    let dir = std::env::temp_dir().join(format!("viewport-images-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (w, h) = write_png(&dir, "moon.png");

    let source = r#"column { image "/moon.png" }"#;
    let bytes = rill_doc::compile(source).expect("compiles").bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });

    // The image is a second fetch, issued once the page resolves — so settle
    // both, laying out each time (the layout is what discovers the source).
    let mut images = rill_viewport::ReadyImages::empty();
    for _ in 0..400 {
        view.poll();
        let (_cmds, ready, _hint) =
            view.layout(Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, &mut FixedMeasurer);
        if ready.iter().next().is_some() {
            images = ready;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let decoded = images.image("/moon.png").expect("the image reached the host as pixels");
    assert_eq!((decoded.width, decoded.height), (w, h), "natural size survived the round trip");
    let raw: &[u8] = &decoded.rgba;
    assert_eq!(
        raw.len(),
        (w * h * 4) as usize,
        "tightly packed RGBA — what attach_image requires"
    );
    assert_eq!(&raw[..4], &[0, 0, 200, 255], "the pixels are the image's own");

    // And it is enumerable, which is how a host learns what to send: painting
    // hosts look sources up, but a host that forwards its window has to ask
    // the other way round.
    let listed: Vec<&str> = images.iter().map(|(s, _)| s).collect();
    assert_eq!(listed, vec!["/moon.png"]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A photo is shipped at about the size it is shown at, not the size it was
/// taken at.
///
/// This is the multiplier that matters: a phone photo is 4000x3000 — 48 MB of
/// pixels — and a thumbnail of it occupies a couple of hundred pixels on
/// screen. Sending the original to fill that costs three hundred times the
/// bytes for a result nobody can tell apart.
#[test]
fn a_large_photo_is_reduced_to_the_size_it_is_shown_at() {
    let dir = std::env::temp_dir().join(format!("viewport-downscale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A "photo": far larger than any box on the page will be.
    let (w, h) = (2048u32, 1536u32);
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
    }
    img.save(dir.join("photo.png")).unwrap();

    // Shown in a box a fraction of its size. The box comes from the frame:
    // an `image` takes its natural size clamped to the width available, so a
    // narrow viewport is how a large photo ends up small.
    let source = r#"column { image "/photo.png" }"#;
    let bytes = rill_doc::compile(source).expect("compiles").bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });

    let window = Rect { x: 0.0, y: 0.0, w: 160.0, h: 120.0 };
    settle_quiet(&mut view, window);
    let (_c, ready, _h) = view.layout(window, &mut FixedMeasurer);
    let (sw, sh) = ready.image("/photo.png").map(|i| (i.width, i.height))
        .expect("the image reached the host");
    let native_bytes = (w as usize) * (h as usize) * 4;
    let sent_bytes = (sw as usize) * (sh as usize) * 4;
    println!(
        "  native {w}x{h} = {:.1} MiB -> sent {sw}x{sh} = {:.1} KiB  ({:.0}x smaller)",
        native_bytes as f64 / (1024.0 * 1024.0),
        sent_bytes as f64 / 1024.0,
        native_bytes as f64 / sent_bytes as f64
    );

    assert!(sw < w && sh < h, "the photo went out at full size ({sw}x{sh})");
    // Still covers the box it is drawn in — reduced, not degraded.
    assert!(sw >= 160, "reduced past the box it has to fill ({sw}x{sh})");
    // Aspect ratio survives, or the picture would be visibly squashed.
    let native_ratio = w as f64 / h as f64;
    let sent_ratio = sw as f64 / sh as f64;
    assert!(
        (native_ratio - sent_ratio).abs() < 0.02,
        "aspect ratio drifted: {native_ratio:.3} vs {sent_ratio:.3}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The client does not keep the original after it has the version it sends.
///
/// It holds the compressed source on disk already, and layout only ever asks
/// how big the picture is — two integers. Keeping 46 MiB of decoded pixels to
/// answer that, next to a compositor holding the reduced copy, is the same
/// image resident twice at full size on a machine with a gigabyte.
///
/// Checked through the public surface: `natural_size` must keep reporting the
/// image's own size long after the pixels behind it are gone, because layout
/// depends on it and it is the thing that could plausibly break.
#[test]
fn the_original_is_not_kept_once_the_reduced_copy_exists() {
    let dir = std::env::temp_dir().join(format!("viewport-residency-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let (w, h) = (2048u32, 1536u32);
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, 64, 255]);
    }
    img.save(dir.join("big.png")).unwrap();

    let source = r#"column { image "/big.png" }"#;
    let bytes = rill_doc::compile(source).expect("compiles").bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });

    let small = Rect { x: 0.0, y: 0.0, w: 160.0, h: 120.0 };
    settle_quiet(&mut view, small);
    let (_c, ready, _h) = view.layout(small, &mut FixedMeasurer);
    let (hw, hh) = ready.image("/big.png").map(|i| (i.width, i.height))
        .expect("the image reached the host");
    assert!(hw < w, "still holding the original ({hw}x{hh} of {w}x{h})");

    // The layout still lays it out at the picture's own size: the box comes
    // from the natural dimensions, which outlive the pixels. If those were
    // lost with the original, the image would suddenly lay out small and the
    // page would reflow underneath the reader.
    let (cmds, _r, _h) = view.layout(small, &mut FixedMeasurer);
    let drawn = cmds.iter().find_map(|c| match c {
        rill_ui::DrawCommand::Image { rect, source } if source == "/big.png" => Some(*rect),
        _ => None,
    });
    let rect = drawn.expect("the image is still drawn");
    // 2048x1536 is 4:3; clamped to a 160-wide frame that is 160x120.
    assert!(
        (rect.w / rect.h - w as f32 / h as f32).abs() < 0.01,
        "laid out at the wrong shape ({}x{}) — natural size was lost with the pixels",
        rect.w,
        rect.h
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A picture the window is not showing is not held at the size it would be
/// shown at — and coming back to it neither leaves a hole nor leaves it blurry.
///
/// This is culling one level below the frame. The frame already stops
/// describing what is off screen; the pixels behind those names were still
/// resident, so a long roll of photographs cost its whole length in RAM while
/// showing two of them. What is off screen drops to a coarse floor instead of
/// vanishing, because a scroll that arrives at an empty box and waits for a
/// fetch is worse than one that arrives at a blurry picture.
///
/// Three things have to hold at once, and only together: it must shrink, it
/// must still be drawable the instant it is drawn again, and it must sharpen.
#[test]
fn a_picture_scrolled_out_of_the_window_falls_to_a_floor_and_comes_back() {
    let dir = std::env::temp_dir().join(format!("viewport-floor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Six pictures, each taller than a third of the window, so scrolling to
    // the end puts the first one well outside the band culling keeps.
    let (w, h) = (512u32, 384u32);
    for i in 0..6 {
        let mut img = image::RgbaImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, (i * 40) as u8, 255]);
        }
        img.save(dir.join(format!("p{i}.png"))).unwrap();
    }
    let mut src = String::from("column gap=8 padding=16 {\n");
    for i in 0..6 {
        src.push_str(&format!("\timage \"/p{i}.png\"\n"));
    }
    src.push_str("}\n");
    let bytes = rill_doc::compile(&src).unwrap().bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });
    let window = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

    // Anything that has been drawable must stay drawable whenever it is drawn:
    // a frame naming a picture the client cannot supply is a hole on screen.
    let mut ever_ready: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut settle = |view: &mut AppView, steps: usize| {
        for _ in 0..steps {
            view.poll();
            let (cmds, ready, _h) = view.layout(window, &mut FixedMeasurer);
            for (s, _) in ready.iter() {
                ever_ready.insert(s.to_string());
            }
            for c in &cmds {
                if let rill_ui::DrawCommand::Image { source, .. } = c
                    && ever_ready.contains(source)
                {
                    assert!(
                        ready.image(source).is_some(),
                        "{source} is drawn in this frame but the client has no pixels \
                         for it — the picture blinked out"
                    );
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };

    settle(&mut view, 200);
    assert!(view.image_bytes_held() > 0, "nothing loaded");

    // To the end, which puts the first pictures out of the band.
    view.scroll_by(-100_000.0);
    settle(&mut view, 200);

    // The claim is about what is *not* on screen, so measure it as such: what
    // the window is showing is expected to cost full size, and everything else
    // together must come to less than one more picture. Comparing the total
    // before and after would not move on a roll this symmetrical — three
    // pictures come off the floor as three go onto it.
    let (_c, ready, _h) = view.layout(window, &mut FixedMeasurer);
    let each = (w as usize) * (h as usize) * 4;
    let on_screen = ready.iter().count();
    let at_end = view.image_bytes_held();
    assert!(
        at_end < (on_screen + 1) * each,
        "held {at_end} bytes with {on_screen} pictures on screen at {each} each — \
         the {} off screen are still costing about full size",
        6 - on_screen
    );
    assert!(
        ready.image("/p0.png").is_none(),
        "a picture scrolled far off screen is still being offered to the host — \
         a forwarding host would send it"
    );

    // And back. The picture must be drawable immediately (checked above, on
    // every frame) and sharpen to what the window is showing it at.
    view.scroll_by(100_000.0);
    settle(&mut view, 400);
    let (_c, ready, _h) = view.layout(window, &mut FixedMeasurer);
    let first = ready.image("/p0.png").expect("the first picture is drawable again");
    assert_eq!(
        (first.width, first.height),
        (w, h),
        "came back at {}x{} — the detail never returned",
        first.width,
        first.height
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A flick past a hundred pictures is not a hundred pictures being read.
///
/// The floor's price is the refetch: a picture coming back into the window has
/// to read its source again to sharpen. Measured before this was handled, a
/// flick down a forty-picture roll and back paid one refetch per picture per
/// pass — eighty source reads and decodes for a scroll that showed a blur.
/// Sharpening waits for the scroll to stop instead, which is a rule about
/// intent rather than a timer, so it holds at any scroll speed.
#[test]
fn a_flick_does_not_refetch_every_picture_it_passes() {
    let dir = std::env::temp_dir().join(format!("viewport-flick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let n = 20;
    for i in 0..n {
        let mut img = image::RgbaImage::new(256, 192);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, (i * 10) as u8, 255]);
        }
        img.save(dir.join(format!("p{i}.png"))).unwrap();
    }
    let mut src = String::from("column gap=8 padding=16 {\n");
    for i in 0..n {
        src.push_str(&format!("\timage \"/p{i}.png\"\n"));
    }
    src.push_str("}\n");
    std::fs::write(dir.join("page.rill"), rill_doc::compile(&src).unwrap().bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });
    let window = Rect { x: 0.0, y: 0.0, w: 640.0, h: 400.0 };
    let settle = |view: &mut AppView, steps: usize| {
        for _ in 0..steps {
            view.poll();
            view.layout(window, &mut FixedMeasurer);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    settle(&mut view, 300);

    // Wheel notches arriving faster than the easing settles: a scroll that
    // never stops moving until it reaches the end.
    let before = view.image_refetches();
    for _ in 0..20 {
        view.scroll_by(-600.0);
        settle(&mut view, 2);
    }
    for _ in 0..20 {
        view.scroll_by(600.0);
        settle(&mut view, 2);
    }
    let during = view.image_refetches() - before;
    assert!(
        during <= n / 2,
        "{during} refetches flicking past {n} pictures twice — the scroll is sharpening \
         everything it passes over"
    );

    // But coming to rest does sharpen what is under the reader's eyes: the
    // suppression must be about motion, not a way of never refining at all.
    // Waited out properly rather than for a fixed number of polls — the view
    // glides to a stop against the clock, and the refine only starts after it
    // has.
    settle_quiet(&mut view, window);
    let (_c, ready, _h) = view.layout(window, &mut FixedMeasurer);
    let first = ready.image("/p0.png").expect("back at the top, the first picture is drawable");
    assert_eq!(
        (first.width, first.height),
        (256, 192),
        "the scroll stopped and the picture stayed coarse ({}x{})",
        first.width,
        first.height
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A resize that wobbles across a halving boundary is not a fetch per wobble.
///
/// Between 900 and 700 px of width, a 1600-wide photograph wants a different
/// power-of-two step — so a drag oscillating across that line used to issue a
/// disk read and a decode per crossing (and, downstream, a multi-megabyte
/// re-send per crossing, which is the traffic that filled the socket's fd
/// ring and killed windows under rapid resize). Detail now waits for the
/// shape to hold still, the same rule scrolling already follows — and then it
/// does arrive, because a rule that never sharpened would just be a blur.
#[test]
fn a_resize_storm_does_not_refetch_per_boundary_crossing() {
    let dir = std::env::temp_dir().join(format!("viewport-reshape-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let (w, h) = (1600u32, 1200u32);
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, 64, 255]);
    }
    img.save(dir.join("photo.png")).unwrap();
    let bytes = rill_doc::compile(r#"column { image "/photo.png" }"#).unwrap().bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });
    let window = |w: f32| Rect { x: 0.0, y: 0.0, w, h: 700.0 };

    settle_quiet(&mut view, window(900.0));
    let sharp = view.image_refetches();

    // The storm: alternate across the boundary, holding each width long
    // enough for a refetch to land — because that is what a real drag does,
    // and it is the case that used to thrash. A back-to-back flip loop would
    // prove nothing: the one-refetch-in-flight guard bounds that on its own.
    for i in 0..20 {
        view.layout(window(if i % 2 == 0 { 700.0 } else { 900.0 }), &mut FixedMeasurer);
        for _ in 0..25 {
            view.poll();
            view.layout(window(if i % 2 == 0 { 700.0 } else { 900.0 }), &mut FixedMeasurer);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let during = view.image_refetches() - sharp;
    assert!(
        during <= 2,
        "{during} refetches while the shape was still moving — every crossing is \
         reading the disk again"
    );

    // The hand stops. The settle window runs out, the repaint it forces asks
    // for the detail, and the picture sharpens to the final width.
    settle_quiet(&mut view, window(900.0));
    let (_c, ready, _h) = view.layout(window(900.0), &mut FixedMeasurer);
    let got = ready.image("/photo.png").expect("drawable after the storm");
    assert!(
        got.width >= 852,
        "settled at 900px wide but the picture stayed at {}px — the deferral never \
         ended",
        got.width
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Dragging a window does not stop to rescale photographs.
///
/// Reducing a picture to the size it is shown at is the cheapest way to keep a
/// window's memory bounded, and doing it *in* layout meant paying for it on the
/// frame path: a drag that crossed a halving boundary rescaled every picture on
/// screen before the frame could be drawn. Measured over a 120-step drag of a
/// sixty-photograph roll: 12.2 s of layout before culling, 1.15 s after it, with
/// a single 401 ms frame inside that — a visible stall, in the one interaction
/// where the whole point is that the window follows the pointer.
///
/// The work still happens, on a worker thread, and nothing about it is urgent:
/// a coarsening has no visible effect, and a picture shown larger keeps
/// painting the copy it has until the finer one lands.
///
/// The bound here is deliberately loose — a hundred times the measured figure —
/// because this asserts that layout does not *do* the work, not how fast the
/// machine running it is.
#[test]
fn a_drag_does_not_rescale_on_the_frame_path() {
    let dir = std::env::temp_dir().join(format!("viewport-drag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Big enough that rescaling one is expensive, and several on a page.
    let n = 8;
    for i in 0..n {
        let mut img = image::RgbaImage::new(1024, 768);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, (i * 20) as u8, 255]);
        }
        img.save(dir.join(format!("p{i}.png"))).unwrap();
    }
    let mut src = String::from("column gap=8 padding=16 {\n");
    for i in 0..n {
        src.push_str(&format!("\timage \"/p{i}.png\"\n"));
    }
    src.push_str("}\n");
    std::fs::write(dir.join("page.rill"), rill_doc::compile(&src).unwrap().bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });
    let window = |w: f32| Rect { x: 0.0, y: 0.0, w, h: 800.0 };
    settle_quiet(&mut view, window(900.0));

    // The drag: width walked down and back up, one layout per step, which is
    // what a resize actually does. It crosses two halving boundaries each way.
    let mut worst = std::time::Duration::ZERO;
    let mut total = std::time::Duration::ZERO;
    let mut w = 900.0f32;
    let mut step = -10.0f32;
    for i in 0..120 {
        if i == 60 {
            step = 10.0;
        }
        w += step;
        let at = std::time::Instant::now();
        view.poll();
        view.layout(window(w), &mut FixedMeasurer);
        let took = at.elapsed();
        worst = worst.max(took);
        total += took;
    }

    assert!(
        worst < std::time::Duration::from_millis(100),
        "a single frame of a window drag took {worst:?} — layout is scaling pictures again"
    );
    assert!(
        total < std::time::Duration::from_millis(2000),
        "a 120-step window drag took {total:?} of layout"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Detail comes back when the picture is shown larger.
///
/// This is the risk in not keeping the original: once the reduced copy is the
/// only copy, zooming in has nothing finer to draw from and must fetch the
/// source again. It should come back — late is fine, never is not — and the
/// coarse version must keep painting meanwhile rather than the image
/// blinking out.
#[test]
fn showing_an_image_larger_recovers_its_detail() {
    let dir = std::env::temp_dir().join(format!("viewport-refine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let (w, h) = (2048u32, 1536u32);
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, 64, 255]);
    }
    img.save(dir.join("big.png")).unwrap();
    let bytes = rill_doc::compile(r#"column { image "/big.png" }"#).unwrap().bytes;
    std::fs::write(dir.join("page.rill"), &bytes).unwrap();

    let fetcher = Fetcher::new(dir.clone(), None, dir.clone()).expect("fetcher");
    let mut view =
        AppView::new(fetcher, Source::Local { dir: dir.clone(), path: "/page.rill".into() });

    // Settle small.
    let small = Rect { x: 0.0, y: 0.0, w: 160.0, h: 120.0 };
    settle_quiet(&mut view, small);
    let (_c, ready, _h) = view.layout(small, &mut FixedMeasurer);
    let narrow = ready.image("/big.png").map(|i| i.width).expect("settled small");
    assert!(narrow < w, "never reduced at all ({narrow}px of {w}px)");

    // Now show it much larger, and let the refetch land.
    let wide = Rect { x: 0.0, y: 0.0, w: 1600.0, h: 1200.0 };
    let mut recovered = narrow;
    for _ in 0..400 {
        view.poll();
        let (_c, ready, _h) = view.layout(wide, &mut FixedMeasurer);
        if let Some(image) = ready.image("/big.png") {
            // Whatever happens, something is always drawable — the coarse
            // copy holds the screen until the finer one arrives.
            recovered = recovered.max(image.width);
            if image.width > narrow {
                break;
            }
        } else {
            panic!("the image vanished while being refined");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert!(
        recovered > narrow,
        "detail never came back: still {recovered}px after being shown at 1600px \
         (was {narrow}px when shown at 160px)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
