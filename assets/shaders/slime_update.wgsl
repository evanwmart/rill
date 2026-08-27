// slime_update.wgsl  —  `[desktop] particle_shader`
//
// Physarum. Each agent has a position and a heading, sniffs the trail at
// three points ahead of itself, turns toward whichever smells strongest, and
// deposits as it goes. That is the entire rule. The structure — veins,
// junctions, the slow reorganisation into a transport network — is not
// written anywhere; it is what a few hundred thousand agents following that
// rule do. (After Sebastian Lague's slime simulation, and Jones 2010.)
//
// Windows are part of the world. An agent will not enter one, and a window
// being *dragged* pushes the agents near it — so the colony parts around
// your windows and re-grows across the space a closed one leaves behind.
//
// State:
//   pos.xy   position in pixels
//   pos.z    depth 0..1, only so the draw pass can pick a layer
//   vel.x    heading in radians (this simulation has no velocity vector)
//   vel.y    per-agent phase, for uncorrelated jitter
//
// Written against the particle compute preamble.
//
// Physarum only becomes Physarum in bulk: the picture is made of accumulated
// trails, not of visible agents, so a few thousand of them is a faint scribble
// that never closes into a network.
// @particles 200000

// How far ahead the agent smells, and how far apart the side sensors are.
const SENSE_DISTANCE: f32 = 12.0;
const SENSE_ANGLE: f32 = 0.62;
// Radius sampled per sensor. Wider is smoother and much more stable than a
// single-texel read, which makes agents chase noise.
const SENSE_RADIUS: i32 = 1;

const MOVE_SPEED: f32 = 46.0;
const TURN_SPEED: f32 = 5.2;
// A little randomness is not decoration: with none, agents lock into the
// first ridge they find and the network stops reorganising.
const JITTER: f32 = 0.55;

const DEPOSIT: f32 = 1.0;

// Windows push agents away while moving, and are solid when still.
const WINDOW_MARGIN: f32 = 6.0;
const PUSH_REACH: f32 = 120.0;
const PUSH_STRENGTH: f32 = 0.9;

fn hash11(p: f32) -> f32 {
    return fract(sin(p * 127.1) * 43758.5453);
}

// Mean trail in a small disc around a sensor point.
fn sense(pos: vec2<f32>, heading: f32, offset: f32) -> f32 {
    let a = heading + offset;
    let at = pos + vec2<f32>(cos(a), sin(a)) * SENSE_DISTANCE;
    var sum = 0.0;
    for (var dy = -SENSE_RADIUS; dy <= SENSE_RADIUS; dy = dy + 1) {
        for (var dx = -SENSE_RADIUS; dx <= SENSE_RADIUS; dx = dx + 1) {
            sum += trail_at(at + vec2<f32>(f32(dx), f32(dy)));
        }
    }
    return sum;
}

// Is this point inside a window, and which one — windows are solid.
fn inside_window(p: vec2<f32>) -> i32 {
    for (var k = 0u; k < params.nwin; k = k + 1u) {
        let r = windows[k];
        if (p.x >= r.x - WINDOW_MARGIN && p.x <= r.x + r.z + WINDOW_MARGIN &&
            p.y >= r.y - WINDOW_MARGIN && p.y <= r.y + r.w + WINDOW_MARGIN) {
            return i32(k);
        }
    }
    return -1;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.count) { return; }
    var me = src[i];

    var heading = me.vel.x;
    let phase = me.vel.y;
    // First step: the scatter has no heading yet, so give each agent one.
    if (heading == 0.0 && phase == 0.0) {
        heading = hash11(f32(i)) * 6.2831853;
    }
    let seed = hash11(f32(i) * 0.017 + params.time * 0.37);

    // Steer toward the strongest of three samples ahead.
    let forward = sense(me.pos.xy, heading, 0.0);
    let left    = sense(me.pos.xy, heading,  SENSE_ANGLE);
    let right   = sense(me.pos.xy, heading, -SENSE_ANGLE);
    let turn = TURN_SPEED * params.dt;
    if (forward < left && forward < right) {
        // Ahead is the worst of the three — commit to a side rather than
        // dithering between two equally good ones.
        heading += (seed - 0.5) * 2.0 * turn;
    } else if (left > right) {
        heading += turn;
    } else if (right > left) {
        heading -= turn;
    }
    heading += (seed - 0.5) * JITTER * params.dt;

    // A window being dragged shoves the colony aside.
    var drift = vec2<f32>(0.0);
    for (var k = 0u; k < params.nwin; k = k + 1u) {
        let v = window_vel[k].xy;
        let speed = length(v);
        if (speed < 8.0) { continue; }
        let r = windows[k];
        let closest = clamp(me.pos.xy, r.xy, r.xy + r.zw);
        let away = me.pos.xy - closest;
        let dist = length(away);
        if (dist > PUSH_REACH) { continue; }
        let falloff = 1.0 - dist / PUSH_REACH;
        drift += v * PUSH_STRENGTH * falloff * falloff;
    }

    var pos = me.pos.xy
        + vec2<f32>(cos(heading), sin(heading)) * MOVE_SPEED * params.dt
        + drift * params.dt;

    // Windows are solid: refuse the step and turn away. Turning rather than
    // bouncing is what makes the colony *flow around* a window instead of
    // piling against it.
    if (inside_window(pos) >= 0) {
        pos = me.pos.xy;
        heading += 1.9 + seed;
    }

    // Screen edges do the same, so the colony stays in frame.
    if (pos.x < 1.0 || pos.x > params.size.x - 1.0 ||
        pos.y < 1.0 || pos.y > params.size.y - 1.0) {
        pos = clamp(pos, vec2<f32>(1.0), params.size - vec2<f32>(1.0));
        heading = heading + 3.14159 + (seed - 0.5);
    }

    // Deposit. Racy by design — see the preamble; the blur hides it.
    let at = trail_index(pos);
    if (at >= 0) {
        trail[at] = min(trail[at] + DEPOSIT, 6.0);
    }

    dst[i] = Particle(
        vec4<f32>(pos, me.pos.z, 0.0),
        vec4<f32>(heading, select(phase, hash11(f32(i) + 3.0), phase == 0.0), 0.0, 0.0),
    );
}
