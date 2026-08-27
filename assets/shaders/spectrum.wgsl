// Mirrored sound columns — the desktop as a quiet equalizer.
//
// One horizontal bar per audio band, 32 of them stacked bass-at-bottom,
// growing inward from the left and right edges in mirror image. Bass
// reaches furthest on a kick, treble shimmers near the top, and the bar
// tips flash white on the beat.
//
// Purely sound-driven on purpose: this never reads `time`, so with nothing
// playing the desktop is pixel-still and the damage gate keeps its idle
// win — the compositor keeps frames coming only while something is audible
// (the audio gate in its render loop). Every value read here is already
// attack/decay smoothed and AGC-normalised producer-side; adding smoothing
// on top would only make the bars feel late.

// How far a full-strength bar reaches, as a fraction of the half-width.
// Under 1.0 so opposing bars never quite touch, even on a peak.
const REACH: f32 = 0.85;

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    // Rows: uv.y runs 0 at the top, so flip — bass belongs at the bottom.
    let slot = (1.0 - in.uv.y) * 32.0;
    let i = u32(clamp(slot, 0.0, 31.0));
    let v = audio_band(i);
    let level = audio_levels().w;
    let beat = audio_beat();

    // Distance from the nearer edge toward the centre, 0..1 — measuring
    // from *either* edge is the mirror, for free.
    let reach = min(in.uv.x, 1.0 - in.uv.x) * 2.0;

    // Bar length. Squared response: quiet bands stay shy at the edges,
    // peaks lunge for the centre — linear reads as a meter, this dances.
    let len = v * v * REACH;
    // Fade a bar in with its signal rather than gating it, so nothing pops.
    let on = smoothstep(0.008, 0.05, v);
    let body = (1.0 - smoothstep(len - 0.015, len + 0.006, reach)) * on;

    // Row shaping: a clear gap between neighbouring bars.
    let within = fract(slot);
    let row = smoothstep(0.10, 0.24, within) * (1.0 - smoothstep(0.76, 0.90, within));

    // A soft bloom past the tip, so a bar lights the dark it is about to
    // move into; and a white flash *at* the tip on the beat.
    let bloom = exp(-max(reach - len, 0.0) * 12.0) * v * on;
    let tip = exp(-abs(reach - len) * 34.0) * beat * on;

    // Deep indigo bass through cyan to pale mint treble.
    let t = f32(i) / 31.0;
    let bar_col = mix(vec3<f32>(0.16, 0.32, 0.95), vec3<f32>(0.32, 0.95, 0.82), t);

    // The ground: a near-black vertical gradient, plus a faint centre
    // bloom that breathes with the overall level — the room responding,
    // not just the bars.
    let base = mix(
        vec3<f32>(0.030, 0.036, 0.066),
        vec3<f32>(0.012, 0.014, 0.026),
        in.uv.y,
    );
    let centre = exp(-abs(in.uv.x - 0.5) * 7.0) * level * 0.05;

    var rgb = base + vec3<f32>(centre * 0.7, centre, centre * 1.3);
    rgb += bar_col * row * (body * (0.30 + 0.70 * v) + bloom * 0.35);
    rgb += vec3<f32>(1.0, 1.0, 1.0) * tip * row * 0.9;
    return vec4<f32>(rgb, 1.0);
}
