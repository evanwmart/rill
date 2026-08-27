// Lofi: an anime-evening scene the desktop lives *inside*. Background
// shader (paints under the windows; must not sample `scene`).
//
// The environment knows the desktop:
//   - Windows cast soft shadows into the scene, away from the sun.
//   - A window's reflection ghosts in the pond, and its arrival (or a drag)
//     sends ripples across the water below it.
//   - A clock stands in the scene and tells the real time (fx.clock).
//   - The sky, light, and lamps follow the actual time of day.
//   - Petals drift; the dock strip is left quiet. (Petals that collide and
//     stack on window tops are slice 2 — the boids-style compute pass.)
//
// Read-only scenery: the world reacts to the desktop; it is never UI.
//
//   [desktop]
//   background_shader = "/path/to/rill/examples/shaders/lofi.wgsl"

fn hash1(n: f32) -> f32 {
    return fract(sin(n * 127.1) * 43758.5453);
}

// Signed distance to rect (x, y, w, h).
fn rect_sd(p: vec2<f32>, r: vec4<f32>) -> f32 {
    let q = abs(p - (r.xy + r.zw * 0.5)) - r.zw * 0.5;
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0);
}

// Distance to segment a..b.
fn seg_sd(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let t = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-5), 0.0, 1.0);
    return length(pa - ba * t);
}

// Rolling silhouette line: y of a hill ridge at x (normalized 0..1).
fn ridge(x: f32, seed: f32) -> f32 {
    return 0.06 * sin(x * 5.0 + seed) + 0.035 * sin(x * 11.0 + seed * 2.7)
        + 0.02 * sin(x * 23.0 + seed * 5.1);
}

