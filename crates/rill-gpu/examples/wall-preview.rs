//! Headless preview of a background (wallpaper) shader — the design loop
//! for scene wallpapers, no compositor needed.
//!
//!   cargo run -p rill-gpu --example wall-preview -- \
//!       assets/shaders/lofi.wgsl out.ppm [WxH] [clock-seconds] [time]
//!
//! A plausible fake desktop is composited over the wallpaper: two app
//! windows (one focused, one freshly spawned) and a dock strip, so the
//! window-aware channels light up in the preview.

use rill_gpu::{FxInputs, Renderer, SceneLayer};
use rill_ui::{Color, DrawCommand, Rect};

fn main() {
    let mut args = std::env::args().skip(1);
    let shader = args.next().expect("shader path");
    let out = args.next().expect("output .ppm path");
    let size = args.next().unwrap_or_else(|| "900x560".into());
    let clock: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(17.0 * 3600.0 + 40.0 * 60.0);
    let time: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8.0);
    let (w, h) = size.split_once('x').expect("WxH");
    let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());

    let r = Renderer::new_headless().expect("wgpu adapter");
    r.device().on_uncaptured_error(Box::new(|e| {
        eprintln!("WGPU-ERROR: {e}");
    }));
    let source = std::fs::read_to_string(&shader).expect("shader source");
    // RILL_PREVIEW_EFFECT=1 installs the shader as a grader over the fake
    // desktop instead of a wallpaper under it — the only way to look at an
    // effect that reacts to window chrome. RILL_PREVIEW_WINDOW_FX=1
    // installs it as the per-window effect instead, drawn as a layer above
    // each fake window — the design loop for window_fire/window_aura-style
    // shaders.
    let as_effect = std::env::var_os("RILL_PREVIEW_EFFECT").is_some();
    let as_window_fx = std::env::var_os("RILL_PREVIEW_WINDOW_FX").is_some();
    if as_window_fx {
        r.set_window_fx(Some(&source)).expect("shader compiles");
    } else if as_effect {
        r.set_effect(Some(&source)).expect("shader compiles");
    } else {
        r.set_background(Some(&source)).expect("shader compiles");
    }

    // The fake desktop: window rects the scene reacts to (y-down, pixels).
    let win_a = [w as f32 * 0.12, h as f32 * 0.18, w as f32 * 0.34, h as f32 * 0.42];
    let win_b = [w as f32 * 0.55, h as f32 * 0.30, w as f32 * 0.30, h as f32 * 0.36];
    let dock = [0.0, h as f32 - 44.0, w as f32, 44.0];
    // RILL_PREVIEW_BARE=1 renders the wallpaper alone — the fake desktop is
    // for checking window-awareness, and it sits in front of the scenery.
    let bare = std::env::var_os("RILL_PREVIEW_BARE").is_some();
    let panes: Vec<DrawCommand> = [win_a, win_b]
        .iter()
        .map(|r| DrawCommand::Rect {
            rect: Rect { x: r[0], y: r[1], w: r[2], h: r[3] },
            color: Color { r: 24, g: 24, b: 34, a: 235 },
            corner_radius: 12.0,
        })
        .collect();

    let target = r.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("preview"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
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
        w,
        h,
        Color { r: 0, g: 0, b: 0, a: 255 },
        &if bare {
            vec![SceneLayer::Shader]
        } else if as_window_fx {
            // Each window's fx layer sits directly above it, scissored to
            // the window grown by the effect's reach — the same shape the
            // compositor builds.
            let grow = |r: &[f32; 4]| Rect {
                x: r[0] - 120.0,
                y: r[1] - 120.0,
                w: r[2] + 240.0,
                h: r[3] + 240.0,
            };
            vec![
                SceneLayer::commands(&panes),
                SceneLayer::WindowFx { window: 0, bounds: grow(&win_a) },
                SceneLayer::WindowFx { window: 1, bounds: grow(&win_b) },
            ]
        } else if as_effect {
            vec![SceneLayer::commands(&panes)]
        } else {
            vec![SceneLayer::Shader, SceneLayer::commands(&panes)]
        },
        FxInputs {
            time,
            clock,
            cursor: [w as f32 * 0.7, h as f32 * 0.8],
            scene: rill_gpu::SceneParams::default(),
            windows: vec![win_a, win_b, dock],
            // (spawn_age, focused, kind, speed): B is focused; A just spawned.
            window_meta: vec![
                [0.6, 0.0, 0.0, 420.0],
                [f32::MAX, 1.0, 0.0, 0.0],
                [f32::MAX, 0.0, 1.0, 0.0],
            ],
            // A is being dragged rightward, B and the dock sit still — so a
            // motion-aware shader shows both states in one frame.
            window_velocity: vec![[420.0, 0.0, 0.0, 0.0], [0.0; 4], [0.0; 4]],
            // A synthetic 120bpm groove, so a sound-reactive shader can be
            // previewed without playing any audio. Two ingredients, like
            // actual music: a kick that hits the low bands on the beat, and
            // sustained mid/high content that wobbles per band — the real
            // tap's 250ms envelopes never drop the whole spectrum to zero
            // between kicks, so neither does the preview.
            audio: {
                let beat = (-(time * 2.0).fract() * 6.0).exp();
                let mut a = rill_gpu::AudioFx {
                    bands: [0.25 + 0.65 * beat, 0.4, 0.3, 0.35 + 0.4 * beat],
                    pulse: [beat, 0.3 + 0.4 * beat, (time * 2.0).floor(), 0.9 * beat],
                    ..Default::default()
                };
                for i in 0..32 {
                    let f = i as f32;
                    let sustained =
                        0.28 + 0.20 * (time * 3.0 + f * 1.7).sin() * (f / 32.0);
                    let kick = 0.7 * beat * (1.0 - f / 12.0).max(0.0);
                    a.spectrum[i / 4][i % 4] = (sustained + kick).min(1.0);
                }
                a
            },
        },
    );
    let rgba = r.read_texture_rgba(&target, w, h);
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.as_chunks::<4>().0 {
        ppm.extend_from_slice(&px[..3]);
    }
    std::fs::write(&out, ppm).expect("write ppm");
    println!("{out}: {w}x{h} clock={clock} time={time}");
}
