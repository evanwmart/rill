// Abstract digital rain for Rill FX / WGSL.
//
// Self-contained:
// - no texture channels
// - slower motion
// - narrow cells for more columns
// - procedural rune / sigil-like glyphs
// - early-outs for inactive columns and cells
//
// Assumes the Rill FX preamble provides:
//   fx.resolution
//   time
//   FxIn

// Tunable from the studio (Desktop → Wallpaper) — see `// @param` in
// docs/refinement-todo.md; values live in [desktop.shader_params.matrix].
// @param speed      0.2 .. 4.0 = 1.0   "How fast the rain falls"
// @param gaps       0.0 .. 0.6 = 0.1   "Fraction of empty columns"
// @param brightness 0.2 .. 3.0 = 1.0   "Overall glow"

const CELL_SIZE: vec2<f32> = vec2<f32>(8.0, 14.0);

fn hash11(p0: f32) -> f32 {
    var p = fract(p0 * 0.1031);
    p *= p + 33.33;
    p *= p + p;
    return fract(p);
}

fn hash21(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 += dot(p3, p3.yzx + vec3<f32>(33.33));
    return fract((p3.x + p3.y) * p3.z);
}

fn segment_mask(
    p: vec2<f32>,
    a: vec2<f32>,
    b: vec2<f32>,
    width: f32,
) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let denom = max(dot(ba, ba), 0.00001);
    let h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    let d = length(pa - ba * h);

    return 1.0 - smoothstep(width, width + 0.035, d);
}

fn dot_mask(
    p: vec2<f32>,
    center: vec2<f32>,
    radius: f32,
) -> f32 {
    let d2 = dot(p - center, p - center);
    let r0 = radius * radius;
    let r1 = (radius + 0.04) * (radius + 0.04);

    return 1.0 - smoothstep(r0, r1, d2);
}

fn ring_mask(
    p: vec2<f32>,
    center: vec2<f32>,
    radius: f32,
    width: f32,
) -> f32 {
    let d = abs(length(p - center) - radius);
    return 1.0 - smoothstep(width, width + 0.035, d);
}

// Compact procedural rune.
//
// Most cells use 2-4 strokes. The symbol changes slowly, independently
// from the stream's downward movement.
fn rune(
    local: vec2<f32>,
    cell: vec2<f32>,
    epoch: f32,
) -> f32 {
    var p = (local - vec2<f32>(0.5)) * vec2<f32>(1.70, 2.00);

    // One hash family per cell/epoch, reused to derive the whole glyph.
    let base = cell + vec2<f32>(epoch * 7.13, epoch * 3.71);

    let h0 = hash21(base + vec2<f32>(0.0, 0.0));
    let h1 = hash21(base + vec2<f32>(13.1, 5.7));
    let h2 = hash21(base + vec2<f32>(2.9, 17.3));
    let h3 = hash21(base + vec2<f32>(19.7, 23.1));

    // Some glyphs become half-symmetric, producing more symbol-like forms.
    if (h0 > 0.76) {
        p.x = abs(p.x);
    }

    var g = 0.0;

    // Primary spine: vertical or diagonal.
    if (h0 < 0.48) {
        let x = (floor(h1 * 3.0) - 1.0) * 0.26;
        g = max(
            g,
            segment_mask(
                p,
                vec2<f32>(x, -0.72),
                vec2<f32>(x, 0.72),
                0.075,
            ),
        );
    } else if (h0 < 0.78) {
        g = max(
            g,
            segment_mask(
                p,
                vec2<f32>(-0.42, -0.70),
                vec2<f32>(0.42, 0.70),
                0.070,
            ),
        );
    } else {
        g = max(
            g,
            segment_mask(
                p,
                vec2<f32>(-0.42, 0.70),
                vec2<f32>(0.42, -0.70),
                0.070,
            ),
        );
    }

    // Cross-arm.
    if (h1 > 0.20) {
        let y = mix(-0.42, 0.42, h2);
        let half_width = mix(0.24, 0.48, h3);

        g = max(
            g,
            segment_mask(
                p,
                vec2<f32>(-half_width, y),
                vec2<f32>(half_width, y),
                0.065,
            ),
        );
    }

    // Secondary angular branch.
    if (h2 > 0.34) {
        let sy = select(-1.0, 1.0, h3 > 0.5);
        let sx = select(-1.0, 1.0, h1 > 0.5);

        g = max(
            g,
            segment_mask(
                p,
                vec2<f32>(0.0, 0.08 * sy),
                vec2<f32>(0.38 * sx, 0.48 * sy),
                0.060,
            ),
        );
    }

    // Sparse ring / dot punctuation.
    if (h3 < 0.22) {
        let c = vec2<f32>(
            mix(-0.28, 0.28, h1),
            mix(-0.40, 0.40, h2),
        );

        g = max(
            g,
            ring_mask(
                p,
                c,
                0.14,
                0.045,
            ),
        );
    } else if (h3 > 0.82) {
        let c = vec2<f32>(
            mix(-0.30, 0.30, h2),
            mix(-0.42, 0.42, h1),
        );

        g = max(
            g,
            dot_mask(
                p,
                c,
                0.095,
            ),
        );
    }

    // Soft crop around the glyph cell.
    let edge = max(abs(p.x), abs(p.y));
    g *= 1.0 - smoothstep(0.88, 0.98, edge);

    return clamp(g, 0.0, 1.0);
}

