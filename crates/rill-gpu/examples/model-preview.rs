//! Headless preview of the model scene layer: OBJ + cinematic shader →
//! one frame as a PPM. The design loop for showcase models.
//!
//!   cargo run -p rill-gpu --example model-preview -- \
//!       model.obj shader.wgsl out.ppm [WxH] [time]

use rill_gpu::{FxInputs, Renderer, SceneLayer};
use rill_ui::Color;

/// Scene overrides from the environment, so the preview can exercise the
/// same knobs `[desktop.showroom]` sets: RILL_SCENE="spin=-0.2,body=#c81e1e,
/// fill=0,key_az=30,distance=5,ground=#101018".
fn scene_from_env() -> rill_gpu::SceneParams {
    let mut s = rill_gpu::SceneParams::default();
    let Ok(spec) = std::env::var("RILL_SCENE") else { return s };
    let hex = |v: &str| -> Option<[f32; 3]> {
        let c = rill_ui::Color::parse_hex(v)?;
        let lin = |b: u8| (b as f32 / 255.0).powf(2.2);
        Some([lin(c.r), lin(c.g), lin(c.b)])
    };
    let dir = |az: f32, el: f32| {
        let (az, el) = (az.to_radians(), el.to_radians());
        [az.sin() * el.cos(), el.sin(), az.cos() * el.cos()]
    };
    let (mut key_az, mut key_el) = (-42.0f32, 55.0f32);
    for part in spec.split(',') {
        let Some((k, v)) = part.split_once('=') else { continue };
        let num = v.parse::<f32>().unwrap_or(0.0);
        match k.trim() {
            "spin" => s.motion[0] = num,
            "distance" => s.motion[2] = num,
            "exposure" => s.motion[3] = num,
            "key_az" => key_az = num,
            "key_el" => key_el = num,
            "key_i" => s.key[3] = num,
            "fill" => s.fill[3] = num,
            "body" => {
                if let Some(c) = hex(v) {
                    s.body_color = [c[0], c[1], c[2], 1.0];
                }
            }
            "up" => {
                s.fit[0] = match v.trim() {
                    "z" => 1.0,
                    "-y" => 2.0,
                    "-z" => 3.0,
                    _ => 0.0,
                }
            }
            "height" => s.fit[1] = num,
            "lift" => s.fit[2] = num,
            "reflection" => s.finish[0] = num,
            "reflection_fade" => s.finish[1] = num,
            "glow" => s.finish[2] = num,
            "vignette" => s.finish[3] = num,
            "rings" => s.backdrop_color[3] = num,
            "backdrop" => {
                if let Some(c) = hex(v) {
                    s.backdrop_color = [c[0], c[1], c[2], s.backdrop_color[3]];
                }
            }
            "ground" => {
                if let Some(c) = hex(v) {
                    s.ground_color = [c[0], c[1], c[2], 0.0];
                }
            }
            "key_color" => {
                if let Some(c) = hex(v) {
                    s.key_color = [c[0], c[1], c[2], 0.0];
                }
            }
            _ => {}
        }
    }
    let d = dir(key_az, key_el);
    s.key = [d[0], d[1], d[2], s.key[3]];
    s
}

fn main() {
    let mut args = std::env::args().skip(1);
    let model = args.next().expect("model .obj path");
    let shader = args.next().expect("shader .wgsl path");
    let out = args.next().expect("output .ppm path");
    let size = args.next().unwrap_or_else(|| "900x560".into());
    let time: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(12.0);
    let (w, h) = size.split_once('x').expect("WxH");
    let (w, h): (u32, u32) = (w.parse().unwrap(), h.parse().unwrap());

    let start = std::time::Instant::now();
    let mesh = rill_gpu::mesh::load(std::path::Path::new(&model)).expect("obj loads");
    eprintln!(
        "{model}: {} triangles, {} materials {:?}, bounds {:?}..{:?} ({:?})",
        mesh.vertices.len() / 3,
        mesh.materials.len(),
        mesh.materials,
        mesh.min,
        mesh.max,
        start.elapsed(),
    );

    let r = Renderer::new_headless().expect("wgpu adapter");
    r.device().on_uncaptured_error(Box::new(|e| eprintln!("WGPU-ERROR: {e}")));
    let source = std::fs::read_to_string(&shader).expect("shader source");
    r.set_model(Some(&source), Some(&mesh)).expect("model shader compiles");
    // Optional wallpaper under the model (RILL_PREVIEW_BG=path.wgsl).
    if let Ok(bg) = std::env::var("RILL_PREVIEW_BG") {
        let src = std::fs::read_to_string(&bg).expect("bg source");
        r.set_background(Some(&src)).expect("bg compiles");
    }

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
        Color { r: 16, g: 14, b: 26, a: 255 },
        &[SceneLayer::Shader, SceneLayer::Model],
        FxInputs { time, scene: scene_from_env(), ..Default::default() },
    );
    let rgba = r.read_texture_rgba(&target, w, h);
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.as_chunks::<4>().0 {
        ppm.extend_from_slice(&px[..3]);
    }
    std::fs::write(&out, ppm).expect("write ppm");
    println!("{out}: {w}x{h} time={time} ({:?} total)", start.elapsed());
}
