// Vignette: a quiet darkening toward the corners. Static.
//
//   [desktop]
//   shader = "/path/to/rill/examples/shaders/vignette.wgsl"

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let c = textureSample(scene, scene_samp, in.uv);
    let p = in.uv * 2.0 - 1.0;
    let v = 1.0 - 0.25 * smoothstep(0.4, 1.6, dot(p, p));
    return vec4<f32>(c.rgb * v, c.a);
}
