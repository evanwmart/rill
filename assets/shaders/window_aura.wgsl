// window_aura.wgsl  —  `[desktop] window_shader`
//
// A glow that hugs each window's border and hears the *bottom end* of the
// music: it flares on the kick (the beat pulse for shape, the raw
// kick-band thump for punch), changes colour on every beat, and throws
// sparks off the frame with each one. Mids and treble deliberately never
// reach it — a hi-hat or a vocal leaves the frame dark. The focused
// window wears the brightest aura.
//
// Per-beat colour is `audio_beat_count()` — the monotonic counter is what
// lets a stateless shader *change* on a beat (a new hue that stays until
// the next one) rather than merely pulse with it. The hue walks the wheel
// by the golden ratio, so neighbouring beats always land on distinct
// colours and the cycle never visibly repeats.
//
// Sparks need no state either: each beat seeds a handful of perimeter
// positions from the beat count, and the *decay of the beat envelope* is
// their clock — they fly outward as the pulse fades and are gone before
// the next kick reseeds them.
//
// Purely sound-driven on purpose: this never reads `time`, so a silent
// desktop is pixel-still and keeps the damage-gated idle; the compositor's
// audio gate keeps frames coming only while something is audible. Like
// window_fire, this is a per-window scene layer: it draws only around its
// own window (`fx.window`), adds premultiplied light with zero alpha, and
// never samples `scene`.

// How far the aura and its sparks may reach beyond the frame, in pixels —
// the band the compositor scissors this pass to.
const AURA_BAND: f32 = 80.0;
// Glow thickness: the e-folding distance of the halo outside the border.
const AURA_SOFT: f32 = 22.0;
// Sparks per beat, per window.
const SPARK_N: i32 = 12;
// How far a spark flies over one beat's decay.
const SPARK_FLY: f32 = 56.0;

fn aura_prng(seed: vec2<f32>) -> f32 {
    var s = fract(seed * vec2<f32>(5.3983, 5.4427));
    s += vec2<f32>(dot(s.yx, s.xy + vec2<f32>(21.5351, 14.3137)));
    return fract(s.x * s.y * 95.4337);
}

// A saturated hue on the colour wheel, kept away from muddy in-betweens.
fn aura_hue(h0: f32) -> vec3<f32> {
    let h = fract(h0) * 6.0;
    let c = clamp(
        abs(vec3<f32>(h - 3.0, h - 2.0, h - 4.0)) * vec3<f32>(1.0, -1.0, -1.0)
            + vec3<f32>(-1.0, 2.0, 2.0),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    // Lift toward white a little: pure primaries glare, jewel tones glow.
    return mix(c, vec3<f32>(1.0), 0.18);
}

// A point on the rect's perimeter at parameter s in [0,1), and its outward
// normal — where a spark is born and which way it flies. Packed as
// (pos.xy, normal.xy); WGSL has no tuples.
fn aura_perimeter(r: vec4<f32>, s: f32) -> vec4<f32> {
    let per = 2.0 * (r.z + r.w);
    var d = fract(s) * per;
    if (d < r.z) { // top edge, left → right
        return vec4<f32>(r.x + d, r.y, 0.0, -1.0);
    }
    d -= r.z;
    if (d < r.w) { // right edge, down
        return vec4<f32>(r.x + r.z, r.y + d, 1.0, 0.0);
    }
    d -= r.w;
    if (d < r.z) { // bottom edge, right → left
        return vec4<f32>(r.x + r.z - d, r.y + r.w, 0.0, 1.0);
    }
    d -= r.z; // left edge, up
    return vec4<f32>(r.x, r.y + r.w - d, -1.0, 0.0);
}

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    // This pass belongs to exactly one window; `fx.window` says which.
    let idx = i32(fx.window);
    if (idx < 0 || u32(idx) >= min(fx_window_count, 64u)) {
        return vec4<f32>(0.0);
    }
    let i = u32(idx);
    // The dock is shell, not a window — a glowing strip reads as a bug.
    let wm = fx_window_meta[i];
    if (wm.z > 0.5) {
        return vec4<f32>(0.0);
    }

    let a = audio_levels();
    let beat = audio_beat();
    // The gate: in silence this pass contributes exactly nothing, so the
    // desktop it idles over is untouched.
    let audible = smoothstep(0.01, 0.05, a.w + beat);
    if (audible <= 0.0) {
        return vec4<f32>(0.0);
    }

    let pixel = in.uv * fx.resolution;
    let r = fx_windows[i];

    // Signed distance to the window's border ring: negative inside.
    let half = r.zw * 0.5;
    let q = abs(pixel - (r.xy + half)) - half;
    let sd = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0);
    if (sd > AURA_BAND || sd < -8.0) {
        return vec4<f32>(0.0);
    }

    // This beat's colour, with a whisper of the last one surviving in the
    // tail of the pulse — the changeover flashes, it never snaps.
    let count = audio_beat_count();
    let col_now = aura_hue(count * 0.61803);
    let col_prev = aura_hue((count - 1.0) * 0.61803);
    let col = mix(col_now, col_prev, 0.35 * (1.0 - beat) * step(0.5, count));

    // The halo answers the *bottom end*: the beat pulse for shape, the raw
    // kick-band thump for punch. `punch` is what drives brightness and
    // swell — mids and treble never reach it, so a hi-hat or a vocal line
    // leaves the frame dark and a kick lights it.
    let kick = audio_kick();
    let punch = max(beat, kick * kick);
    let focus = 0.45 + 0.55 * wm.y;
    // A whisper of the smoothed bass keeps the aura present through a
    // rolling bassline; everything above bass is deliberately absent.
    let breathe = 0.20 + 0.80 * a.x;
    let ring = exp(-abs(sd) / (AURA_SOFT * (0.6 + 0.7 * punch)));
    // Inside the frame the glow dies fast — a rim on the chrome's own
    // edge, not a wash over the window's content.
    let inside = select(1.0, exp(sd * 0.6), sd < 0.0);
    var light = col * ring * inside * breathe * focus * (0.12 + 1.05 * punch);

    // Sparks: seeded by this beat's count, flown by its decay. `age` runs
    // 0 → 1 as the pulse fades, so they leap on the kick and die into the
    // dark before the next one reseeds them.
    let age = 1.0 - beat;
    let fly = beat * beat; // brightness: hot at birth, gone by the tail
    if (fly > 0.003) {
        for (var k = 0; k < SPARK_N; k = k + 1) {
            let seed = vec2<f32>(f32(k) * 1.618 + count * 0.377, count + f32(k));
            let s = aura_prng(seed);
            let p = aura_perimeter(r, s);
            let hurl = 0.4 + 0.6 * aura_prng(seed + 3.7);
            // Outward, with a sideways waggle so a burst reads as embers
            // rather than as rays.
            let side = vec2<f32>(-p.w, p.z) * (aura_prng(seed + 7.1) - 0.5) * 30.0 * age;
            let at = p.xy + p.zw * (age * SPARK_FLY * hurl) + side;
            let d2 = dot(pixel - at, pixel - at);
            let radius = 2.4 * (1.0 - 0.5 * age);
            let core = exp(-d2 / (2.0 * radius * radius));
            let halo = 0.25 * exp(-d2 / (2.0 * 9.0 * radius * radius));
            light += (core + halo) * fly * focus * mix(col, vec3<f32>(1.0), 0.6);
        }
    }

    // Premultiplied, zero alpha: the aura is light. It brightens what is
    // behind it and darkens nothing, and a window stacked above simply
    // paints over it.
    return vec4<f32>(light * audible, 0.0);
}
