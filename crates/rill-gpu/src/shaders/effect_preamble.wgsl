// `clock` is seconds since local midnight — the wall clock, so a scene can
// have a time of day. (`time` stays seconds-since-launch, for motion.)
struct FxUniforms {
    resolution: vec2<f32>,
    cursor: vec2<f32>,
    clock: f32,
    // Which window this pass belongs to, as an index into `fx_windows`, or
    // **-1 for a whole-output pass** (a grader, or a background wallpaper).
    //
    // This is what makes a *per-window* effect possible. Such an effect is
    // drawn as a scene layer immediately above its own window rather than
    // over the finished frame, so anything stacked higher — including a
    // glass window's backdrop sample — sees it as part of the scene and
    // blurs it correctly. A shader that draws for `window` only, and leaves
    // every other pixel at zero alpha, occludes for real instead of testing
    // rects to guess what covers it.
    window: f32,
    // Scalar padding on purpose: a vec3 here is 16-aligned and would inflate
    // the struct past the 32-byte buffer.
    _pad1: f32,
    _pad2: f32,
}
@group(0) @binding(0) var scene: texture_2d<f32>;
@group(0) @binding(1) var scene_samp: sampler;
@group(0) @binding(2) var<uniform> fx: FxUniforms;
@group(0) @binding(3) var<uniform> time: f32;
// The live window layout, in screen pixels (x, y, w, h) — wallpapers and
// effects can react to where the user's windows are.
@group(0) @binding(4) var<uniform> fx_windows: array<vec4<f32>, 64>;
@group(0) @binding(5) var<uniform> fx_window_count: u32;
// Scene semantics, parallel to fx_windows: (spawn_age_secs, focused, kind,
// speed_px_per_sec). kind: 0 = app window, 1 = the dock strip. Windows are
// listed bottom→top — index order IS stacking order. Read-only scenery
// input: react to the desktop, never pretend to be part of its UI.
@group(0) @binding(6) var<uniform> fx_window_meta: array<vec4<f32>, 64>;
// The showcase scene's knobs — the same values the model pass receives, so
// a background and its object cannot disagree about the room they share.
// (`[desktop.showroom]` in theme.toml; Theme Studio edits it live.)
struct Studio {
    key: vec4<f32>,          // xyz direction, w intensity
    key_color: vec4<f32>,
    fill: vec4<f32>,         // w <= 0 disables the fill light
    fill_color: vec4<f32>,
    rim_color: vec4<f32>,    // w intensity
    body_color: vec4<f32>,   // w >= 0.5 overrides the model's own colour
    ground_color: vec4<f32>,
    motion: vec4<f32>,       // spin rate (signed), phase, camera distance, exposure
    backdrop_color: vec4<f32>, // rgb the backdrop is, w = turntable ring strength
    finish: vec4<f32>,       // reflection, reflection fade, backdrop glow, vignette
    fit: vec4<f32>,          // model up-axis (0 = Y, 1 = Z), height, lift, spare
}
@group(0) @binding(7) var<uniform> studio: Studio;
// Per-window velocity in screen pixels per second, parallel to fx_windows:
// (vx, vy, _, _). Smoothed over ~0.12s and left to decay, so a drag reads as
// a push that lingers a moment rather than a single frame's jump. The meta
// row's speed is this vector's length — take it from here when direction
// matters (which way a flame leans, where a wake trails).
@group(0) @binding(8) var<uniform> fx_window_vel: array<vec4<f32>, 64>;
// What the desktop *sounds* like — the compositor's tap on the system
// output monitor, so any playing audio drives it. All rows are zero in
// silence (or when no tap is running); a reactive shader must read zero as
// "be still", never as an error.
//   row 0: (bass, mid, treble, level), each 0..~1, attack/decay smoothed
//          and slow-AGC normalised producer-side — they already move like
//          music, do not re-smooth.
//   row 1: x = beat, 1.0 on a bass onset decaying to 0 — the difference
//          between a wallpaper that dances and one that meters;
//          y = raw unsmoothed level;
//          z = beats heard so far, a monotonic counter — what lets a
//          stateless shader change *per* beat (cycle a palette, reseed
//          sparks) rather than merely pulse with one;
//          w = raw kick-band energy (~40–120Hz, unsmoothed) — the thump
//          itself, jumpy on purpose, for punch rather than pulse.
//   rows 2..9: 32 log-spaced spectrum bands, low → high, 4 per row.
// Read through the helpers below rather than indexing rows by hand.
@group(0) @binding(9) var<uniform> fx_audio: array<vec4<f32>, 10>;

// (bass, mid, treble, level) — smoothed, normalised.
fn audio_levels() -> vec4<f32> { return fx_audio[0]; }
// The beat pulse: 1.0 on a bass onset, decaying to 0.
fn audio_beat() -> f32 { return fx_audio[1].x; }
// Beats heard so far — monotonic, for per-beat choices (palettes, seeds).
fn audio_beat_count() -> f32 { return fx_audio[1].z; }
// Raw kick-band energy — the unsmoothed thump, 0 between kicks.
fn audio_kick() -> f32 { return fx_audio[1].w; }
// Spectrum band `i` of 32, low → high; out-of-range clamps to the top.
fn audio_band(i: u32) -> f32 {
    let j = min(i, 31u);
    return fx_audio[2u + j / 4u][j % 4u];
}

// Declared parameters: the values behind this shader's `// @param` lines,
// lane-packed in declaration order (parameter 0 is fx_params[0].x, 4 is
// fx_params[1].x, …). The studio's sliders and theme.toml's
// [desktop.shader_params.<shader>] write them; an undeclared or untouched
// parameter reads as its declared default (the host uploads defaults too).
// Zero when nothing was declared — read through `param(i)`.
@group(0) @binding(10) var<uniform> fx_params: array<vec4<f32>, 8>;

// Declared parameter `i`, in declaration order; out-of-range reads 0.
fn param(i: u32) -> f32 {
    let j = min(i, 31u);
    return fx_params[j / 4u][j % 4u];
}

struct FxIn {
    @builtin(position) frag: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> FxIn {
    let xy = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    var out: FxIn;
    out.frag = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}
