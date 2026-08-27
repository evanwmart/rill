// Boid rendering, straight from the simulation's storage buffer. Each
// instance emits six vertices: a soft drop-shadow triangle (offset by
// altitude — depth is height) then the velocity-oriented body. Depth also
// scales size and brightness, and selects the layer: the `layer_front`
// uniform (0 = behind windows, 1 = in front) discards out-of-band boids
// by emitting off-screen degenerate vertices.
// Bindings, `VsOut`, `to_clip` and `skip` come from the particle render
// preamble.

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    var out: VsOut;
    let b = particles[ii];
    let z = b.pos.z;
    let in_front = z >= 0.5;
    if (in_front != (layer_front == 1u)) {
        return skip();
    }

    let dir = normalize(b.vel.xy + vec2<f32>(0.001, 0.0));
    let side = vec2<f32>(-dir.y, dir.x);
    let lvi = vi % 3u;
    var local: vec2<f32>;
    switch lvi {
        case 0u: { local = dir * 6.5; }
        case 1u: { local = -dir * 4.0 + side * 3.2; }
        default: { local = -dir * 4.0 - side * 3.2; }
    }
    // Depth = altitude: higher boids are bigger, brighter, and throw their
    // shadow further.
    let scale = mix(0.55, 1.55, z);
    let shadow = vi < 3u;
    var px = b.pos.xy + local * scale * select(1.0, 1.2, shadow);
    if (shadow) {
        px += vec2<f32>(7.0, 11.0) * mix(0.5, 1.8, z);
    }
    out.clip = to_clip(px);
    if (shadow) {
        out.color = vec4<f32>(0.0, 0.0, 0.0, 0.18 * scale);
    } else {
        let t = fract(f32(ii) * 0.61803398);
        let c1 = vec3<f32>(0.43, 0.66, 1.0);
        let c2 = vec3<f32>(1.0, 0.48, 0.78);
        out.color = vec4<f32>(mix(c1, c2, t), mix(0.55, 0.95, z));
    }
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
