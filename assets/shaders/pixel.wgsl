// Pixel: mosaic cells with color quantization — the retro handheld look.
// Static (zero idle cost). Windows are left untouched: a mosaic over live
// text made it shimmer at cell boundaries, and a desktop effect's job is to
// dress the desktop, not to degrade what someone is reading. The window
// rects the compositor already publishes are the mask.
//
// One sample, coordinates chosen by the mask, because textureSample must
// stay in uniform control flow — a per-window early return fails naga's
// uniformity analysis.
//
//   [desktop]
//   shader = "/path/to/rill/examples/shaders/pixel.wgsl"
//
// Tunable from the studio (Desktop → Screen Effect):
// @param cell   1.0 .. 8.0  = 2.0  "Mosaic cell size, in pixels"
// @param levels 4.0 .. 32.0 = 14.0 "Colour steps per channel"

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let pixel = in.uv * fx.resolution;
    var in_window = false;
    let count = min(fx_window_count, 64u);
    for (var i = 0u; i < count; i = i + 1u) {
        let r = fx_windows[i];
        if (pixel.x >= r.x && pixel.x <= r.x + r.z
            && pixel.y >= r.y && pixel.y <= r.y + r.w) {
            in_window = true;
        }
    }
    let cell = max(param(0u), 1.0);
    let levels = max(param(1u), 1.0);
    let center = floor(pixel / cell) * cell + vec2<f32>(cell * 0.5);
    // Inside a window: sample this very pixel and keep its true colour —
    // text stays vector-sharp while the desktop around it wears the mosaic.
    let uv = select(center / fx.resolution, in.uv, in_window);
    let c = textureSample(scene, scene_samp, uv);
    let q = select(floor(c.rgb * levels + 0.5) / levels, c.rgb, in_window);
    return vec4<f32>(q, 1.0);
}
