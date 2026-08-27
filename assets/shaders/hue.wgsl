// Hue drift: slowly rotate every color on screen about the grey axis —
// the whole desktop cycles the spectrum (~40s per revolution) while
// neutrals stay neutral. Animated.
//
//   [desktop]
//   shader = "/path/to/rill/examples/shaders/hue.wgsl"

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let c = textureSample(scene, scene_samp, in.uv);
    let a = time * 0.15;
    // Rodrigues rotation of the RGB vector about (1,1,1)/sqrt(3).
    let k = vec3<f32>(0.57735026);
    let rgb = c.rgb * cos(a)
        + cross(k, c.rgb) * sin(a)
        + k * dot(k, c.rgb) * (1.0 - cos(a));
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.a);
}