@fragment
fn fs_main(in: FxIn) -> @location(0) vec4<f32> {
    let res = fx.resolution;
    let px = in.uv * res;
    let u = px / res.y; // square units, y-down
    let aspect = res.x / res.y;

    // ---- time of day -----------------------------------------------------
    let hour = fx.clock / 3600.0;
    // Daylight factor: 0 deep night, 1 full day; dawn 5–8, dusk 17–20.
    let day = smoothstep(5.0, 8.0, hour) * (1.0 - smoothstep(17.0, 20.0, hour));
    let dusk = exp(-pow((hour - 18.2) * 0.55, 2.0)) + exp(-pow((hour - 6.2) * 0.55, 2.0));
    // Sun arcs 6h→20h; the moon takes the night shift on the same rail.
    let sun_t = clamp((hour - 6.0) / 14.0, 0.0, 1.0);
    let moon_t = fract(clamp((fract((hour + 4.0) / 24.0) * 24.0) / 10.0, 0.0, 1.0));
    let body_t = select(moon_t, sun_t, day > 0.02);
    let body = vec2<f32>(
        (0.12 + 0.76 * body_t) * aspect,
        0.62 - sin(body_t * 3.14159) * 0.42,
    );

    // ---- sky -------------------------------------------------------------
    let sky_t = clamp(u.y / 0.62, 0.0, 1.0);
    let night_top = vec3<f32>(0.05, 0.06, 0.13);
    let night_hor = vec3<f32>(0.13, 0.12, 0.22);
    let day_top = vec3<f32>(0.45, 0.63, 0.78);
    let day_hor = vec3<f32>(0.78, 0.80, 0.76);
    let dusk_top = vec3<f32>(0.23, 0.16, 0.32);
    let dusk_hor = vec3<f32>(0.86, 0.48, 0.38);
    var top = mix(night_top, day_top, day);
    var hor = mix(night_hor, day_hor, day);
    top = mix(top, dusk_top, clamp(dusk, 0.0, 1.0) * 0.85);
    hor = mix(hor, dusk_hor, clamp(dusk, 0.0, 1.0) * 0.9);
    var col = mix(top, hor, pow(sky_t, 1.4));

    // Sun / moon disc with a soft bloom.
    let bd = distance(u, body);
    let disc = smoothstep(0.045, 0.041, bd);
    let bloom = exp(-bd * 9.0);
    let sun_col = mix(vec3<f32>(0.95, 0.93, 0.85), vec3<f32>(0.99, 0.62, 0.38), clamp(dusk, 0.0, 1.0));
    let moon_col = vec3<f32>(0.86, 0.88, 0.95);
    let body_col = select(moon_col, sun_col, day > 0.02);
    col += body_col * (disc * 0.9 + bloom * mix(0.12, 0.30, clamp(dusk, 0.0, 1.0)));

    // Stars after dark (static field, twinkle slow).
    if (day < 0.25) {
        let cell = floor(px / 3.0);
        let s = hash1(dot(cell, vec2<f32>(91.7, 271.3)) + hash1(cell.y) * 17.0);
        let tw = 0.6 + 0.4 * sin(time * 0.7 + s * 40.0);
        col += vec3<f32>(0.9) * step(0.9965, s) * (1.0 - day * 4.0) * tw * (1.0 - sky_t);
    }

    // ---- hills -----------------------------------------------------------
    let far_y = 0.50 + ridge(u.x, 1.0);
    let near_y = 0.58 + ridge(u.x * 1.4, 7.0);
    let haze = mix(hor, vec3<f32>(0.10, 0.10, 0.20), 0.5 + 0.3 * day);
    let far_hill = mix(haze, top * 0.55, 0.5);
    let near_hill = mix(vec3<f32>(0.08, 0.10, 0.14), vec3<f32>(0.16, 0.24, 0.20), day);
    col = mix(col, far_hill, smoothstep(far_y, far_y + 0.004, u.y));
    col = mix(col, near_hill, smoothstep(near_y, near_y + 0.004, u.y));

    // ---- cherry tree (right side): trunk + blossom cloud -----------------
    let tx = aspect - 0.30;
    let trunk = seg_sd(u, vec2<f32>(tx, 0.66), vec2<f32>(tx + 0.05, 0.40));
    let branch = seg_sd(u, vec2<f32>(tx + 0.04, 0.46), vec2<f32>(tx - 0.10, 0.34));
    col = mix(col, vec3<f32>(0.11, 0.08, 0.10), smoothstep(0.012, 0.008, min(trunk, branch)));
    var blossom = 0.0;
    for (var i = 0u; i < 12u; i = i + 1u) {
        let fi = f32(i);
        // Clusters hang off the branch line (tx+0.04,0.46)→(tx-0.10,0.34).
        let along = hash1(fi + 3.0);
        let bpos = mix(vec2<f32>(tx + 0.06, 0.47), vec2<f32>(tx - 0.12, 0.33), along);
        let c = bpos + vec2<f32>(hash1(fi + 41.0) - 0.5, hash1(fi + 11.0) - 0.5) * 0.07;
        let rad = 0.030 + 0.022 * hash1(fi + 23.0);
        blossom = max(blossom, smoothstep(rad, rad - 0.022, distance(u, c)));
    }
    let blossom_col = mix(vec3<f32>(0.38, 0.24, 0.33), vec3<f32>(0.93, 0.72, 0.78), 0.25 + 0.75 * day + 0.35 * dusk);
    col = mix(col, blossom_col, blossom * 0.9);

    // ---- pond ------------------------------------------------------------
    let pond_y = 0.70;
    let in_pond = smoothstep(pond_y, pond_y + 0.004, u.y);
    if (in_pond > 0.0) {
        // Mirror the sky with a gentle wave wobble and a darker tint.
        let my = pond_y - (u.y - pond_y) * 0.9;
        let wob = 0.006 * sin(u.x * 40.0 + time * 0.8) * (u.y - pond_y + 0.05) * 8.0;
        let msky = mix(top, hor, pow(clamp((my + wob) / 0.62, 0.0, 1.0), 1.4));
        let depth = clamp((u.y - pond_y) / 0.30, 0.0, 1.0);
        var water = mix(msky * vec3<f32>(0.30, 0.42, 0.55), vec3<f32>(0.03, 0.07, 0.12), 0.30 + 0.55 * depth);
        // The celestial body reflects as a shimmering column.
        let rb = distance(vec2<f32>(u.x, my + wob), body);
        water += body_col * exp(-rb * 12.0) * 0.35;
        col = mix(col, water, in_pond);
    }

    // ---- the desktop in the scene ---------------------------------------
    let n = min(fx_window_count, 64u);
    let light_dir = normalize(vec2<f32>(select(0.6, -0.6, body.x > u.x * 0.0 + aspect * 0.5), 0.8));
    for (var i = 0u; i < n; i = i + 1u) {
        let r = fx_windows[i];
        let wm = fx_window_meta[i];
        if (r.z <= 0.0 || wm.z > 0.5) { // dock: part of the frame, not the scene
            continue;
        }
        // Soft shadow cast into the scene, offset away from the sun/moon.
        let shadow_off = light_dir * 26.0;
        let sd = rect_sd(px - shadow_off, r);
        let inside = rect_sd(px, r);
        let shade = exp(-max(sd, 0.0) / 30.0) * smoothstep(-2.0, 6.0, inside);
        col *= 1.0 - 0.35 * shade;

        // Pond interactions, in pixel space below the window.
        let pond_px = pond_y * res.y;
        if (px.y > pond_px) {
            // Ghost reflection: the window mirrored over the pond line.
            let mirrored = vec4<f32>(r.x, 2.0 * pond_px - r.y - r.w, r.z, r.w);
            let md = rect_sd(vec2<f32>(px.x, px.y + 0.004 * res.y * sin(px.x * 0.05 + time)), mirrored);
            col = mix(col, vec3<f32>(0.10, 0.12, 0.18), 0.22 * smoothstep(3.0, -3.0, md));

            // Ripples: rings spreading from below the window on arrival or
            // while it moves.
            let foot = vec2<f32>(r.x + r.z * 0.5, pond_px);
            let dist = distance(vec2<f32>(px.x, (px.y - pond_px) * 2.6 + pond_px), foot);
            if (wm.x < 2.5) {
                let rad = wm.x * 260.0;
                let ring = exp(-abs(dist - rad) / 10.0) * (1.0 - wm.x / 2.5);
                col += vec3<f32>(0.35, 0.42, 0.50) * ring * 0.5;
            }
            let stir = clamp(wm.w / 800.0, 0.0, 1.0);
            if (stir > 0.01) {
                let ring2 = exp(-abs(dist - (60.0 + 30.0 * sin(time * 3.0))) / 14.0);
                col += vec3<f32>(0.30, 0.36, 0.44) * ring2 * stir * 0.5;
            }
        }
    }

    // ---- the clock in the world ------------------------------------------
    // A lamppost clock on the left bank; face glows after dark.
    let base = vec2<f32>(0.22, 0.635);
    let post = seg_sd(u, vec2<f32>(base.x, base.y), vec2<f32>(base.x, base.y - 0.17));
    col = mix(col, vec3<f32>(0.10, 0.09, 0.11), smoothstep(0.006, 0.004, post));
    let face_c = vec2<f32>(base.x, base.y - 0.20);
    let fd = distance(u, face_c);
    let face_glow = mix(0.0, 0.35, 1.0 - day);
    col += vec3<f32>(0.95, 0.85, 0.55) * exp(-fd * 30.0) * face_glow;
    let face = smoothstep(0.040, 0.038, fd);
    let rim = smoothstep(0.043, 0.040, fd) - smoothstep(0.038, 0.035, fd);
    col = mix(col, vec3<f32>(0.92, 0.90, 0.84), face);
    col = mix(col, vec3<f32>(0.15, 0.13, 0.12), rim);
    // Hands from the real wall clock.
    let hr = (fx.clock / 3600.0) % 12.0 / 12.0 * 6.28318 - 1.5708;
    let mn = (fx.clock / 60.0) % 60.0 / 60.0 * 6.28318 - 1.5708;
    let hr_tip = face_c + vec2<f32>(cos(hr), sin(hr)) * 0.019;
    let mn_tip = face_c + vec2<f32>(cos(mn), sin(mn)) * 0.030;
    let hands = min(seg_sd(u, face_c, hr_tip), seg_sd(u, face_c, mn_tip));
    col = mix(col, vec3<f32>(0.15, 0.13, 0.12), smoothstep(0.0035, 0.0015, hands) * face);

    // ---- drifting petals -------------------------------------------------
    for (var i = 0u; i < 18u; i = i + 1u) {
        let fi = f32(i);
        let speed = 0.014 + 0.02 * hash1(fi + 51.0);
        let phase = hash1(fi + 71.0) * 100.0;
        let py = fract(hash1(fi) + time * speed);
        let sway = 0.05 * sin(time * (0.6 + hash1(fi + 31.0)) + phase);
        let p = vec2<f32>(
            fract(hash1(fi + 13.0) + time * 0.006 + sway * 0.3) * aspect,
            0.16 + py * 0.60,
        );
        let d = distance(u, p);
        let size = 0.0040 + 0.0022 * hash1(fi + 91.0);
        col = mix(col, vec3<f32>(0.95, 0.66, 0.74), smoothstep(size, size * 0.4, d) * 0.8);
    }

    // ---- finish: vignette + a breath of grain ----------------------------
    let v = distance(in.uv, vec2<f32>(0.5));
    col *= 1.0 - 0.28 * v * v;
    col += (hash1(px.x + px.y * 917.0) - 0.5) * 0.012;

    return vec4<f32>(col, 1.0);
}
