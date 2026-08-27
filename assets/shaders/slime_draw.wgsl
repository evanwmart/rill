// slime_draw.wgsl  —  `[desktop] particle_render`
//
// Colours the trail field rather than drawing the agents. The agents are
// only a few pixels of deposit each; the *structure* — the veins and
// junctions the colony builds — lives entirely in the field, so that is
// what gets painted.
//
// The pass is instanced once per agent, which is the wrong shape for a
// fullscreen draw, so instance 0 emits the quad and every other instance
// skips immediately. Those skipped invocations cost a clipped vertex each
// and nothing more.
//
// Written against the particle render preamble.

// Deep base → warm vein → hot core. Dark enough at the low end that empty
// ground reads as desktop rather than as a grey wash.
const COOL: vec3<f32> = vec3<f32>(0.05, 0.10, 0.16);
const MID:  vec3<f32> = vec3<f32>(0.15, 0.55, 0.62);
const HOT:  vec3<f32> = vec3<f32>(0.98, 0.86, 0.55);

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    // One fullscreen quad, from one instance, in the back layer.
    if (ii != 0u || layer_front == 1u) {
        return skip();
    }
    var corner = vec2<f32>(0.0, 0.0);
    switch vi {
        case 0u, 3u: { corner = vec2<f32>(0.0, 0.0); }
        case 1u:     { corner = vec2<f32>(1.0, 0.0); }
        case 2u, 4u: { corner = vec2<f32>(1.0, 1.0); }
        default:     { corner = vec2<f32>(0.0, 1.0); }
    }
    var out: VsOut;
    out.clip = to_clip(corner * viewport);
    // Carry the pixel position through so the fragment stage can read the
    // field; `color` is just the interpolator that happens to be here.
    out.color = vec4<f32>(corner * viewport, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let v = trail_at_pixel(in.color.xy);
    // Knee low so faint exploratory trails still show; the veins saturate
    // well before the deposit cap.
    let t = clamp(v * 0.85, 0.0, 1.0);
    var rgb = mix(COOL, MID, smoothstep(0.0, 0.45, t));
    rgb = mix(rgb, HOT, smoothstep(0.55, 1.0, t));
    // Alpha tracks intensity, so bare ground stays transparent and whatever
    // the desktop puts behind this shows through instead of a flat field.
    let a = smoothstep(0.02, 0.35, t);
    return vec4<f32>(rgb * a, a);
}
