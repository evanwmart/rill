struct KUniform { texel: vec2<f32>, offset: f32, pad: f32 }
@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;
@group(0) @binding(2) var<uniform> u: KUniform;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    let xy = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    var out: VsOut;
    out.clip = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}

@fragment
fn fs_down(in: VsOut) -> @location(0) vec4<f32> {
    let o = u.texel * u.offset;
    var c = textureSample(src_tex, src_samp, in.uv) * 4.0;
    c += textureSample(src_tex, src_samp, in.uv - o);
    c += textureSample(src_tex, src_samp, in.uv + o);
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(o.x, -o.y));
    c += textureSample(src_tex, src_samp, in.uv - vec2<f32>(o.x, -o.y));
    return c / 8.0;
}

@fragment
fn fs_up(in: VsOut) -> @location(0) vec4<f32> {
    let o = u.texel * u.offset;
    var c = textureSample(src_tex, src_samp, in.uv + vec2<f32>(-o.x * 2.0, 0.0));
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(-o.x, o.y)) * 2.0;
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(0.0, o.y * 2.0));
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(o.x, o.y)) * 2.0;
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(o.x * 2.0, 0.0));
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(o.x, -o.y)) * 2.0;
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(0.0, -o.y * 2.0));
    c += textureSample(src_tex, src_samp, in.uv + vec2<f32>(-o.x, -o.y)) * 2.0;
    return c / 12.0;
}