@fragment
fn fs_main(
    in: FxIn,
) -> @location(0) vec4<f32> {
    let resolution = max(fx.resolution, vec2<f32>(1.0));
    let frag = in.uv * resolution;

    let grid = frag / CELL_SIZE;
    let cell = floor(grid);
    let local = fract(grid);

    let rows = max(floor(resolution.y / CELL_SIZE.y), 1.0);
    let column = cell.x;
    let row = cell.y;

    let background = vec3<f32>(0.0, 0.006, 0.004);

    // Leave occasional narrow gaps between streams.
    let stream_seed = hash11(column * 0.713 + 4.19);

    if (stream_seed < param(1u)) {
        return vec4<f32>(background, 1.0);
    }

    // Long trails, but substantially slower than classic Matrix rain.
    let trail_len = mix(
        14.0,
        34.0,
        hash11(column * 1.117 + 8.73),
    );

    // Rows per second. Independent of screen resolution.
    let speed = mix(
        1.15,
        2.70,
        hash11(column * 0.337 + 1.91),
    ) * param(0u);

    let cycle = rows + trail_len;

    let phase =
        hash11(column * 0.491 + 12.7) *
        cycle;

    // Head moves from above the display toward the bottom.
    let head =
        fract(
            (time * speed + phase) /
            cycle
        ) *
        cycle -
        trail_len;

    // Positive behind the head, measured in character rows.
    let d = head - row;

    // Most fragments terminate here before doing glyph work.
    if (d < 0.0 || d > trail_len) {
        return vec4<f32>(background, 1.0);
    }

    // Reciprocal falloffs avoid exp()/pow() in the main trail shaping.
    let trail =
        1.0 /
        (
            1.0 +
            d * 0.22 +
            d * d * 0.018
        );

    let head_glow =
        1.0 /
        (
            1.0 +
            d * d * 1.65
        );

    // Change symbols slowly: roughly once every 2.8 seconds.
    let epoch =
        floor(
            time * 0.36 +
            column * 0.071 +
            row * 0.037
        );

    let glyph =
        rune(
            local,
            cell,
            epoch,
        );

    if (glyph <= 0.001) {
        // Preserve a tiny stream haze without evaluating any more hashes.
        return vec4<f32>(
            background +
            vec3<f32>(0.004, 0.025, 0.014) *
            trail,
            1.0,
        );
    }

    // Very slow cell-level intensity variation.
    let flicker =
        0.86 +
        0.14 *
        hash21(
            cell +
            vec2<f32>(
                floor(time * 0.65),
                31.7,
            )
        );

    let tail_color =
        vec3<f32>(
            0.025,
            0.44,
            0.19,
        );

    let head_color =
        vec3<f32>(
            0.56,
            1.00,
            0.82,
        );

    let brightness =
        glyph *
        trail *
        flicker *
        (
            0.72 +
            head_glow * 1.90
        ) * param(2u);

    let color =
        mix(
            tail_color,
            head_color,
            clamp(
                head_glow * 1.20,
                0.0,
                1.0,
            ),
        ) *
        brightness;

    let haze =
        vec3<f32>(
            0.008,
            0.055,
            0.028,
        ) *
        trail *
        0.35;

    return vec4<f32>(
        background +
        color +
        haze,
        1.0,
    );
}
