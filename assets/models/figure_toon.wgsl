// Figure (toon): a generic model shader for meshes that carry no materials
// — STL prints, untextured OBJs, anything you just dropped in.
//
// Nothing about one model is baked in here. The renderer hands over the
// mesh's own bounds, so this centres it, stands it on the showroom floor,
// and scales it to a comfortable height whatever units it was authored in.
// Shading is cel-banded with a rim light: kind to faceted STL normals,
// which physically-based shading only makes look worse.
//
// Materials, when a mesh has several parts (a directory of STLs loads as
// one mesh with an id per file), tint by index rather than pretending to
// know what they are.
//
//   [desktop.showroom]
//   model = "~/.config/rill/models/Pikachu"      # a dir of parts, or one file

struct Frame {
    resolution: vec2<f32>,
    time: f32,
    exposure: f32,
    bounds_min: vec4<f32>,
    bounds_max: vec4<f32>,
}

@group(0) @binding(0)
var<uniform> frame: Frame;

struct Studio {
    key: vec4<f32>,
    key_color: vec4<f32>,
    fill: vec4<f32>,
    fill_color: vec4<f32>,
    rim_color: vec4<f32>,
    body_color: vec4<f32>,
    ground_color: vec4<f32>,
    motion: vec4<f32>,
    backdrop_color: vec4<f32>,
    finish: vec4<f32>,
    fit: vec4<f32>,
}

@group(0) @binding(1)
var<uniform> studio: Studio;

struct VertexIn {
    @builtin(instance_index) instance: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) material_id: u32,
}

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) @interpolate(flat) material_id: u32,
    @location(3) @interpolate(flat) mirrored: f32,
    /// Height through the model, 0 at the feet — drives the ground tint.
    @location(4) up: f32,
}

const PI: f32 = 3.14159265359;
// The showroom's floor plane, shared with examples/shaders/showroom.wgsl.
const FLOOR_Y: f32 = -0.412465;


fn rotate_y(p: vec3<f32>, angle: f32) -> vec3<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return vec3<f32>(c * p.x + s * p.z, p.y, -s * p.x + c * p.z);
}

fn look_at(eye: vec3<f32>, focus: vec3<f32>, up_hint: vec3<f32>) -> mat4x4<f32> {
    let f = normalize(focus - eye);
    let s = normalize(cross(f, up_hint));
    let u = cross(s, f);
    return mat4x4<f32>(
        vec4<f32>(s.x, u.x, -f.x, 0.0),
        vec4<f32>(s.y, u.y, -f.y, 0.0),
        vec4<f32>(s.z, u.z, -f.z, 0.0),
        vec4<f32>(-dot(s, eye), -dot(u, eye), dot(f, eye), 1.0),
    );
}

fn perspective(fovy: f32, aspect: f32, z_near: f32, z_far: f32) -> mat4x4<f32> {
    let f = 1.0 / tan(fovy * 0.5);
    let nf = 1.0 / (z_near - z_far);
    return mat4x4<f32>(
        vec4<f32>(f / aspect, 0.0, 0.0, 0.0),
        vec4<f32>(0.0, f, 0.0, 0.0),
        vec4<f32>(0.0, 0.0, z_far * nf, -1.0),
        vec4<f32>(0.0, 0.0, z_near * z_far * nf, 0.0),
    );
}

fn camera_position() -> vec3<f32> {
    let focus = vec3<f32>(0.0, -0.04, 0.0);
    let dir = normalize(vec3<f32>(0.0, 0.46, 3.85) - focus);
    return focus + dir * max(studio.motion.z, 0.6);
}

/// Rotate a vector from the model's authored up-axis into the renderer's
/// Y-up world. `model_up` names it: "y" (default), "z", "-y", "-z" — a
/// print exported from a slicer is as likely to be any of them, and
/// guessing stands a model on its face.
fn upright(v: vec3<f32>) -> vec3<f32> {
    let axis = i32(studio.fit.x + 0.5);
    switch axis {
        case 1: { return vec3<f32>(v.x, v.z, -v.y); }   // z-up
        case 2: { return vec3<f32>(v.x, -v.y, -v.z); }  // y-down
        case 3: { return vec3<f32>(v.x, -v.z, v.y); }   // -z up
        default: { return v; }
    }
}

