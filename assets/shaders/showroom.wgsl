// Showroom: a cinematic studio built *around* the model layer. Background
// shader (paints under the windows; must not sample `scene`).
//
// This is the scene half of a showcase. It shares the model shader's camera
// and floor plane verbatim — see the CONTRACT block below — so the ground
// this paints is the ground the car's wheels rest on, its contact shadow
// turns with the turntable, and the model pass's mirrored instance lands
// exactly on the floor drawn here.
//
//   [desktop]
//   background_shader = ".../examples/shaders/showroom.wgsl"
//   model             = ".../Shelby.obj"
//   model_shader      = ".../examples/models/shelby_cinematic.wgsl"
//
// Pair it with a different model and only the shadow footprint needs a
// re-tune (SHADOW_LONG / SHADOW_WIDE).

// ---- CONTRACT: identical to shelby_cinematic.wgsl --------------------
const EYE_DIR: vec3<f32> = vec3<f32>(0.0, 0.46, 3.85);
const FOCUS: vec3<f32> = vec3<f32>(0.0, -0.04, 0.0);
const FOV_Y: f32 = 0.5934119; // 34 degrees
const FLOOR_Y: f32 = -0.412465;
// Contact-shadow footprint, in world units (the Cobra is ~2.8 long).
const SHADOW_LONG: f32 = 1.42;
const SHADOW_WIDE: f32 = 0.66;

const PI: f32 = 3.14159265359;

