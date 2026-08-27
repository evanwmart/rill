@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    return textureSample(scene, scene_samp, in.uv);
}
