// VHS: per-band horizontal jitter, a slow rolling glitch bar, chromatic
// aberration, tape grain, scanlines, and a soft vignette. Animated.
//
// Windows are left untouched, the same decision as pixel.wgsl: tape damage
// over live text is illegible, and a desktop effect dresses the desktop,
// not what someone is reading. The window rects the compositor publishes
// are the mask — and because textureSample must stay in uniform control
// flow, the mask zeroes the displacement, fringe and damage rather than
// branching around the samples.
//
//   [desktop]
//   shader = "/path/to/rill/examples/shaders/vhs.wgsl"
//
// Tunable from the studio (Desktop → Screen Effect):
// @param jitter     0.0 .. 0.01 = 0.003 "Horizontal tape jitter"
// @param aberration 0.0 .. 6.0  = 1.5   "Colour fringe, in pixels"
// @param grain      0.0 .. 0.2  = 0.05  "Tape grain"
// @param scanlines  0.0 .. 0.3  = 0.08  "Scanline depth"

fn hash(p: vec2<f32>) -> f32 {
    let h = dot(p, vec2<f32>(127.1, 311.7));
    return fract(sin(h) * 43758.5453123);
}

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
    // Inside a window every distortion collapses to zero — the samples land
    // exactly where the pixel is and the damage terms multiply out.
    let damage = select(1.0, 0.0, in_window);

    var uv = in.uv;

    // Horizontal jitter, re-rolled per 3px band eight times a second. The
    // rolling glitch bar shoves five times as hard as the band jitter, the
    // same ratio the fixed numbers had.
    let jitter = param(0u) * damage;
    let band = floor(uv.y * fx.resolution.y / 3.0);
    uv.x += (hash(vec2<f32>(band, floor(time * 8.0))) - 0.5) * jitter;
    let roll = fract(time * 0.09);
    let bar = 1.0 - smoothstep(0.0, 0.015, abs(uv.y - roll));
    uv.x += bar * jitter * 5.0;

    // Chromatic aberration.
    let ab = param(1u) * damage / fx.resolution.x;
    let r = textureSample(scene, scene_samp, uv + vec2<f32>(ab, 0.0)).r;
    let g = textureSample(scene, scene_samp, uv).g;
    let b = textureSample(scene, scene_samp, uv - vec2<f32>(ab, 0.0)).b;
    var color = vec3<f32>(r, g, b);

    // Tape grain + scanlines + vignette.
    let grain = hash(uv * fx.resolution + vec2<f32>(fract(time) * 61.7, 0.0));
    color += (grain - 0.5) * param(2u) * damage;
    let depth = param(3u) * damage;
    let line = sin(uv.y * fx.resolution.y * 3.14159) * 0.5 + 0.5;
    color *= (1.0 - depth) + depth * line;
    let p = in.uv * 2.0 - 1.0;
    color *= 1.0 - 0.12 * dot(p, p) * damage;

    return vec4<f32>(color, 1.0);
}
