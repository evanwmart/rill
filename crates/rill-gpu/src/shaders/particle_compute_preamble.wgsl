// The contract a particle *update* shader is compiled against.
//
// A particle wallpaper is two shaders over one double-buffered state array:
// this one steps the simulation, and a draw shader turns the result into
// geometry. The renderer supplies both preambles; a wallpaper supplies
// `@compute @workgroup_size(64) fn cs_main(...)` and a vertex/fragment pair.
//
// State is deliberately loose — position and velocity, four floats each,
// with the w components free. A flock uses `pos.z` as depth; dust can use
// it as settle time or as a seed. The renderer never interprets it, except
// that the draw side splits on `pos.z` to decide which side of the window
// stack a particle lands on.
//
// `src` is the previous step and `dst` is the one being written. They swap
// every frame, so never read `dst` expecting to find anything in it.
struct Particle {
    pos: vec4<f32>,
    vel: vec4<f32>,
}

struct Params {
    count: u32,
    // How many entries of `windows`/`window_vel` are live this frame.
    nwin: u32,
    // Seconds since the last step, already clamped to something sane — a
    // stalled frame must not teleport the simulation.
    dt: f32,
    // Seconds since launch.
    time: f32,
    // Output size in pixels; particle positions are in the same space.
    size: vec2<f32>,
    cursor: vec2<f32>,
}

@group(0) @binding(0) var<storage, read> src: array<Particle>;
@group(0) @binding(1) var<storage, read_write> dst: array<Particle>;
@group(0) @binding(2) var<uniform> params: Params;
// Window rects in pixels (x, y, w, h), bottom→top — index order is
// stacking order.
@group(0) @binding(3) var<uniform> windows: array<vec4<f32>, 64>;
// Window velocity in pixels/second, parallel to `windows`: (vx, vy, _, _).
// Smoothed compositor-side over ~0.12s and left to decay, so letting go of
// a drag coasts to a stop instead of stopping dead. This is what a particle
// reads to be *pushed* rather than merely to avoid a rectangle.
@group(0) @binding(4) var<uniform> window_vel: array<vec4<f32>, 64>;

// The trail field: one f32 per output pixel, row-major, `params.size.x` wide.
// Double buffered alongside the particles and swapped with them, so `trail`
// is what this frame reads and deposits into, and `trail_next` is what a
// field pass writes.
//
// This is what lets a simulation leave something behind. A slime mould
// senses its own deposits and follows them; ink spreads; a fluid carries
// dye. None of that fits in particle state, because it is a property of the
// *surface* rather than of any agent, and it has to survive between frames.
//
// Two passes share these bindings. The agent pass (`cs_main`, dispatched one
// invocation per particle) reads `trail` to steer and adds to it to deposit.
// The optional field pass (`[desktop] particle_diffuse`, dispatched one
// invocation per *pixel*) reads `trail` and writes `trail_next` — the blur
// and decay that turns deposits into a living surface.
//
// Deposits race: several agents may add to one pixel in the same step, and
// the result is whichever write landed last. For a trail that is immediately
// blurred and decayed this is invisible, and it is far cheaper than atomics.
@group(0) @binding(5) var<storage, read_write> trail: array<f32>;
@group(0) @binding(6) var<storage, read_write> trail_next: array<f32>;

// What the desktop sounds like — same contract as the effect preamble's
// fx_audio: row 0 = (bass, mid, treble, level) smoothed; row 1.x = beat
// pulse, row 1.y = raw level, row 1.z = monotonic beat count, row 1.w =
// raw kick-band thump; rows 2..9 = 32 log-spaced spectrum bands. All zero
// in silence. Read through the helpers.
@group(0) @binding(7) var<uniform> fx_audio: array<vec4<f32>, 10>;

fn audio_levels() -> vec4<f32> { return fx_audio[0]; }
fn audio_beat() -> f32 { return fx_audio[1].x; }
fn audio_beat_count() -> f32 { return fx_audio[1].z; }
fn audio_kick() -> f32 { return fx_audio[1].w; }
fn audio_band(i: u32) -> f32 {
    let j = min(i, 31u);
    return fx_audio[2u + j / 4u][j % 4u];
}

// Index into the trail for a pixel, or -1 when the point is off-screen.
fn trail_index(p: vec2<f32>) -> i32 {
    let x = i32(p.x);
    let y = i32(p.y);
    let w = i32(params.size.x);
    let h = i32(params.size.y);
    if (x < 0 || y < 0 || x >= w || y >= h) {
        return -1;
    }
    return y * w + x;
}

// Trail value at a point, 0 off-screen.
fn trail_at(p: vec2<f32>) -> f32 {
    let i = trail_index(p);
    if (i < 0) { return 0.0; }
    return trail[i];
}
