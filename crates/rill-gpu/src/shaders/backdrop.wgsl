@group(0) @binding(0) var<uniform> viewport: vec2<f32>;
@group(1) @binding(0) var blurred_tex: texture_2d<f32>;
@group(1) @binding(1) var blurred_samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    // Scene UV computed in the vertex stage (the viewport uniform is
    // vertex-visible only); linear interpolation keeps it exact.
    @location(0) uv: vec2<f32>,
    @location(1) local: vec2<f32>,   // pixel offset from the pane centre
    @location(2) half: vec2<f32>,
    @location(3) radius: f32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) radius: f32,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let px = pos + corners[vi] * size;
    let ndc = vec2<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = px / viewport;
    out.local = px - (pos + size * 0.5);
    out.half = size * 0.5;
    out.radius = radius;
    return out;
}

fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let color = textureSample(blurred_tex, blurred_samp, in.uv);
    // Vibrancy: saturate and lift what shows through the glass — blur alone
    // reads flat; this is what makes frosted panes look lit from behind.
    let lum = dot(color.rgb, vec3<f32>(0.299, 0.587, 0.114));
    let vibrant = clamp(
        mix(vec3<f32>(lum), color.rgb, 1.35) * 1.06,
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    let r = min(in.radius, min(in.half.x, in.half.y));
    let d = sd_round_box(in.local, in.half, r);
    let aa = max(fwidth(d), 1e-4);
    let mask = 1.0 - smoothstep(-aa, aa, d);
    return vec4<f32>(vibrant, mask);
}
