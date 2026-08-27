//! Declared shader parameters: `// @param` lines read out of WGSL source.
//!
//! The declaration lives with the thing it describes — the model `@particles`
//! proved — so a shader stays one file to add and one file to tune:
//!
//! ```wgsl
//! // @param sense_distance  4.0 .. 40.0 = 12.0   "How far ahead it smells"
//! // @param decay           0.1 ..  3.0 =  0.62  "How fast trails fade"
//! ```
//!
//! Parameters are *values*, not code: a fixed-size uniform block
//! (`fx_params`, [`SLOT_COUNT`] vec4s) and one scalar type is the whole
//! vocabulary, and staying that small is what keeps this inside the
//! inert-content boundary (docs/risks.md #6). The studio renders one slider
//! per declaration; the compositor uploads the values; the shader reads
//! `param(i)` where it used a const.
//!
//! Shared between the studio (which draws the sliders and writes
//! `[desktop.shader_params.<stem>]`) and the compositor (which overlays those
//! values on the declared defaults) for the same reason `rices` is: a format
//! parsed in two places is two formats within the month.

/// How many vec4 rows the uniform block carries.
pub const SLOT_COUNT: usize = 8;
/// One scalar per slot lane.
pub const MAX_PARAMS: usize = SLOT_COUNT * 4;

/// One declared parameter, in declaration order — the order *is* the uniform
/// index, so reordering lines in the shader reorders the block.
#[derive(Debug, Clone, PartialEq)]
pub struct ShaderParam {
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// The human sentence from the declaration, shown beside the slider.
    pub doc: String,
}

/// Parse every well-formed `// @param name lo .. hi = default "doc"` line,
/// in order, capped at [`MAX_PARAMS`]. A malformed line is skipped rather
/// than fatal — a shader that misdeclares a knob still runs; it just cannot
/// be tuned — and a default outside its own range is clamped into it.
pub fn shader_params(source: &str) -> Vec<ShaderParam> {
    let mut out = Vec::new();
    for line in source.lines() {
        let Some(rest) = line.trim_start().strip_prefix("//") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix("@param") else { continue };
        let Some(param) = parse_line(rest) else { continue };
        if out.len() == MAX_PARAMS {
            break;
        }
        out.push(param);
    }
    out
}

fn parse_line(rest: &str) -> Option<ShaderParam> {
    // name  lo .. hi = default  "doc"
    let rest = rest.trim_start();
    let name_end = rest.find(char::is_whitespace)?;
    let name = &rest[..name_end];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let rest = rest[name_end..].trim_start();
    let eq = rest.find('=')?;
    let (range, rest) = (&rest[..eq], rest[eq + 1..].trim_start());
    let (lo, hi) = range.split_once("..")?;
    let min: f32 = lo.trim().parse().ok().filter(|v: &f32| v.is_finite())?;
    let max: f32 = hi.trim().parse().ok().filter(|v: &f32| v.is_finite())?;
    if min >= max {
        return None;
    }
    // Default runs to the doc string (or the end of the line).
    let (default_text, doc) = match rest.find('"') {
        Some(q) => {
            let doc = rest[q + 1..].split('"').next().unwrap_or("").trim().to_string();
            (rest[..q].trim(), doc)
        }
        None => (rest.trim(), String::new()),
    };
    let default: f32 = default_text.parse().ok().filter(|v: &f32| v.is_finite())?;
    Some(ShaderParam {
        name: name.to_string(),
        min,
        max,
        default: default.clamp(min, max),
        doc,
    })
}

/// Pack values into the uniform rows, one scalar per lane in declaration
/// order; unused lanes are zero. `values` longer than the block is truncated
/// — the parser already refuses to declare more.
pub fn pack(values: &[f32]) -> [[f32; 4]; SLOT_COUNT] {
    let mut rows = [[0.0f32; 4]; SLOT_COUNT];
    for (i, v) in values.iter().take(MAX_PARAMS).enumerate() {
        rows[i / 4][i % 4] = *v;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declaration format, exactly as the docs write it — including the
    /// alignment padding people will actually type.
    #[test]
    fn declared_params_parse_in_order() {
        let src = r#"
// @param sense_distance  4.0 .. 40.0 = 12.0   "How far ahead it smells"
// @param decay           0.1 ..  3.0 =  0.62  "How fast trails fade"
//@param bare 0 .. 1 = 0.5
fn main() {}
"#;
        let params = shader_params(src);
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "sense_distance");
        assert_eq!((params[0].min, params[0].max, params[0].default), (4.0, 40.0, 12.0));
        assert_eq!(params[0].doc, "How far ahead it smells");
        assert_eq!(params[1].name, "decay");
        assert_eq!(params[2].doc, "");
    }

    /// Malformed lines are skipped, never fatal; out-of-range defaults clamp.
    #[test]
    fn malformed_declarations_do_not_take_the_shader_down() {
        let src = r#"
// @param backwards 5.0 .. 1.0 = 2.0
// @param no_range = 1.0
// @param bad-name 0 .. 1 = 0.5
// @param clamped 0.0 .. 1.0 = 9.0 "too big"
// @param nan 0 .. 1 = nan
"#;
        let params = shader_params(src);
        assert_eq!(params.len(), 1, "{params:?}");
        assert_eq!(params[0].name, "clamped");
        assert_eq!(params[0].default, 1.0);
    }

    /// Values pack lane-by-lane in declaration order — the contract the
    /// preamble's `param(i)` helper depends on.
    #[test]
    fn packing_is_lane_ordered_and_capped() {
        let rows = pack(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(rows[0], [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(rows[1], [5.0, 0.0, 0.0, 0.0]);
        let too_many: Vec<f32> = (0..40).map(|i| i as f32).collect();
        assert_eq!(pack(&too_many)[SLOT_COUNT - 1][3], 31.0);
    }
}
