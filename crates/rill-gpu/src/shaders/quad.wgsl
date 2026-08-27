@group(0) @binding(0) var<uniform> viewport: vec2<f32>;


struct ClipMask {
    center: vec2<f32>,
    half_size: vec2<f32>,
    radius: f32,
    on: f32,
};
@group(1) @binding(0) var<uniform> clip_mask: ClipMask;

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
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,   // pixel offset from the shape centre
    @location(2) half:  vec2<f32>,
    @location(3) params: vec3<f32>,  // (radius, blur, stroke)
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) center: vec2<f32>,
    @location(1) half:   vec2<f32>,
    @location(2) radius: f32,
    @location(3) blur:   f32,
    @location(4) color:  vec4<f32>,
    @location(5) stroke: f32,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0,  1.0), vec2<f32>(1.0, -1.0), vec2<f32>( 1.0, 1.0),
    );
    let margin = abs(blur) + 1.0;            // AA (1px) or blur/glow room
    let local = corners[vi] * (half + vec2<f32>(margin));
    let px = center + local;
    let ndc = vec2<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.local = local;
    out.half = half;
    out.params = vec3<f32>(radius, blur, stroke);
    return out;
}

// Signed distance to a rounded box (negative inside).
fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let radius = min(in.params.x, min(in.half.x, in.half.y));
    let blur = in.params.y;
    let d = sd_round_box(in.local, in.half, radius);
    let aa = max(fwidth(d), 1e-4);           // computed in uniform control flow
    let stroke = in.params.z;
    var coverage: f32;
    if (stroke > 0.0) {
        // Outline: cover the band straddling the edge. Same distance field as
        // the fill, read at |d| instead of d.
        coverage = 1.0 - smoothstep(stroke * 0.5 - aa, stroke * 0.5 + aa, abs(d));
    } else if (blur > 0.0) {
        coverage = 1.0 - smoothstep(-blur, blur, d);
    } else if (blur < 0.0) {
        // Glow (negative-blur marker): light only *outside* the shape,
        // peaking at the edge and falling off over |blur| — nothing bleeds
        // into the (possibly translucent) interior.
        coverage = (1.0 - smoothstep(0.0, -blur, max(d, 0.0)))
            * smoothstep(-aa, aa, d);
    } else {
        coverage = 1.0 - smoothstep(-aa, aa, d);
    }
    return vec4<f32>(in.color.rgb, in.color.a * coverage * mask_coverage(in.clip.xy));
}
