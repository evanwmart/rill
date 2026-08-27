// Night light: warm the whites, ease the blues, lift the gamma a touch.
// Static (never reads `time`) — the compositor keeps its damage-gated idle.
//
//   [desktop]
//   shader = "/path/to/rill/examples/shaders/night.wgsl"

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let c = textureSample(scene, scene_samp, in.uv);
    var color = c.rgb * vec3<f32>(1.0, 0.93, 0.82);
    color = pow(color, vec3<f32>(0.96));
    return vec4<f32>(color, c.a);
}