/// Auto-fit: centre on the mesh's own bounds, scale to the configured
/// height, and stand it on the floor — so any mesh, in any units, arrives
/// framed instead of as a speck or a wall.
fn fit(p_in: vec3<f32>) -> vec3<f32> {
    let lo = upright(frame.bounds_min.xyz);
    let hi = upright(frame.bounds_max.xyz);
    let a = min(lo, hi);
    let b = max(lo, hi);
    let size = max(b - a, vec3<f32>(1e-4));
    // Fit the largest dimension into a size in *world* units — so the
    // camera's distance does what a camera's distance should: move away and
    // the subject gets smaller. `model_scale` is a multiplier on a base
    // that frames well at the default distance.
    let want = 1.5 * clamp(studio.fit.y, 0.1, 4.0);
    let scale = want / max(size.x, max(size.y, size.z));
    let centre = vec3<f32>(a.x + size.x * 0.5, a.y, a.z + size.z * 0.5);
    return (upright(p_in) - centre) * scale
        + vec3<f32>(0.0, FLOOR_Y + studio.fit.z, 0.0);
}

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    let angle = frame.time * studio.motion.x + studio.motion.y;
    var p = rotate_y(fit(in.position), angle);
    var n = normalize(rotate_y(upright(in.normal), angle));

    let mirrored = f32(in.instance);
    if (mirrored > 0.5) {
        p.y = 2.0 * FLOOR_Y - p.y;
        n.y = -n.y;
    }

    let eye = camera_position();
    let view = look_at(eye, vec3<f32>(0.0, -0.04, 0.0), vec3<f32>(0.0, 1.0, 0.0));
    let aspect = max(frame.resolution.x / max(frame.resolution.y, 1.0), 0.01);
    let proj = perspective(34.0 * PI / 180.0, aspect, 0.05, 100.0);

    var out: VertexOut;
    out.position = proj * view * vec4<f32>(p, 1.0);
    out.world_pos = p;
    out.world_normal = n;
    out.material_id = in.material_id;
    out.mirrored = mirrored;
    out.up = clamp((p.y - FLOOR_Y) / 1.4, 0.0, 1.0);
    return out;
}

/// Surface colour. Generic here — parts by index, since a mesh loaded from
/// a directory gets one id per file and the shader has no business guessing
/// what they are. A per-model shader replaces this function and may use the
/// extra arguments: `up` is height through the model (0 at the feet), `n`
/// the normal, `p` the world position. `body_color` overrides part 0.
fn surface_color(id: u32, up: f32, n: vec3<f32>, p: vec3<f32>) -> vec3<f32> {
    var c = vec3<f32>(0.82, 0.80, 0.78);
    switch id {
        case 0u: { c = vec3<f32>(0.86, 0.83, 0.80); }
        case 1u: { c = vec3<f32>(0.42, 0.45, 0.52); }
        case 2u: { c = vec3<f32>(0.72, 0.66, 0.58); }
        case 3u: { c = vec3<f32>(0.58, 0.62, 0.70); }
        default: { c = vec3<f32>(0.78, 0.76, 0.74); }
    }
    if (id == 0u && studio.body_color.w >= 0.5) {
        c = studio.body_color.rgb;
    }
    return c;
}

/// Cel banding: quantise the lambert term into a few steps with soft edges,
/// which is what makes a faceted mesh read as a figure rather than a print.
fn band(x: f32) -> f32 {
    let steps = 4.0;
    let q = floor(x * steps) / steps;
    let f = fract(x * steps);
    return q + smoothstep(0.35, 0.65, f) / steps;
}

@fragment
fn fs_main(in: VertexOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    var n = normalize(in.world_normal);
    if (!front) {
        n = -n;
    }
    let v = normalize(camera_position() - in.world_pos);
    let base = surface_color(in.material_id, in.up, n, in.world_pos);

    let key_l = normalize(studio.key.xyz);
    let key_amount = band(max(dot(n, key_l), 0.0)) * clamp(studio.key.w / 7.2, 0.1, 2.0);
    var col = base * studio.key_color.rgb * key_amount;

    if (studio.fill.w > 0.0) {
        let fill_l = normalize(studio.fill.xyz);
        let fill_amount = band(max(dot(n, fill_l), 0.0) * 0.6)
            * clamp(studio.fill.w / 1.8, 0.1, 2.0);
        col += base * studio.fill_color.rgb * fill_amount * 0.45;
    }

    // Ambient: mostly neutral, so a surface in shadow keeps its own colour
    // (a coloured ambient turns an unlit yellow body backdrop-blue), with a
    // little of the room mixed in and a ground bounce at the feet.
    col += base * (0.30 + 0.08 * (1.0 - in.up));
    col += base * mix(studio.ground_color.rgb * 1.6, studio.backdrop_color.rgb, in.up) * 0.35;

    // Rim: the separation light, strongest at the silhouette.
    let rim = pow(1.0 - max(dot(n, v), 0.0), 3.0);
    col += studio.rim_color.rgb * rim * clamp(studio.rim_color.w / 2.6, 0.0, 2.0) * 0.35;

    // A soft specular sheen — enough to read as surface, not as plastic.
    let h = normalize(v + key_l);
    col += studio.key_color.rgb * pow(max(dot(n, h), 0.0), 42.0) * 0.25;

    col = clamp(col * max(studio.motion.w, 0.05), vec3<f32>(0.0), vec3<f32>(1.0));

    if (in.mirrored > 0.5) {
        let strength = studio.finish.x;
        if (strength <= 0.001) {
            discard;
        }
        let below = clamp((FLOOR_Y - in.world_pos.y) / max(studio.finish.y, 0.05), 0.0, 1.0);
        let fade = (1.0 - below) * (1.0 - below);
        return vec4<f32>(mix(col, studio.ground_color.rgb, 0.58) * 0.5, strength * fade);
    }
    return vec4<f32>(col, 1.0);
}
