// Boid flock step in 2.5D: separation/alignment/cohesion in the screen
// plane plus a gentle depth dimension (pos.z in 0..1). Depth drives which
// side of the window stack a boid renders on (< 0.5 behind, >= 0.5 in
// front), so the flock genuinely weaves through the desktop. Window rects
// are obstacles for the deep half; the cursor is a curiosity field.
// State, params and the window arrays come from the particle compute
// preamble — this is the built-in flock written against that contract, and
// so is a worked example of one.

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.count) { return; }
    var me = src[i];

    var sep = vec2<f32>(0.0);
    var ali = vec3<f32>(0.0);
    var coh = vec3<f32>(0.0);
    var n = 0.0;
    for (var j = 0u; j < params.count; j = j + 1u) {
        if (j == i) { continue; }
        let d = src[j].pos.xy - me.pos.xy;
        let d2 = dot(d, d);
        if (d2 < 4900.0) {              // 70px neighborhood — loose flocks
            n += 1.0;
            ali += src[j].vel.xyz;
            coh += src[j].pos.xyz;
            if (d2 < 1600.0) {          // 40px personal space — sparse
                sep -= d / max(d2, 1.0) * 40.0;
            }
        }
    }
    var acc = vec3<f32>(0.0);
    if (n > 0.0) {
        let ali_avg = ali / n - me.vel.xyz;
        let coh_avg = coh / n - me.pos.xyz;
        acc += vec3<f32>(ali_avg.xy * 0.9, ali_avg.z * 0.4);
        acc += vec3<f32>(coh_avg.xy * 0.25, coh_avg.z * 0.05);
    }
    acc += vec3<f32>(sep * 16.0, 0.0);

    // Windows are terrain for the deep half of the flock; front-flyers
    // glide over them untouched.
    let avoid_w = 1.0 - smoothstep(0.30, 0.60, me.pos.z);
    for (var k = 0u; k < params.nwin; k = k + 1u) {
        let r = windows[k];
        let m = 14.0;
        let lo = r.xy - vec2<f32>(m);
        let hi = r.xy + r.zw + vec2<f32>(m);
        let cp = clamp(me.pos.xy, lo, hi);
        let d = me.pos.xy - cp;
        let d2 = dot(d, d);
        if (d2 <= 0.0) {
            let c = (lo + hi) * 0.5;
            let q = (me.pos.xy - c) / max(hi - c, vec2<f32>(1.0));
            if (abs(q.x) > abs(q.y)) {
                acc.x += select(-900.0, 900.0, q.x >= 0.0) * avoid_w;
            } else {
                acc.y += select(-900.0, 900.0, q.y >= 0.0) * avoid_w;
            }
            // Being inside a window also nudges a deep boid up and over.
            acc.z += 0.35 * avoid_w;
        } else if (d2 < 3600.0) {
            let dist = sqrt(d2);
            acc += vec3<f32>(d / dist * (60.0 - dist) * 16.0 * avoid_w, 0.0);
        }
    }

    // The cursor: curious from afar, respectful up close.
    let cd = params.cursor - me.pos.xy;
    let cl = length(cd);
    if (cl > 1.0 && cl < 260.0) {
        if (cl > 70.0) {
            acc += vec3<f32>(cd / cl * 22.0, 0.0);
        } else {
            acc -= vec3<f32>(cd / cl * 280.0, 0.0);
        }
    }

    // Depth wander: each boid slowly porpoises through the z band, so the
    // flock keeps trading places with the window stack.
    acc.z += sin(params.time * 0.35 + f32(i) * 0.73) * 0.05;
    if (me.pos.z < 0.10) { acc.z += 0.20; }
    if (me.pos.z > 0.90) { acc.z -= 0.20; }

    // Screen bounds: a wide margin with force proportional to intrusion —
    // a constant nudge loses to flock pressure and boids pin on the edge.
    let m2 = 70.0;
    acc.x += max(m2 - me.pos.x, 0.0) * 6.0;
    acc.y += max(m2 - me.pos.y, 0.0) * 6.0;
    acc.x -= max(me.pos.x - (params.size.x - m2), 0.0) * 6.0;
    acc.y -= max(me.pos.y - (params.size.y - m2), 0.0) * 6.0;

    var vel = me.vel.xyz + acc * params.dt;
    let sp = length(vel.xy);
    if (sp > 0.001) {
        let spc = clamp(sp, 60.0, 200.0);
        vel = vec3<f32>(vel.xy / sp * spc, clamp(vel.z, -0.08, 0.08));
    }
    var pos_xy = me.pos.xy + vel.xy * params.dt;
    // Hard boundary bounces: clamping position while velocity still points
    // outward is what made boids stick and slide along the edge — reflect
    // the offending component inward instead.
    if (pos_xy.x < 2.0) { pos_xy.x = 2.0; vel.x = abs(vel.x); }
    if (pos_xy.y < 2.0) { pos_xy.y = 2.0; vel.y = abs(vel.y); }
    if (pos_xy.x > params.size.x - 2.0) { pos_xy.x = params.size.x - 2.0; vel.x = -abs(vel.x); }
    if (pos_xy.y > params.size.y - 2.0) { pos_xy.y = params.size.y - 2.0; vel.y = -abs(vel.y); }
    let pos = vec3<f32>(pos_xy, clamp(me.pos.z + vel.z * params.dt, 0.02, 0.98));
    dst[i] = Particle(vec4<f32>(pos, 0.0), vec4<f32>(vel, 0.0));
}
