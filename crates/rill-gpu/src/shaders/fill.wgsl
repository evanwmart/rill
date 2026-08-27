// Filled closed contours, even-odd, with analytic anti-aliasing: one
// instance per fill covering its bounding box; the fragment stage ray-casts
// against the fill's flattened segments (a slice of the frame's shared
// segment buffer) for parity, and takes the distance to the nearest edge
// for coverage. Icons are a few hundred segments over a few hundred pixels
// — the loop is small where the box is small.
@group(0) @binding(0) var<uniform> viewport: vec2<f32>;
@group(1) @binding(0) var<storage, read> segments: array<vec4<f32>>;


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
    @location(0) color: vec4<f32>,
    @location(1) px: vec2<f32>,
    @location(2) @interpolate(flat) seg_start: u32,
    @location(3) @interpolate(flat) seg_count: u32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) bbox: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) seg: vec2<u32>,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vi];
    let px = bbox.xy + c * bbox.zw;
    let ndc = vec2<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.px = px;
    out.seg_start = seg.x;
    out.seg_count = seg.y;
    return out;
}

fn sd_segment(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let denom = max(dot(ba, ba), 1e-6);
    let h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    return length(pa - ba * h);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var inside = false;
    var d = 1e9;
    for (var i = 0u; i < in.seg_count; i = i + 1u) {
        let s = segments[in.seg_start + i];
        let a = s.xy;
        let b = s.zw;
        if ((a.y > in.px.y) != (b.y > in.px.y)) {
            let t = (in.px.y - a.y) / (b.y - a.y);
            if (in.px.x < a.x + t * (b.x - a.x)) {
                inside = !inside;
            }
        }
        d = min(d, sd_segment(in.px, a, b));
    }
    // d is the unsigned distance to the nearest edge; parity signs it.
    let coverage = clamp(select(0.5 - d, 0.5 + d, inside), 0.0, 1.0);
    return vec4<f32>(in.color.rgb, in.color.a * coverage * mask_coverage(in.clip.xy));
}
