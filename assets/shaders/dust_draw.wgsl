// dust_draw.wgsl  —  `[desktop] particle_render`
//
// One soft round mote per particle: two triangles, faded toward the edge in
// the fragment stage so there is no hard rim at this size. Depth (`pos.z`)
// decides both which side of the window stack a mote is drawn on and how
// big and bright it is, so the field genuinely has near and far in it.
//
// A mote also brightens with speed. That is what makes the push visible:
// the air lights up exactly where a window is shoving it, and dims again as
// it settles.
//
// Written against the particle render preamble.

const SIZE: f32 = 2.6;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let p = particles[ii];
    let z = p.pos.z;
    // Half the field behind the windows, half in front.
    if ((z >= 0.5) != (layer_front == 1u)) {
        return skip();
    }

    // Two triangles as a quad, in the mote's own local space.
    var corner = vec2<f32>(-1.0, -1.0);
    switch vi {
        case 0u, 3u: { corner = vec2<f32>(-1.0, -1.0); }
        case 1u:     { corner = vec2<f32>( 1.0, -1.0); }
        case 2u, 4u: { corner = vec2<f32>( 1.0,  1.0); }
        default:     { corner = vec2<f32>(-1.0,  1.0); }
    }

    // Nearer motes are bigger. Faster ones stretch very slightly along their
    // travel, which reads as motion without becoming a streak.
    let scale = mix(0.55, 1.7, z) * SIZE;
    let speed = length(p.vel.xy);
    let stretch = 1.0 + min(speed / 900.0, 0.6);
    var dir = vec2<f32>(1.0, 0.0);
    if (speed > 1.0) {
        dir = p.vel.xy / speed;
    }
    let side = vec2<f32>(-dir.y, dir.x);
    let local = dir * corner.x * scale * stretch + side * corner.y * scale;

    var out: VsOut;
    out.clip = to_clip(p.pos.xy + local);
    // Brightness: far motes dimmer, moving motes brighter. `color.a` carries
    // the alpha and the fragment stage shapes it into a disc.
    let lit = mix(0.16, 0.42, z) + min(speed / 700.0, 0.5);
    out.color = vec4<f32>(corner, lit, mix(0.35, 0.9, z));
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // `in.color.xy` is the interpolated corner, so its length is the
    // distance from the mote's centre in local units: >1 is outside the
    // disc, and the last third is the soft edge.
    let d = length(in.color.xy);
    if (d > 1.0) {
        discard;
    }
    let soft = smoothstep(1.0, 0.35, d);
    let lit = in.color.z;
    // Warm white, so it reads as lit air rather than as pixels.
    return vec4<f32>(vec3<f32>(1.0, 0.94, 0.86) * lit, in.color.w * soft);
}