fn hash12(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    return clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14),
                 vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let res = fx.resolution;
    let aspect = max(res.x / max(res.y, 1.0), 0.01);

    // Rebuild the model shader's camera ray for this pixel, so screen space
    // and world space agree between the two passes.
    // Camera distance is a scene knob; the model shader pulls back the same
    // way, so the two views stay locked together.
    let eye = FOCUS + normalize(EYE_DIR - FOCUS) * max(studio.motion.z, 0.6);
    let fwd = normalize(FOCUS - eye);
    let right = normalize(cross(fwd, vec3<f32>(0.0, 1.0, 0.0)));
    let up = cross(right, fwd);
    let tan_half = tan(FOV_Y * 0.5);
    let ndc_x = (in.uv.x * 2.0 - 1.0) * aspect * tan_half;
    let ndc_y = (1.0 - in.uv.y * 2.0) * tan_half;
    let dir = normalize(fwd + right * ndc_x + up * ndc_y);

    let angle = time * studio.motion.x + studio.motion.y;
    // The turntable's long axis, matching rotate_y in the model shader.
    let long_axis = vec2<f32>(cos(angle), -sin(angle));
    let wide_axis = vec2<f32>(sin(angle), cos(angle));

    // Where each light stands on the floor: walk out along its direction
    // from the object, so moving a light moves its pool — the room and the
    // car agree because they read the same vectors.
    let key_pos = normalize(studio.key.xz + vec2<f32>(1e-4, 0.0)) * 2.7;
    let fill_pos = normalize(studio.fill.xz + vec2<f32>(1e-4, 0.0)) * 2.7;

    var col: vec3<f32>;

    let hits_ground = dir.y < -1e-4;
    var horizon_blend = 0.0;
    if (hits_ground) {
        let t = (FLOOR_Y - eye.y) / dir.y;
        let hit = eye + dir * t;
        let q = vec2<f32>(hit.x, hit.z);
        let dist = length(q);

        // Polished charcoal floor: a dark base lifted by two light pools.
        var ground = studio.ground_color.rgb;
        let key_pool = exp(-distance(q, key_pos) * 0.62);
        ground += studio.key_color.rgb * key_pool * 0.60 * clamp(studio.key.w / 7.2, 0.2, 2.0);
        if (studio.fill.w > 0.0) {
            let fill_pool = exp(-distance(q, fill_pos) * 0.70);
            ground += studio.fill_color.rgb * fill_pool * 0.48 * clamp(studio.fill.w / 1.8, 0.2, 2.0);
        }

        // A slow sweep, like a light being walked around the car.
        let sweep_a = time * 0.22;
        let sweep = vec2<f32>(cos(sweep_a), sin(sweep_a)) * 3.1;
        ground += vec3<f32>(0.20, 0.20, 0.26) * exp(-distance(q, sweep) * 0.75) * 0.5;

        // Turntable: a faint disc edge and two concentric scribes.
        let rings = studio.backdrop_color.w;
        let ring = abs(dist - 2.05);
        ground += vec3<f32>(0.10, 0.11, 0.14) * exp(-ring * 22.0) * rings;
        ground += vec3<f32>(0.05, 0.055, 0.07) * exp(-abs(dist - 2.35) * 30.0) * rings;
        ground *= 1.0 - 0.10 * smoothstep(2.05, 2.5, dist);

        // Contact shadow: an ellipse under the car, turning with it, plus a
        // tighter core so the wheels feel planted rather than hovering.
        let along = dot(q, long_axis) / SHADOW_LONG;
        let across = dot(q, wide_axis) / SHADOW_WIDE;
        let e = length(vec2<f32>(along, across));
        let soft = 1.0 - smoothstep(0.55, 1.45, e);
        let core = 1.0 - smoothstep(0.10, 0.85, e);
        ground *= 1.0 - 0.82 * soft;
        ground *= 1.0 - 0.68 * core;

        // Fade into the cove as the floor runs away from the camera.
        horizon_blend = smoothstep(9.0, 26.0, t);
        col = ground;
    } else {
        col = vec3<f32>(0.0);
        horizon_blend = 1.0;
    }

    // Infinity cove: a soft vertical wash with the same two lights bouncing
    // off it, so the floor dissolves into the backdrop without a seam.
    let h = clamp(dir.y * 1.6 + 0.30, 0.0, 1.0);
    // The backdrop has its own colour: a warm key should not drag the whole
    // room orange. `backdrop_glow` says how much light bounces onto it.
    let cove_base = studio.backdrop_color.rgb;
    var cove = mix(cove_base, cove_base * 0.35, pow(h, 0.75));
    let glow = studio.finish.z;
    if (glow > 0.001) {
        // The lights bounce off the cove from behind the object, so their
        // glow sits opposite where they stand.
        let warm_dir = normalize(vec3<f32>(-studio.key.x, 0.22, -abs(studio.key.z) - 0.3));
        cove += studio.key_color.rgb * pow(max(dot(dir, warm_dir), 0.0), 5.0) * 0.55 * glow;
        if (studio.fill.w > 0.0) {
            let cool_dir = normalize(vec3<f32>(-studio.fill.x, 0.16, -abs(studio.fill.z) - 0.3));
            cove += studio.fill_color.rgb * pow(max(dot(dir, cool_dir), 0.0), 6.0) * 0.45 * glow;
        }
    }
    col = mix(col, cove, horizon_blend);

    // The desktop still lives here: windows dim the studio behind them, so
    // the scene reads as a room the UI is standing in.
    let px = in.uv * res;
    let n = min(fx_window_count, 64u);
    for (var i = 0u; i < n; i = i + 1u) {
        let r = fx_windows[i];
        let wm = fx_window_meta[i];
        if (r.z <= 0.0 || wm.z > 0.5) {
            continue;
        }
        let c = r.xy + r.zw * 0.5;
        let d = abs(px - c) - r.zw * 0.5;
        let sd = length(max(d, vec2<f32>(0.0))) + min(max(d.x, d.y), 0.0);
        col *= 1.0 - 0.30 * exp(-max(sd, 0.0) / 46.0);
    }

    col = aces(col * 1.15 * max(studio.motion.w, 0.05));
    let v = distance(in.uv, vec2<f32>(0.5));
    col *= 1.0 - studio.finish.w * v * v;
    col += (hash12(px) - 0.5) / 220.0;
    return vec4<f32>(col, 1.0);
}
