// dust_update.wgsl  —  `[desktop] particle_shader`
//
// Motes of dust hanging in the air, shoved aside by a window as it is
// dragged through them, drifting back once it has passed.
//
// The behaviour that makes it read as *air* rather than as a screensaver is
// that a window pushes with its **velocity**, not with its presence. A rect
// you merely avoid gives you a force field with a hole in it; a rect that is
// moving gives you a bow wave in front, a wake behind, and stillness when it
// stops. `window_vel` is smoothed compositor-side and left to decay, so
// letting go of a drag lets the air coast to rest instead of stopping dead.
//
// State, beyond the preamble's meaning:
//   pos.z   depth in 0..1, which decides whether a mote draws behind or in
//           front of the windows. Fixed per mote — dust does not porpoise.
//   pos.w   the mote's home x, **normalised 0..1**.
//   vel.w   the mote's home y, normalised.
//
// Home is normalised rather than kept in pixels on purpose: positions are in
// output pixels, so a home in pixels is a home on the *old* screen the moment
// the output is resized — every mote would then drift back to a rectangle the
// size of whatever the desktop used to be. In 0..1 it means the same place at
// any resolution, and a resize costs nothing.
//
// Written against the particle compute preamble.
//
// Every mote is drawn, so the count is a look rather than a threshold — this
// is enough to read as air in a room without becoming a snowstorm.
// @particles 1500

// How far ahead of a moving edge the air starts to move. Wider than it looks
// like it should be: a bow wave that begins at the edge reads as a collision
// rather than as displacement.
const PUSH_REACH: f32 = 150.0;
// How hard a dragged window throws the air, per unit of its own speed.
const PUSH_STRENGTH: f32 = 2.2;
// Pull back home. Low, so motes take a second or two to settle — snapping
// back instantly reads as elastic, not as air.
const HOME_PULL: f32 = 0.55;
// Air resistance. This is what actually makes it settle rather than
// oscillate around home forever.
const DRAG: f32 = 1.9;

// Cheap hash → 0..1, for per-mote variation.
fn hash11(p: f32) -> f32 {
    return fract(sin(p * 127.1) * 43758.5453);
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.count) { return; }
    var me = src[i];

    // Home is carried, normalised, in the spare w channels. On the first
    // step they are still zero, so the initial scatter *is* home.
    var home_n = vec2<f32>(me.pos.w, me.vel.w);
    if (home_n.x == 0.0 && home_n.y == 0.0) {
        home_n = me.pos.xy / max(params.size, vec2<f32>(1.0));
    }
    // Resolved against the output as it is *this* frame, so a resize moves
    // every home with it.
    let home = home_n * params.size;

    var acc = vec2<f32>(0.0);

    // Every moving window pushes the air around it.
    for (var k = 0u; k < params.nwin; k = k + 1u) {
        let v = window_vel[k].xy;
        let speed = length(v);
        if (speed < 8.0) {
            continue;   // a window sitting still does not stir anything
        }
        let r = windows[k];
        // Distance to the window's box, zero inside it.
        let lo = r.xy;
        let hi = r.xy + r.zw;
        let closest = clamp(me.pos.xy, lo, hi);
        let away = me.pos.xy - closest;
        let dist = length(away);
        if (dist > PUSH_REACH) {
            continue;
        }
        // Falls off with distance, and with how deep the mote is: the ones
        // drawn behind the windows are further away, so they move less.
        let falloff = 1.0 - dist / PUSH_REACH;
        let depth = mix(0.45, 1.0, me.pos.z);
        // Pushed along the window's travel, and squeezed out sideways —
        // without the sideways part the air piles up in front instead of
        // parting around the edges.
        var dir = v / speed;
        if (dist > 0.5) {
            dir = normalize(dir + away / dist * 0.85);
        }
        acc += dir * speed * PUSH_STRENGTH * falloff * falloff * depth;
    }

    // The cursor stirs it too, gently — the pointer is the other thing on
    // this desktop that moves.
    let cd = me.pos.xy - params.cursor;
    let cl = length(cd);
    if (cl > 0.5 && cl < 90.0) {
        acc += cd / cl * (90.0 - cl) * 0.7;
    }

    // Home, and a slow breath so the field is never perfectly static.
    let breath = vec2<f32>(
        sin(params.time * 0.21 + hash11(f32(i)) * 6.28),
        cos(params.time * 0.17 + hash11(f32(i) + 7.0) * 6.28),
    ) * 5.0;
    acc += (home + breath - me.pos.xy) * HOME_PULL;

    var vel = me.vel.xy + acc * params.dt;
    vel -= vel * DRAG * params.dt;

    var pos = me.pos.xy + vel * params.dt;
    // Wrap rather than bounce: dust has no walls, and a mote pinned to the
    // edge of the screen is the one thing that would give the illusion away.
    // Home wraps with it, in normalised space, so the mote keeps belonging
    // where it ended up rather than being hauled back across the screen.
    if (pos.x < -20.0)                  { pos.x += params.size.x + 40.0; home_n.x += 1.0; }
    if (pos.x > params.size.x + 20.0)   { pos.x -= params.size.x + 40.0; home_n.x -= 1.0; }
    if (pos.y < -20.0)                  { pos.y += params.size.y + 40.0; home_n.y += 1.0; }
    if (pos.y > params.size.y + 20.0)   { pos.y -= params.size.y + 40.0; home_n.y -= 1.0; }
    home_n = fract(home_n);

    dst[i] = Particle(
        vec4<f32>(pos, me.pos.z, home_n.x),
        vec4<f32>(vel, 0.0, home_n.y),
    );
}
