// The contract a particle *draw* shader is compiled against.
//
// The pass is instanced: one instance per particle, and the vertex count per
// instance is the shader's own business — emit a triangle, a quad, or a
// degenerate vertex to skip. A wallpaper supplies
// `@vertex fn vs_main(...) -> VsOut` and `@fragment fn fs_main(...)`.
//
// It runs **twice** per frame, once under the windows and once over them,
// so a particle field can genuinely weave through the desktop rather than
// sitting entirely in front of it or entirely behind. `layer_front` says
// which pass this is; a shader decides per particle which layer it belongs
// to (`pos.z` is the usual choice) and emits an off-screen degenerate
// vertex for the pass it does not belong in. Doing nothing about this is
// also valid — the particle is then simply drawn in both layers.
struct Particle {
    pos: vec4<f32>,
    vel: vec4<f32>,
}

@group(0) @binding(0) var<uniform> viewport: vec2<f32>;
@group(1) @binding(0) var<storage, read> particles: array<Particle>;
// 0 = this pass is behind the windows, 1 = in front of them.
@group(1) @binding(1) var<uniform> layer_front: u32;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

// Pixels → clip space, with y down as everything else in Rill measures it.
fn to_clip(px: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(
        px.x / viewport.x * 2.0 - 1.0,
        1.0 - px.y / viewport.y * 2.0,
        0.0,
        1.0,
    );
}

// The "not in this layer" answer: off-screen, so it is clipped and costs
// nothing.
fn skip() -> VsOut {
    var out: VsOut;
    out.clip = vec4<f32>(2.0, 2.0, 0.0, 1.0);
    out.color = vec4<f32>(0.0);
    return out;
}

// The trail field the last field pass produced, readable from the fragment
// stage. A field simulation is drawn by colouring this, not by drawing its
// agents — see `slime_draw.wgsl`, which emits one fullscreen quad from
// instance 0 and skips every other instance.
@group(1) @binding(2) var<storage, read> trail: array<f32>;

// What the desktop sounds like — same contract as the effect preamble's
// fx_audio: row 0 = (bass, mid, treble, level) smoothed; row 1.x = beat
// pulse, row 1.y = raw level, row 1.z = monotonic beat count, row 1.w =
// raw kick-band thump; rows 2..9 = 32 log-spaced spectrum bands. All zero
// in silence. Read through the helpers.
@group(1) @binding(3) var<uniform> fx_audio: array<vec4<f32>, 10>;

fn audio_levels() -> vec4<f32> { return fx_audio[0]; }
fn audio_beat() -> f32 { return fx_audio[1].x; }
fn audio_beat_count() -> f32 { return fx_audio[1].z; }
fn audio_kick() -> f32 { return fx_audio[1].w; }
fn audio_band(i: u32) -> f32 {
    let j = min(i, 31u);
    return fx_audio[2u + j / 4u][j % 4u];
}

fn trail_at_pixel(px: vec2<f32>) -> f32 {
    let x = i32(px.x);
    let y = i32(px.y);
    let w = i32(viewport.x);
    let h = i32(viewport.y);
    if (x < 0 || y < 0 || x >= w || y >= h) {
        return 0.0;
    }
    return trail[y * w + x];
}
