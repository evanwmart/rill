@group(0) @binding(0) var<uniform> viewport: vec2<f32>;
@group(1) @binding(0) var image_tex: texture_2d<f32>;
@group(1) @binding(1) var image_samp: sampler;


struct ClipMask {
    center: vec2<f32>,
    half_size: vec2<f32>,
    radius: f32,
    on: f32,
};
@group(2) @binding(0) var<uniform> clip_mask: ClipMask;

// Coverage of the active rounded clip at a framebuffer pixel — 1 everywhere
// when no rounded clip is active. The scissor already bounds the rect; this
// enforces the curve, which is how a window masks content to its shape.
fn mask_coverage(px: vec2<f32>) -> f32 {
    if (clip_mask.on < 0.5) { return 1.0; }
    let r = min(clip_mask.radius, min(clip_mask.half_size.x, clip_mask.half_size.y));
    let q = abs(px - clip_mask.center) - clip_mask.half_size + vec2<f32>(r);
    let d = min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
    return clamp(0.5 - d, 0.0, 1.0);
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) alpha: f32,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let corner = corners[vi];
    let px = pos + corner * size;
    let ndc = vec2<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = corner;
    out.alpha = alpha;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(image_tex, image_samp, in.uv);
    return vec4<f32>(c.rgb, c.a) * in.alpha * mask_coverage(in.clip.xy);
}
