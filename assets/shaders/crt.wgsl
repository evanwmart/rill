// CRT: barrel curvature, scanlines, RGB fringe, phosphor flicker. Animated
// (reads `time`), so the compositor renders continuously while installed.
//
// Install via ~/.config/rill/theme.toml:
//   [desktop]
//   shader = "/path/to/rill/examples/shaders/crt.wgsl"
//   warp_barrel = 0.07   # MUST match the 0.07 in the curvature below —
//                        # the compositor warps pointer input through the
//                        # same map so clicks land on what you see.
// (The dock's Shader toggle writes both keys together.)
//
// This file is a fragment stage only — the renderer prepends the scene
// texture, `fx` uniforms (resolution, cursor), `time`, and the fullscreen
// vertex stage (rill-gpu EFFECT_PREAMBLE; specs/wgpu-renderer.md D5).

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    // Curve the screen: push UVs outward from the centre.
    var uv = in.uv * 2.0 - 1.0;
    let r2 = dot(uv, uv);
    uv = uv * (1.0 + 0.07 * r2);
    uv = uv * 0.5 + 0.5;

    // Slight horizontal RGB fringe. (No early-out for offscreen UVs — the
    // samples must stay in uniform control flow; masked to black below.)
    let fringe = 0.75 / fx.resolution.x;
    let cr = textureSample(scene, scene_samp, uv + vec2<f32>(fringe, 0.0)).r;
    let cg = textureSample(scene, scene_samp, uv).g;
    let cb = textureSample(scene, scene_samp, uv - vec2<f32>(fringe, 0.0)).b;
    var color = vec3<f32>(cr, cg, cb);

    // Scanlines, a soft corner vignette, and a gentle flicker.
    let line = sin(uv.y * fx.resolution.y * 3.14159) * 0.5 + 0.5;
    color *= 0.88 + 0.12 * line;
    color *= 1.0 - 0.18 * r2;
    color *= 0.985 + 0.015 * sin(time * 120.0);

    // Black outside the curved tube.
    let inside = step(0.0, uv.x) * step(uv.x, 1.0) * step(0.0, uv.y) * step(uv.y, 1.0);
    return vec4<f32>(color * inside, 1.0);
}
