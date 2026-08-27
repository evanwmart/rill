// One instance per path segment, drawn as a capsule: the segment swept by a
// circle of diameter `width`. Round caps come free from the SDF, and because
// consecutive segments share an endpoint, the overlapping caps *are* the
// round join — no joint geometry, no miter math.
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
    // Everything below is in pixel space, so the fragment stage can measure
    // its distance to the segment directly.
    @location(1) px: vec2<f32>,
    @location(2) p0: vec2<f32>,
    @location(3) p1: vec2<f32>,
    @location(4) half_width: f32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) p0: vec2<f32>,
    @location(1) p1: vec2<f32>,
    @location(2) width: f32,
    @location(3) color: vec4<f32>,
) -> VsOut {
    // Oriented bounding quad around the capsule. A zero-length segment (a
    // single-point path = a dot) has no direction of its own, so fall back to
    // +x; the SDF still renders a circle.
    let d = p1 - p0;
    let len = length(d);
    var dir = vec2<f32>(1.0, 0.0);
    if (len > 1e-6) {
        dir = d / len;
    }
    let normal = vec2<f32>(-dir.y, dir.x);

    let half_width = max(width, 0.0) * 0.5;
    let pad = half_width + 1.0;               // room for the AA ramp
    let mid = (p0 + p1) * 0.5;
    let half_len = len * 0.5 + pad;

    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0,  1.0), vec2<f32>(1.0, -1.0), vec2<f32>( 1.0, 1.0),
    );
    let c = corners[vi];
    let px = mid + dir * (c.x * half_len) + normal * (c.y * pad);

    let ndc = vec2<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.px = px;
    out.p0 = p0;
    out.p1 = p1;
    out.half_width = half_width;
    return out;
}

// Distance from p to the segment ab.
fn sd_segment(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let denom = max(dot(ba, ba), 1e-6);
    let h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    return length(pa - ba * h);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let d = sd_segment(in.px, in.p0, in.p1) - in.half_width;
    let aa = max(fwidth(d), 1e-4);
    let coverage = 1.0 - smoothstep(-aa, aa, d);
    return vec4<f32>(in.color.rgb, in.color.a * coverage * mask_coverage(in.clip.xy));
}
