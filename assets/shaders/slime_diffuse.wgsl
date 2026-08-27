// slime_diffuse.wgsl  —  `[desktop] particle_diffuse`
//
// The other half of Physarum, and the half that does the shaping: blur the
// trail a little and fade it a lot, once per pixel per frame.
//
// Without this the deposits are just a scribble of where agents have been.
// With it, nearby deposits merge into ridges an agent can smell from further
// away, and unvisited ground fades — which is what turns wandering into a
// network that reorganises. Diffusion builds the roads; decay closes the
// ones nobody uses.
//
// Dispatched one invocation per *pixel* (16x16 workgroups), reading `trail`
// and writing `trail_next`.

const DECAY_PER_SECOND: f32 = 0.62;
// How much of each pixel is replaced by the blurred neighbourhood.
const DIFFUSE_RATE: f32 = 9.0;

@compute @workgroup_size(16, 16)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(params.size.x);
    let h = u32(params.size.y);
    if (gid.x >= w || gid.y >= h) { return; }
    let at = gid.y * w + gid.x;

    // 3x3 mean. Clamped at the edges rather than wrapped, so the colony does
    // not leak across the screen.
    var sum = 0.0;
    for (var dy = -1; dy <= 1; dy = dy + 1) {
        for (var dx = -1; dx <= 1; dx = dx + 1) {
            let x = clamp(i32(gid.x) + dx, 0, i32(w) - 1);
            let y = clamp(i32(gid.y) + dy, 0, i32(h) - 1);
            sum += trail[u32(y) * w + u32(x)];
        }
    }
    let blurred = sum / 9.0;

    let original = trail[at];
    let mixed = mix(original, blurred, clamp(DIFFUSE_RATE * params.dt, 0.0, 1.0));
    trail_next[at] = max(mixed - DECAY_PER_SECOND * params.dt, 0.0);
}
