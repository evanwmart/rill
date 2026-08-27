//! KDL source → canonical `.rill` bytes (document-format.md §7).
//!
//! Style layering happens here: nodes list partial styles
//! (`style="card serif"`), the compiler merges left→right (last listed
//! wins, with a note per override) and emits one flattened style-table
//! entry per distinct combination. The runtime never composes styles.

use std::collections::{BTreeMap, BTreeSet};

use kdl::{KdlDocument, KdlNode, KdlValue};
use rill_protocol::validate_path;

use crate::{
    ActionValue, Color, ColorRef, Dimension, DocAction, DocError, Document, MAX_DEPTH, NO_STYLE,
    Node, StateVar, Style, encode, err,
};

/// A color as authored, before string interning: a literal, or a semantic
/// token name (`accent`, `surface`, …) that the client resolves at render
/// time against the active theme.
#[derive(Debug, Clone, PartialEq)]
enum PartialColor {
    Literal(Color),
    Token(String),
}

/// Compilation output: canonical bytes plus human-facing notes
/// (style-override diagnostics).
#[derive(Debug)]
pub struct Compiled {
    pub bytes: Vec<u8>,
    pub notes: Vec<String>,
}

/// A named partial style, as authored.
#[derive(Debug, Clone, Default, PartialEq)]
struct Partial {
    color: Option<PartialColor>,
    background: Option<PartialColor>,
    font_size: Option<f32>,
    font_weight: Option<u16>,
    corner_radius: Option<f32>,
    font_family: Option<String>,
    align: Option<crate::Align>,
    width: Option<Dimension>,
    height: Option<Dimension>,
    underline: Option<bool>,
    size_token: Option<String>,
    padding_token: Option<String>,
    gap_token: Option<String>,
    padding_px: Option<f32>,
    gap_px: Option<f32>,
    padding_x_token: Option<String>,
    padding_x_px: Option<f32>,
    padding_y_token: Option<String>,
    padding_y_px: Option<f32>,
    measure_group: Option<String>,
    valign: Option<crate::Align>,
    ellipsis: Option<bool>,
    shadow_token: Option<String>,
    border: Option<f32>,
    border_color: Option<PartialColor>,
    hover: Option<String>,
    backdrop: Option<f32>,
    wrap: Option<bool>,
}

/// Intermediate tree between KDL and the flat node table.
enum Ir {
    Text { styles: Vec<String>, value: String },
    Image { styles: Vec<String>, source: String },
    Icon { styles: Vec<String>, name: String, size: Dimension },
    Row { styles: Vec<String>, gap: Dimension, padding: Dimension, target: Option<String>, children: Vec<Ir> },
    Column { styles: Vec<String>, gap: Dimension, padding: Dimension, target: Option<String>, children: Vec<Ir> },
    Rectangle { styles: Vec<String>, width: Dimension, height: Dimension },
    Spacer { styles: Vec<String>, size: Dimension },
    Link { styles: Vec<String>, label: String, target: String },
    Scroll { styles: Vec<String>, child: Box<Ir> },
    Chrome { styles: Vec<String>, child: Box<Ir> },
    Button { styles: Vec<String>, label: String, icon: Option<String>, action: IrAction },
    TextInput { styles: Vec<String>, bind: String, placeholder: String, action: Option<IrAction>, multiline: bool },
    Code { styles: Vec<String>, bind: String, lang: String },
    Slider { styles: Vec<String>, bind: String, min: f32, max: f32, step: f32, action: Option<IrAction> },
    When { state: String, invert: bool, child: Box<Ir> },
    Key { combo: String, target: Option<String>, action: Option<IrAction> },
    Menu { items: Vec<IrMenuItem> },
    Keys { target: String },
    Live { target: String, interval: u16 },
    Sensitive { tier: u8 },
    Closing { target: String },
    Page { color: PartialColor },
}

#[derive(Debug, Clone)]
struct IrMenuItem {
    label: String,
    icon: Option<String>,
    target: Option<String>,
    action: Option<IrAction>,
    danger: bool,
    separator: bool,
}

#[derive(Debug, Clone)]
enum IrAction {
    OpenMenu,
    Navigate(String),
    Toggle(String),
    Set(String, ActionValue),
    Submit { endpoint: String, fields: Vec<(String, String)> },
    PickFile { into: String },
}

impl Ir {
    fn styles(&self) -> &[String] {
        match self {
            Ir::Text { styles, .. }
            | Ir::Icon { styles, .. }
            | Ir::Image { styles, .. }
            | Ir::Row { styles, .. }
            | Ir::Column { styles, .. }
            | Ir::Rectangle { styles, .. }
            | Ir::Spacer { styles, .. }
            | Ir::Link { styles, .. }
            | Ir::Scroll { styles, .. }
            | Ir::Chrome { styles, .. }
            | Ir::Button { styles, .. }
            | Ir::TextInput { styles, .. }
            | Ir::Code { styles, .. }
            | Ir::Slider { styles, .. } => styles,
            Ir::When { .. }
            | Ir::Key { .. }
            | Ir::Menu { .. }
            | Ir::Keys { .. }
            | Ir::Live { .. }
            | Ir::Sensitive { .. }
            | Ir::Closing { .. }
            | Ir::Page { .. } => &[],
        }
    }
}

/// Rejects source nested deeper than [`MAX_DEPTH`] *before* handing it to the
/// KDL parser, which recurses per `{` and overflows its stack — an abort, not
/// an error — a few thousand levels down. Depth is counted lexically because
/// there is nothing else to count it with at this point: the tree that would
/// answer the question is what the parser has not built yet.
///
/// Only brace depth matters, so this skips exactly the constructs in which a
/// brace is not a brace: comments and the three string forms. Miscounting
/// would have to survive 256 levels of headroom over the deepest real page
/// (under 20) to reject anything anyone wrote.
fn check_source_depth(source: &str) -> Result<(), DocError> {
    let b = source.as_bytes();
    let (mut i, mut depth, mut comment) = (0usize, 0u32, 0u32);
    while i < b.len() {
        // Block comments nest in KDL, so they get a counter of their own.
        if comment > 0 {
            match b[i..] {
                [b'/', b'*', ..] => (comment, i) = (comment + 1, i + 2),
                [b'*', b'/', ..] => (comment, i) = (comment - 1, i + 2),
                _ => i += 1,
            }
            continue;
        }
        match b[i..] {
            [b'/', b'*', ..] => (comment, i) = (1, i + 2),
            [b'/', b'/', ..] => i += b[i..].iter().position(|&c| c == b'\n').unwrap_or(b.len() - i),
            // Raw string: `#"…"#`, `##"…"##`, … closed by a quote and the
            // same run of hashes. No escapes inside, by definition.
            [b'#', ..] => {
                let hashes = b[i..].iter().take_while(|&&c| c == b'#').count();
                if b.get(i + hashes) != Some(&b'"') {
                    i += hashes; // `#true`/`#false`/`#null`, not a string
                    continue;
                }
                i += hashes + 1;
                let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                i = match b[i..].windows(close.len()).position(|w| w == close) {
                    Some(p) => i + p + close.len(),
                    None => b.len(), // unterminated — the parser will say so
                };
            }
            [b'"', ..] => {
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            [b'{', ..] => {
                depth += 1;
                i += 1;
                if depth > MAX_DEPTH {
                    return Err(err(format!(
                        "source nested deeper than {MAX_DEPTH} — refused before parsing, \
                         which would recurse per level and overflow the stack"
                    )));
                }
            }
            [b'}', ..] => (depth, i) = (depth.saturating_sub(1), i + 1),
            _ => i += 1,
        }
    }
    Ok(())
}

pub fn compile(source: &str) -> Result<Compiled, DocError> {
    check_source_depth(source)?;
    let kdoc: KdlDocument =
        source.parse().map_err(|e: kdl::KdlError| err(format!("parse error: {e}")))?;

    // Top level: style definitions plus exactly one root UI node.
    let mut partials: BTreeMap<String, Partial> = BTreeMap::new();
    let mut state_order: Vec<(String, ActionValue)> = Vec::new();
    let mut root_kdl: Option<&KdlNode> = None;
    for node in kdoc.nodes() {
        if node.name().value() == "style" {
            let (name, partial) = parse_style_def(node)?;
            if partials.insert(name.clone(), partial).is_some() {
                return Err(err(format!("style {name:?} defined twice")));
            }
        } else if node.name().value() == "state" {
            let (name, initial) = parse_state_def(node)?;
            if state_order.iter().any(|(n, _)| n == &name) {
                return Err(err(format!("state {name:?} defined twice")));
            }
            state_order.push((name, initial));
        } else if root_kdl.is_some() {
            return Err(err("more than one root UI node (wrap them in a column or row)"));
        } else {
            root_kdl = Some(node);
        }
    }
    let root_kdl = root_kdl.ok_or_else(|| err("no root UI node"))?;
    let ir = build_ir(root_kdl)?;

    // Resolve style combos in first-use (walk) order; collect notes.
    let mut notes = Vec::new();
    let mut combos: Vec<(String, Partial)> = Vec::new(); // (debug name, resolved)
    let mut combo_index: BTreeMap<Vec<String>, u16> = BTreeMap::new();
    resolve_combos(&ir, &partials, &mut combos, &mut combo_index, &mut notes)?;
    // A style named only by `hover=` is never applied to a node, so the walk
    // above never reaches it — and the viewer would look it up by name and
    // find nothing. Emit those too, once, in declaration order.
    let mut hover_targets: Vec<String> = combos
        .iter()
        .filter_map(|(_, p)| p.hover.clone())
        .collect();
    hover_targets.dedup();
    for target in hover_targets {
        if combo_index.contains_key(std::slice::from_ref(&target)) {
            continue;
        }
        let Some(partial) = partials.get(&target) else {
            return Err(err(format!("hover names unknown style {target:?}")));
        };
        if combos.len() >= u16::MAX as usize {
            return Err(err("too many style combos"));
        }
        combo_index.insert(vec![target.clone()], combos.len() as u16);
        combos.push((target, partial.clone()));
    }

    // String table: everything textual, sorted + deduplicated.
    let mut string_set: BTreeSet<String> = BTreeSet::new();
    collect_strings(&ir, &mut string_set);
    for (name, _) in &state_order {
        string_set.insert(name.clone());
    }
    for (name, resolved) in &combos {
        string_set.insert(name.clone());
        if let Some(family) = &resolved.font_family {
            string_set.insert(family.clone());
        }
        for c in [&resolved.color, &resolved.background].into_iter().flatten() {
            if let PartialColor::Token(tok) = c {
                string_set.insert(tok.clone());
            }
        }
        // Scale steps are strings in the table too — they are resolved
        // against the theme at render time, exactly like colour tokens.
        for token in [
            &resolved.size_token,
            &resolved.padding_token,
            &resolved.gap_token,
            &resolved.shadow_token,
            &resolved.hover,
            &resolved.padding_x_token,
            &resolved.padding_y_token,
            &resolved.measure_group,
        ]
        .into_iter()
        .flatten()
        {
            string_set.insert(token.clone());
        }
        if let Some(PartialColor::Token(tok)) = &resolved.border_color {
            string_set.insert(tok.clone());
        }
    }
    let strings: Vec<String> = string_set.into_iter().collect();
    if strings.len() > u16::MAX as usize {
        return Err(err("too many distinct strings"));
    }
    let string_idx = |s: &str| -> u16 {
        strings.binary_search_by(|x| x.as_str().cmp(s)).expect("collected") as u16
    };

    let color_ref = |c: &PartialColor| -> ColorRef {
        match c {
            PartialColor::Literal(c) => ColorRef::Literal(*c),
            PartialColor::Token(tok) => ColorRef::Token(string_idx(tok)),
        }
    };
    let styles: Vec<Style> = combos
        .iter()
        .map(|(name, p)| Style {
            name_idx: string_idx(name),
            color: p.color.as_ref().map(&color_ref),
            background: p.background.as_ref().map(&color_ref),
            font_size: p.font_size,
            font_weight: p.font_weight,
            corner_radius: p.corner_radius,
            font_family: p.font_family.as_deref().map(string_idx),
            align: p.align,
            width: p.width,
            height: p.height,
            underline: p.underline,
            size_token: p.size_token.as_deref().map(string_idx),
            padding_token: p.padding_token.as_deref().map(string_idx),
            gap_token: p.gap_token.as_deref().map(string_idx),
            padding_px: p.padding_px,
            gap_px: p.gap_px,
            padding_x_token: p.padding_x_token.as_deref().map(string_idx),
            padding_x_px: p.padding_x_px,
            padding_y_token: p.padding_y_token.as_deref().map(string_idx),
            padding_y_px: p.padding_y_px,
            measure_group: p.measure_group.as_deref().map(string_idx),
            valign: p.valign,
            ellipsis: p.ellipsis,
            shadow_token: p.shadow_token.as_deref().map(string_idx),
            border: p.border,
            border_color: p.border_color.as_ref().map(&color_ref),
            hover: p.hover.as_deref().map(string_idx),
            backdrop: p.backdrop,
            wrap: p.wrap,
        })
        .collect();

    // State table (declaration order) + name → index map for references.
    let mut state_index: BTreeMap<String, u16> = BTreeMap::new();
    let states: Vec<StateVar> = state_order
        .iter()
        .enumerate()
        .map(|(i, (name, initial))| {
            state_index.insert(name.clone(), i as u16);
            StateVar { name_idx: string_idx(name), initial: initial.clone() }
        })
        .collect();
    let state_ref = |name: &str| -> Result<u16, DocError> {
        state_index
            .get(name)
            .copied()
            .ok_or_else(|| err(format!("unknown state {name:?} (declare it: state {name:?} initial=…)")))
    };
    let state_type = |idx: u16| -> &ActionValue { &state_order[idx as usize].1 };

    // Action table, in emission order; buttons reference by index.
    let mut actions: Vec<DocAction> = Vec::new();
    let mut ctx = EmitCtx {
        combo_index: &combo_index,
        string_idx: &string_idx,
        state_ref: &state_ref,
        state_type: &state_type,
        actions: &mut actions,
    };
    let mut nodes: Vec<Node> = Vec::new();
    let root = emit(&ir, &mut ctx, &mut nodes)?;
    let doc = Document { strings, styles, states, actions, nodes, root, warnings: Vec::new() };
    Ok(Compiled { bytes: encode(&doc)?, notes })
}

fn rill_doc_no_style() -> u16 {
    crate::NO_STYLE
}

fn build_doc_action(action: &IrAction, ctx: &EmitCtx) -> Result<DocAction, DocError> {
    Ok(match action {
        IrAction::OpenMenu => DocAction::OpenMenu,
        IrAction::Navigate(target) => DocAction::Navigate { target: (ctx.string_idx)(target) },
        IrAction::Toggle(name) => {
            let state = (ctx.state_ref)(name)?;
            if !matches!((ctx.state_type)(state), ActionValue::Bool(_)) {
                return Err(err(format!("toggle {name:?}: state is not a bool")));
            }
            DocAction::Toggle { state }
        }
        IrAction::Set(name, value) => {
            let state = (ctx.state_ref)(name)?;
            if std::mem::discriminant((ctx.state_type)(state)) != std::mem::discriminant(value) {
                return Err(err(format!(
                    "set {name:?}: value type {} does not match state type {}",
                    value.type_name(),
                    (ctx.state_type)(state).type_name()
                )));
            }
            DocAction::SetState { state, value: value.clone() }
        }
        IrAction::Submit { endpoint, fields } => {
            let mut out = Vec::with_capacity(fields.len());
            for (fname, from) in fields {
                out.push(((ctx.string_idx)(fname), (ctx.state_ref)(from)?));
            }
            DocAction::Submit { endpoint: (ctx.string_idx)(endpoint), fields: out }
        }
        IrAction::PickFile { into } => {
            let state = (ctx.state_ref)(into)?;
            if !matches!((ctx.state_type)(state), ActionValue::Str(_)) {
                return Err(err(format!("pick_file into {into:?}: state is not a string")));
            }
            DocAction::PickFile { into: state }
        }
    })
}

struct EmitCtx<'a> {
    combo_index: &'a BTreeMap<Vec<String>, u16>,
    string_idx: &'a dyn Fn(&str) -> u16,
    state_ref: &'a dyn Fn(&str) -> Result<u16, DocError>,
    state_type: &'a dyn Fn(u16) -> &'a ActionValue,
    actions: &'a mut Vec<DocAction>,
}

// ------------------------------------------------------------- KDL helpers

fn prop<'a>(node: &'a KdlNode, name: &str) -> Option<&'a KdlValue> {
    node.entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some(name))
        .map(|e| e.value())
}

fn positional(node: &KdlNode, i: usize) -> Option<&KdlValue> {
    node.entries().iter().filter(|e| e.name().is_none()).nth(i).map(|e| e.value())
}

fn as_number(v: &KdlValue) -> Option<f64> {
    v.as_integer().map(|i| i as f64).or_else(|| v.as_float())
}

fn check_props(node: &KdlNode, allowed: &[&str]) -> Result<(), DocError> {
    for e in node.entries() {
        if let Some(name) = e.name() {
            let name = name.value();
            if !allowed.contains(&name) {
                return Err(err(format!(
                    "{}: unknown property {name:?} (allowed: {})",
                    node.name().value(),
                    allowed.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// A scale-step name: lowercase letters only, so it can never be confused
/// with a literal and never carries anything but a token.
fn scale_token(v: &KdlValue, style: &str, what: &str) -> Result<String, DocError> {
    let name = v.as_string().filter(|s| {
        !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_lowercase())
    });
    name.map(|s| s.to_string()).ok_or_else(|| {
        err(format!("style {style:?}: {what} is a number or a scale step (xs, sm, md, lg, xl)"))
    })
}

fn parse_dimension(node: &KdlNode, name: &str, default: Dimension) -> Result<Dimension, DocError> {
    let Some(v) = prop(node, name) else { return Ok(default) };
    dimension_value(v).ok_or_else(|| {
        err(format!("{}: {name} must be a finite number or \"auto\"", node.name().value()))
    })
}

fn dimension_value(v: &KdlValue) -> Option<Dimension> {
    if let Some(n) = as_number(v) {
        let f = n as f32;
        return f.is_finite().then_some(Dimension::Px(f));
    }
    (v.as_string() == Some("auto")).then_some(Dimension::Auto)
}

/// A color property is either a `#rrggbb[aa]` literal or a bare semantic
/// token name (lowercase letters, digits, and dashes — e.g. `accent`,
/// `surface-raised`) resolved against the active theme at render time.
fn parse_color(v: &KdlValue, what: &str) -> Result<PartialColor, DocError> {
    let bad = || {
        err(format!(
            "{what}: color is \"#rrggbb\"/\"#rrggbbaa\" or a theme token like \"accent\""
        ))
    };
    let s = v.as_string().ok_or_else(bad)?;
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 && hex.len() != 8 {
            return Err(bad());
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| bad());
        return Ok(PartialColor::Literal(Color {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if hex.len() == 8 { byte(6)? } else { 0xFF },
        }));
    }
    let is_token = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if is_token { Ok(PartialColor::Token(s.to_string())) } else { Err(bad()) }
}

fn parse_style_def(node: &KdlNode) -> Result<(String, Partial), DocError> {
    let name = positional(node, 0)
        .and_then(|v| v.as_string())
        .ok_or_else(|| err("style: first argument must be the style name"))?
        .to_string();
    check_props(
        node,
        &[
            "color", "background", "size", "weight", "font", "corner", "align", "width",
            "height", "underline", "padding", "gap", "shadow", "border", "border-color",
            "hover", "backdrop", "wrap", "valign", "ellipsis", "padding-x", "padding-y", "group",
        ],
    )?;
    if node.children().is_some() {
        return Err(err(format!("style {name:?}: styles take no children")));
    }
    let mut p = Partial::default();
    if let Some(v) = prop(node, "color") {
        p.color = Some(parse_color(v, &format!("style {name:?}"))?);
    }
    if let Some(v) = prop(node, "background") {
        p.background = Some(parse_color(v, &format!("style {name:?}"))?);
    }
    if let Some(v) = prop(node, "size") {
        // A number is a literal size; a name is a step on the theme's type
        // scale, which is what lets one table re-type the whole desktop.
        match as_number(v).filter(|n| n.is_finite() && *n > 0.0) {
            Some(n) => p.font_size = Some(n as f32),
            None => {
                p.size_token = Some(scale_token(v, &name, "size")?);
            }
        }
    }
    if let Some(v) = prop(node, "weight") {
        p.font_weight = Some(match v.as_string() {
            Some("normal") => 400,
            Some("bold") => 700,
            _ => as_number(v)
                .map(|n| n as i64)
                .filter(|n| (1..=1000).contains(n))
                .ok_or_else(|| {
                    err(format!("style {name:?}: weight is \"normal\", \"bold\", or 1–1000"))
                })? as u16,
        });
    }
    if let Some(v) = prop(node, "font") {
        p.font_family = Some(
            v.as_string()
                .ok_or_else(|| err(format!("style {name:?}: font must be a string")))?
                .to_string(),
        );
    }
    if let Some(v) = prop(node, "corner") {
        let n = as_number(v).filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(|| err(format!("style {name:?}: corner must be ≥ 0")))?;
        p.corner_radius = Some(n as f32);
    }
    if let Some(v) = prop(node, "align") {
        p.align = Some(match v.as_string() {
            Some("left") => crate::Align::Left,
            Some("center") => crate::Align::Center,
            Some("right") => crate::Align::Right,
            _ => {
                return Err(err(format!(
                    "style {name:?}: align is \"left\", \"center\", or \"right\""
                )));
            }
        });
    }
    // The same three positions, turned ninety degrees: how a row places a
    // child shorter than the row. Spelled top/center/bottom because that is
    // what a person means, and stored as the same enum.
    if let Some(v) = prop(node, "valign") {
        p.valign = Some(match v.as_string() {
            Some("top") => crate::Align::Left,
            Some("center") => crate::Align::Center,
            Some("bottom") => crate::Align::Right,
            _ => {
                return Err(err(format!(
                    "style {name:?}: valign is \"top\", \"center\", or \"bottom\""
                )));
            }
        });
    }
    if let Some(v) = prop(node, "group") {
        let group = v.as_string().filter(|g| {
            !g.is_empty() && g.len() <= 40 && g.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        });
        p.measure_group = Some(
            group
                .map(String::from)
                .ok_or_else(|| err(format!("style {name:?}: group is a short kebab-case name")))?,
        );
    }
    if let Some(v) = prop(node, "ellipsis") {
        p.ellipsis = Some(
            v.as_bool()
                .ok_or_else(|| err(format!("style {name:?}: ellipsis is #true or #false")))?,
        );
    }
    for (prop_name, slot) in [("width", 0), ("height", 1)] {
        let Some(v) = prop(node, prop_name) else { continue };
        // A number is pixels; "fill" takes a share of the leftover (weight 1,
        // matching how spacers and containers already flex); "auto" is the
        // intrinsic default.
        let dim = if let Some(n) = as_number(v).filter(|n| n.is_finite() && *n >= 0.0) {
            Dimension::Px(n as f32)
        } else {
            match v.as_string() {
                Some("fill") => Dimension::Fill(1.0),
                Some("auto") => Dimension::Auto,
                _ => {
                    return Err(err(format!(
                        "style {name:?}: {prop_name} is a number, \"fill\", or \"auto\""
                    )));
                }
            }
        };
        if slot == 0 {
            p.width = Some(dim);
        } else {
            p.height = Some(dim);
        }
    }
    for (prop_name, slot) in [("padding", 0), ("gap", 1), ("padding-x", 2), ("padding-y", 3)] {
        let Some(v) = prop(node, prop_name) else { continue };
        // A number is a literal — zero has no scale step and should not get
        // one — and a name is a step on the theme's space scale.
        match as_number(v).filter(|n| n.is_finite() && *n >= 0.0) {
            Some(n) => {
                let n = n as f32;
                match slot {
                    0 => p.padding_px = Some(n),
                    1 => p.gap_px = Some(n),
                    2 => p.padding_x_px = Some(n),
                    _ => p.padding_y_px = Some(n),
                }
            }
            None => {
                let token = scale_token(v, &name, prop_name)?;
                match slot {
                    0 => p.padding_token = Some(token),
                    1 => p.gap_token = Some(token),
                    2 => p.padding_x_token = Some(token),
                    _ => p.padding_y_token = Some(token),
                }
            }
        }
    }
    if let Some(v) = prop(node, "shadow") {
        p.shadow_token = Some(scale_token(v, &name, "shadow")?);
    }
    if let Some(v) = prop(node, "border") {
        let n = as_number(v).filter(|n| n.is_finite() && (0.0..=64.0).contains(n)).ok_or_else(
            || err(format!("style {name:?}: border must be a width from 0 to 64")),
        )?;
        p.border = Some(n as f32);
    }
    if let Some(v) = prop(node, "border-color") {
        p.border_color = Some(parse_color(v, &format!("style {name:?}"))?);
    }
    if let Some(v) = prop(node, "hover") {
        p.hover = Some(
            v.as_string()
                .ok_or_else(|| err(format!("style {name:?}: hover names a style")))?
                .to_string(),
        );
    }
    if let Some(v) = prop(node, "wrap") {
        p.wrap = Some(v.as_bool().ok_or_else(|| {
            err(format!("style {name:?}: wrap must be #true or #false"))
        })?);
    }
    if let Some(v) = prop(node, "backdrop") {
        let n = as_number(v).filter(|n| n.is_finite() && (0.0..=256.0).contains(n)).ok_or_else(
            || err(format!("style {name:?}: backdrop must be a blur from 0 to 256")),
        )?;
        p.backdrop = Some(n as f32);
    }
    if let Some(v) = prop(node, "underline") {
        p.underline = Some(v.as_bool().ok_or_else(|| {
            err(format!("style {name:?}: underline must be #true or #false"))
        })?);
    }
    Ok((name, p))
}

fn kdl_value(v: &KdlValue, what: &str) -> Result<ActionValue, DocError> {
    if let Some(b) = v.as_bool() {
        return Ok(ActionValue::Bool(b));
    }
    if let Some(n) = as_number(v) {
        if !n.is_finite() {
            return Err(err(format!("{what}: non-finite number")));
        }
        return Ok(ActionValue::Num(n));
    }
    if let Some(s) = v.as_string() {
        return Ok(ActionValue::Str(s.to_string()));
    }
    Err(err(format!("{what}: value must be a string, number, or bool")))
}

fn parse_state_def(node: &KdlNode) -> Result<(String, ActionValue), DocError> {
    let name = positional(node, 0)
        .and_then(|v| v.as_string())
        .ok_or_else(|| err("state: first argument must be the state name"))?
        .to_string();
    if name.is_empty() || name.len() > 64 {
        return Err(err(format!("state {name:?}: name must be 1–64 bytes")));
    }
    check_props(node, &["initial"])?;
    let initial = prop(node, "initial")
        .ok_or_else(|| err(format!("state {name:?}: initial=<value> is required (types are inferred)")))?;
    Ok((name.clone(), kdl_value(initial, &format!("state {name:?}"))?))
}

fn parse_style_ref(node: &KdlNode) -> Result<Vec<String>, DocError> {
    match prop(node, "style") {
        None => Ok(Vec::new()),
        Some(v) => {
            let s = v.as_string().ok_or_else(|| {
                err(format!("{}: style must be a string of layer names", node.name().value()))
            })?;
            let layers: Vec<String> = s.split_whitespace().map(String::from).collect();
            if layers.is_empty() {
                return Err(err(format!("{}: empty style reference", node.name().value())));
            }
            Ok(layers)
        }
    }
}

// ------------------------------------------------------------ IR building

/// Parse one action node (navigate/toggle/set/submit) — shared by buttons
/// and text_input on-Enter.
fn parse_action_node(a: &KdlNode) -> Result<IrAction, DocError> {
    match a.name().value() {
        "navigate" => {
            let target = positional(a, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("navigate: needs a target path"))?;
            validate_path(target).map_err(|e| err(format!("navigate: {e}")))?;
            Ok(IrAction::Navigate(target.to_string()))
        }
        "toggle" => Ok(IrAction::Toggle(
            positional(a, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("toggle: needs a state name"))?
                .to_string(),
        )),
        "set" => {
            let name = positional(a, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("set: needs a state name"))?;
            let value = positional(a, 1).ok_or_else(|| err("set: needs a value"))?;
            Ok(IrAction::Set(name.to_string(), kdl_value(value, "set")?))
        }
        "submit" => {
            let endpoint = positional(a, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("submit: needs an endpoint path"))?;
            validate_path(endpoint).map_err(|e| err(format!("submit: {e}")))?;
            let mut fields = Vec::new();
            if let Some(fblock) = a.children() {
                for f in fblock.nodes() {
                    if f.name().value() != "field" {
                        return Err(err("submit: children must be field nodes"));
                    }
                    let fname = positional(f, 0)
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| err("field: needs a name"))?;
                    let from = prop(f, "from")
                        .and_then(|v| v.as_string())
                        .ok_or_else(|| err("field: from=\"state\" is required"))?;
                    fields.push((fname.to_string(), from.to_string()));
                }
            }
            Ok(IrAction::Submit { endpoint: endpoint.to_string(), fields })
        }
        "menu" => Ok(IrAction::OpenMenu),
        "pick_file" => {
            let into = prop(a, "into")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("pick_file: into=\"state\" is required"))?;
            Ok(IrAction::PickFile { into: into.to_string() })
        }
        other => Err(err(format!(
            "unknown action {other:?} (navigate, toggle, set, submit, pick_file)"
        ))),
    }
}

fn build_ir(node: &KdlNode) -> Result<Ir, DocError> {
    let name = node.name().value();
    let children = |node: &KdlNode| -> Result<Vec<Ir>, DocError> {
        node.children().map_or(Ok(Vec::new()), |block| {
            block.nodes().iter().map(build_ir).collect()
        })
    };
    let no_children = |node: &KdlNode| -> Result<(), DocError> {
        if node.children().is_some_and(|c| !c.nodes().is_empty()) {
            return Err(err(format!("{name}: takes no children")));
        }
        Ok(())
    };

    match name {
        "text" => {
            check_props(node, &["style"])?;
            no_children(node)?;
            let value = positional(node, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("text: first argument must be the text string"))?;
            Ok(Ir::Text { styles: parse_style_ref(node)?, value: value.to_string() })
        }
        "icon" => {
            check_props(node, &["style", "size"])?;
            let name = positional(node, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("icon: first argument must be the icon name"))?;
            // Lowercase and dashes only: the name indexes a built-in table,
            // so anything else is a typo rather than a glyph.
            let ok = !name.is_empty()
                && name.len() <= 40
                && name.chars().all(|c| c.is_ascii_lowercase() || c == '-');
            if !ok {
                return Err(err(format!("icon: bad name {name:?}")));
            }
            Ok(Ir::Icon {
                styles: parse_style_ref(node)?,
                name: name.to_string(),
                // Unsized means unsized: the renderer sits an unsized icon on
                // the text it labels (its style's font size). The old Px(18)
                // default made that rule unreachable — every compiled icon
                // arrived hand-sized whether the document said so or not.
                size: parse_dimension(node, "size", Dimension::Auto)?,
            })
        }
        "image" => {
            check_props(node, &["style"])?;
            no_children(node)?;
            let source = positional(node, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("image: first argument must be the source path"))?;
            validate_path(source).map_err(|e| err(format!("image {source:?}: {e}")))?;
            Ok(Ir::Image { styles: parse_style_ref(node)?, source: source.to_string() })
        }
        "row" | "column" => {
            check_props(node, &["style", "gap", "padding", "target"])?;
            let target = match prop(node, "target").and_then(|v| v.as_string()) {
                Some(t) => {
                    validate_path(t).map_err(|e| err(format!("{name} target {t:?}: {e}")))?;
                    Some(t.to_string())
                }
                None => None,
            };
            // Unspecified spacing is Auto, not zero: the two used to encode
            // identically, so the theme never got a chance to supply a
            // rhythm. An explicit `padding=0` still means zero.
            let gap = parse_dimension(node, "gap", Dimension::Auto)?;
            let padding = parse_dimension(node, "padding", Dimension::Auto)?;
            let styles = parse_style_ref(node)?;
            let kids = children(node)?;
            Ok(if name == "row" {
                Ir::Row { styles, gap, padding, target, children: kids }
            } else {
                Ir::Column { styles, gap, padding, target, children: kids }
            })
        }
        "rect" => {
            check_props(node, &["style", "width", "height"])?;
            no_children(node)?;
            Ok(Ir::Rectangle {
                styles: parse_style_ref(node)?,
                width: parse_dimension(node, "width", Dimension::Auto)?,
                height: parse_dimension(node, "height", Dimension::Auto)?,
            })
        }
        "spacer" => {
            check_props(node, &["style", "size"])?;
            no_children(node)?;
            let size = match positional(node, 0) {
                Some(v) => dimension_value(v)
                    .ok_or_else(|| err("spacer: size must be a finite number or \"auto\""))?,
                None => parse_dimension(node, "size", Dimension::Auto)?,
            };
            Ok(Ir::Spacer { styles: parse_style_ref(node)?, size })
        }
        "link" => {
            check_props(node, &["style", "target"])?;
            no_children(node)?;
            let label = positional(node, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("link: first argument must be the label"))?;
            let target = prop(node, "target")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("link: target=\"/path\" is required"))?;
            validate_path(target).map_err(|e| err(format!("link target {target:?}: {e}")))?;
            Ok(Ir::Link {
                styles: parse_style_ref(node)?,
                label: label.to_string(),
                target: target.to_string(),
            })
        }
        "titlebar" => {
            // The document's claim on the window's own chrome strip. One
            // child, like scroll — the host gives it a rect, not a flow.
            check_props(node, &["style"])?;
            let mut kids = children(node)?;
            if kids.len() != 1 {
                return Err(err(format!("titlebar: needs exactly one child, has {}", kids.len())));
            }
            Ok(Ir::Chrome { styles: parse_style_ref(node)?, child: Box::new(kids.remove(0)) })
        }
        "scroll" => {
            check_props(node, &["style"])?;
            let mut kids = children(node)?;
            if kids.len() != 1 {
                return Err(err(format!("scroll: needs exactly one child, has {}", kids.len())));
            }
            Ok(Ir::Scroll { styles: parse_style_ref(node)?, child: Box::new(kids.remove(0)) })
        }
        "button" => {
            check_props(node, &["style", "icon"])?;
            let icon = match prop(node, "icon") {
                Some(v) => {
                    let name = v.as_string().filter(|n| {
                        !n.is_empty()
                            && n.len() <= 40
                            && n.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                    });
                    Some(
                        name.map(String::from)
                            .ok_or_else(|| err("button: icon is a lowercase glyph name"))?,
                    )
                }
                None => None,
            };
            // The label is optional when an icon carries the meaning.
            let label = positional(node, 0).and_then(|v| v.as_string()).map(String::from);
            if label.is_none() && icon.is_none() {
                return Err(err("button: needs a label, an icon, or both"));
            }
            let block = node
                .children()
                .ok_or_else(|| err("button: needs an action child, e.g. { navigate \"/x\" }"))?;
            let action_nodes = block.nodes();
            if action_nodes.len() != 1 {
                return Err(err("button: exactly one action child (navigate/toggle/set/submit)"));
            }
            let action = parse_action_node(&action_nodes[0])?;
            Ok(Ir::Button {
                styles: parse_style_ref(node)?,
                label: label.unwrap_or_default(),
                icon,
                action,
            })
        }
        "text_input" => {
            check_props(node, &["style", "bind", "placeholder", "multiline"])?;
            let multiline = prop(node, "multiline").and_then(|v| v.as_bool()).unwrap_or(false);
            let bind = prop(node, "bind")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("text_input: bind=\"state\" is required"))?;
            let placeholder = prop(node, "placeholder")
                .and_then(|v| v.as_string())
                .unwrap_or("");
            // Optional single action child: fires on Enter.
            let action = match node.children().map(|b| b.nodes()) {
                None => None,
                Some([]) => None,
                Some([a]) => Some(parse_action_node(a)?),
                Some(_) => return Err(err("text_input: at most one action child (on Enter)")),
            };
            Ok(Ir::TextInput {
                styles: parse_style_ref(node)?,
                bind: bind.to_string(),
                placeholder: placeholder.to_string(),
                action,
                multiline,
            })
        }
        "code" => {
            // `code bind="body" lang="rs"` — the whole editor in one node:
            // the bound state as a highlighted mono grid, gutter, caret,
            // click-to-edit. One mode, the way an editor is one thing.
            check_props(node, &["style", "bind", "lang"])?;
            no_children(node)?;
            let bind = prop(node, "bind")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("code: bind=\"state\" is required"))?;
            let lang = prop(node, "lang").and_then(|v| v.as_string()).unwrap_or("");
            Ok(Ir::Code {
                styles: parse_style_ref(node)?,
                bind: bind.to_string(),
                lang: lang.to_string(),
            })
        }
        "slider" => {
            // `slider bind="decay" min=0.1 max=3.0 step=0.01 { submit … }` —
            // the range is document content, like a link's target: the page
            // says what values exist, the far end says what they mean.
            check_props(node, &["style", "bind", "min", "max", "step"])?;
            let bind = prop(node, "bind")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("slider: bind=\"state\" is required"))?;
            let range = |name: &str| -> Result<f32, DocError> {
                prop(node, name)
                    .and_then(as_number)
                    .map(|v| v as f32)
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| err(format!("slider: {name}= must be a finite number")))
            };
            let (min, max) = (range("min")?, range("max")?);
            if min >= max {
                return Err(err(format!("slider: min {min} is not below max {max}")));
            }
            let step = match prop(node, "step") {
                None => 0.0,
                Some(v) => as_number(v)
                    .map(|v| v as f32)
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v <= max - min)
                    .ok_or_else(|| err("slider: step= must fit inside min..max"))?,
            };
            // Optional single action child: fires on release.
            let action = match node.children().map(|b| b.nodes()) {
                None | Some([]) => None,
                Some([a]) => Some(parse_action_node(a)?),
                Some(_) => return Err(err("slider: at most one action child (on release)")),
            };
            Ok(Ir::Slider {
                styles: parse_style_ref(node)?,
                bind: bind.to_string(),
                min,
                max,
                step,
                action,
            })
        }
        "key" => {
            // `key "down" target="/x"` or `key "delete" { submit "/rm" }` —
            // a page-level keyboard binding, one meaning per combo.
            check_props(node, &["target"])?;
            let combo = positional(node, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("key: first argument is the combo, e.g. \"ctrl+shift+n\""))?;
            crate::validate_key_combo(combo).map_err(|e| err(format!("key {combo:?}: {e}")))?;
            let target = match prop(node, "target").map(|v| v.as_string()) {
                Some(Some(t)) => {
                    validate_path(t).map_err(|e| err(format!("key target {t:?}: {e}")))?;
                    Some(t.to_string())
                }
                Some(None) => return Err(err("key: target must be a string")),
                None => None,
            };
            let action = match node.children().map(|b| b.nodes()) {
                None | Some([]) => None,
                Some([a]) => Some(parse_action_node(a)?),
                Some(_) => return Err(err("key: at most one action child")),
            };
            if target.is_some() == action.is_some() {
                return Err(err("key: exactly one of target=\"/x\" or an action child"));
            }
            Ok(Ir::Key { combo: combo.to_string(), target, action })
        }
        "keys" => {
            // `keys target="/term/key"` — the page takes the whole keyboard.
            // One target, no children: what a key means is decided at the far
            // end, which is the entire point of asking for all of them.
            check_props(node, &["target"])?;
            let target = prop(node, "target")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("keys: target=\"/path\" is required"))?;
            validate_path(target).map_err(|e| err(format!("keys target {target:?}: {e}")))?;
            if node.children().is_some_and(|b| !b.nodes().is_empty()) {
                return Err(err("keys: no children — every key goes to the one target"));
            }
            Ok(Ir::Keys { target: target.to_string() })
        }
        "live" => {
            // `live target="/term/screen" every=50` — the page reloads itself
            // on a clock, so a document can show something that moves without
            // the protocol growing a push channel.
            check_props(node, &["target", "every"])?;
            let target = prop(node, "target")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("live: target=\"/path\" is required"))?;
            validate_path(target).map_err(|e| err(format!("live target {target:?}: {e}")))?;
            let every = prop(node, "every")
                .and_then(|v| v.as_integer())
                .ok_or_else(|| err("live: every=<milliseconds> is required"))?;
            let interval = u16::try_from(every)
                .map_err(|_| err(format!("live: every={every} is out of range")))?;
            if interval < crate::MIN_LIVE_INTERVAL_MS {
                return Err(err(format!(
                    "live: every={interval} is below the {}ms floor",
                    crate::MIN_LIVE_INTERVAL_MS
                )));
            }
            if node.children().is_some_and(|b| !b.nodes().is_empty()) {
                return Err(err("live: no children"));
            }
            Ok(Ir::Live { target: target.to_string(), interval })
        }
        "sensitive" => {
            // `sensitive tier=N` — the page classifies what it shows
            // (specs/history.md decision 4). Only raising exists in the
            // vocabulary: tier=0 is what an undeclared page already is, so
            // declaring it is a misunderstanding worth refusing loudly
            // rather than encoding a no-op.
            check_props(node, &["tier"])?;
            no_children(node)?;
            let tier = prop(node, "tier")
                .and_then(|v| v.as_integer())
                .ok_or_else(|| err("sensitive: tier=<1|2> is required"))?;
            let tier = u8::try_from(tier)
                .ok()
                .filter(|t| (1..=2).contains(t))
                .ok_or_else(|| err(format!("sensitive: tier={tier} is not 1 or 2")))?;
            Ok(Ir::Sensitive { tier })
        }
        "closing" => {
            // `closing target="/term/7/close"` — an action the host fires,
            // best-effort, when the window showing this page closes. The
            // app names its own goodbye; the timeout it keeps anyway stays
            // the safety net for clients that never got to say it.
            check_props(node, &["target"])?;
            let target = prop(node, "target")
                .and_then(|v| v.as_string())
                .ok_or_else(|| err("closing: target=\"/path\" is required"))?;
            validate_path(target).map_err(|e| err(format!("closing target {target:?}: {e}")))?;
            if node.children().is_some_and(|b| !b.nodes().is_empty()) {
                return Err(err("closing: no children — it is a declaration, not a container"));
            }
            Ok(Ir::Closing { target: target.to_string() })
        }
        "page" => {
            // `page background="#00000000"` — what is behind the whole
            // document. A page that is a surface in its own right says so
            // here; everything else takes the desktop's `page` colour and
            // should.
            check_props(node, &["background"])?;
            let value = prop(node, "background")
                .ok_or_else(|| err("page: background=\"…\" is required"))?;
            let color = parse_color(value, "page")?;
            if node.children().is_some_and(|b| !b.nodes().is_empty()) {
                return Err(err("page: no children — it is a declaration, not a container"));
            }
            Ok(Ir::Page { color })
        }
        "menu" => {
            // The element's context menu: items with a label and exactly one
            // of target=/an action child; `separator` between groups.
            check_props(node, &[])?;
            let block = node
                .children()
                .ok_or_else(|| err("menu: needs item children"))?;
            let mut items = Vec::new();
            for child in block.nodes() {
                match child.name().value() {
                    "separator" => {
                        check_props(child, &[])?;
                        items.push(IrMenuItem {
                            label: String::new(),
                            icon: None,
                            target: None,
                            action: None,
                            danger: false,
                            separator: true,
                        });
                    }
                    "item" => {
                        check_props(child, &["icon", "target", "danger"])?;
                        let label = positional(child, 0)
                            .and_then(|v| v.as_string())
                            .filter(|l| !l.is_empty())
                            .ok_or_else(|| err("menu item: first argument is the label"))?;
                        let icon = match prop(child, "icon").map(|v| v.as_string()) {
                            Some(Some(name))
                                if !name.is_empty()
                                    && name.len() <= 40
                                    && name
                                        .chars()
                                        .all(|c| c.is_ascii_lowercase() || c == '-') =>
                            {
                                Some(name.to_string())
                            }
                            Some(_) => {
                                return Err(err("menu item: icon is a lowercase glyph name"));
                            }
                            None => None,
                        };
                        let danger =
                            prop(child, "danger").and_then(|v| v.as_bool()).unwrap_or(false);
                        let target = match prop(child, "target").map(|v| v.as_string()) {
                            Some(Some(t)) => {
                                validate_path(t)
                                    .map_err(|e| err(format!("menu target {t:?}: {e}")))?;
                                Some(t.to_string())
                            }
                            Some(None) => return Err(err("menu item: target must be a string")),
                            None => None,
                        };
                        let action = match child.children().map(|b| b.nodes()) {
                            None | Some([]) => None,
                            Some([a]) => Some(parse_action_node(a)?),
                            Some(_) => return Err(err("menu item: at most one action child")),
                        };
                        if target.is_some() == action.is_some() {
                            return Err(err(
                                "menu item: exactly one of target=\"/x\" or an action child",
                            ));
                        }
                        items.push(IrMenuItem {
                            label: label.to_string(),
                            icon,
                            target,
                            action,
                            danger,
                            separator: false,
                        });
                    }
                    other => return Err(err(format!("menu: unknown child {other:?}"))),
                }
            }
            if items.is_empty() || items.len() > crate::MAX_MENU_ITEMS {
                return Err(err("menu: needs 1..=32 items"));
            }
            Ok(Ir::Menu { items })
        }
        "when" | "unless" => {
            check_props(node, &[])?;
            let state = positional(node, 0)
                .and_then(|v| v.as_string())
                .ok_or_else(|| err(format!("{name}: needs a bool state name")))?;
            let mut kids = children(node)?;
            if kids.len() != 1 {
                return Err(err(format!(
                    "{name}: needs exactly one child (wrap several in a column)"
                )));
            }
            Ok(Ir::When {
                state: state.to_string(),
                invert: name == "unless",
                child: Box::new(kids.remove(0)),
            })
        }
        other => Err(err(format!(
            "unknown node {other:?} (known: text, image, row, column, rect, spacer, link, scroll, button, text_input, slider, when, unless, key, menu, style, state)"
        ))),
    }
}

// -------------------------------------------------------- combo resolution

fn resolve_combos(
    ir: &Ir,
    partials: &BTreeMap<String, Partial>,
    combos: &mut Vec<(String, Partial)>,
    combo_index: &mut BTreeMap<Vec<String>, u16>,
    notes: &mut Vec<String>,
) -> Result<(), DocError> {
    let layers = ir.styles();
    if !layers.is_empty() && !combo_index.contains_key(layers) {
        let mut resolved = Partial::default();
        for layer in layers {
            let partial = partials
                .get(layer)
                .ok_or_else(|| err(format!("unknown style {layer:?}")))?;
            merge(&mut resolved, partial, layer, layers, notes);
        }
        let name = layers.join("+");
        if combos.len() >= u16::MAX as usize {
            return Err(err("too many style combinations"));
        }
        combo_index.insert(layers.to_vec(), combos.len() as u16);
        combos.push((name, resolved));
    }
    match ir {
        Ir::Row { children, .. } | Ir::Column { children, .. } => {
            for child in children {
                resolve_combos(child, partials, combos, combo_index, notes)?;
            }
        }
        Ir::Scroll { child, .. } | Ir::When { child, .. } | Ir::Chrome { child, .. } => {
            resolve_combos(child, partials, combos, combo_index, notes)?
        }
        _ => {}
    }
    Ok(())
}

/// Last-listed wins; every actual override gets a note.
fn merge(
    base: &mut Partial,
    layer: &Partial,
    layer_name: &str,
    combo: &[String],
    notes: &mut Vec<String>,
) {
    let combo_name = combo.join(" ");
    let mut note = |what: &str| {
        notes.push(format!(
            "note: {layer_name:?} overrides {what} in style combo \"{combo_name}\""
        ));
    };
    macro_rules! lay {
        ($field:ident, $label:expr) => {
            if let Some(v) = &layer.$field {
                if base.$field.is_some() && base.$field.as_ref() != Some(v) {
                    note($label);
                }
                base.$field = Some(v.clone());
            }
        };
    }
    lay!(color, "color");
    lay!(background, "background");
    lay!(font_size, "size");
    lay!(font_weight, "weight");
    lay!(corner_radius, "corner");
    lay!(font_family, "font");
    lay!(align, "align");
    lay!(width, "width");
    lay!(height, "height");
    lay!(underline, "underline");
    lay!(size_token, "size");
    lay!(padding_token, "padding");
    lay!(gap_token, "gap");
    lay!(padding_px, "padding");
    lay!(gap_px, "gap");
    lay!(valign, "valign");
    lay!(ellipsis, "ellipsis");
    lay!(padding_x_token, "padding-x");
    lay!(padding_x_px, "padding-x");
    lay!(padding_y_token, "padding-y");
    lay!(padding_y_px, "padding-y");
    lay!(measure_group, "group");
    lay!(shadow_token, "shadow");
    lay!(border, "border");
    lay!(border_color, "border-color");
    lay!(hover, "hover");
    lay!(backdrop, "backdrop");
    lay!(wrap, "wrap");
}

// -------------------------------------------------------------- emission

fn collect_strings(ir: &Ir, set: &mut BTreeSet<String>) {
    match ir {
        Ir::Text { value, .. } => {
            set.insert(value.clone());
        }
        Ir::Code { lang, .. } => {
            set.insert(lang.clone());
        }
        Ir::Icon { name, .. } => {
            set.insert(name.clone());
        }
        Ir::Image { source, .. } => {
            set.insert(source.clone());
        }
        Ir::Link { label, target, .. } => {
            set.insert(label.clone());
            set.insert(target.clone());
        }
        Ir::Row { target, children, .. } | Ir::Column { target, children, .. } => {
            if let Some(t) = target {
                set.insert(t.clone());
            }
            for child in children {
                collect_strings(child, set);
            }
        }
        Ir::Scroll { child, .. } | Ir::When { child, .. } | Ir::Chrome { child, .. } => {
            collect_strings(child, set)
        }
        Ir::Button { label, icon, action, .. } => {
            set.insert(label.clone());
            if let Some(icon) = icon {
                set.insert(icon.clone());
            }
            match action {
                IrAction::Navigate(t) => {
                    set.insert(t.clone());
                }
                IrAction::Submit { endpoint, fields } => {
                    set.insert(endpoint.clone());
                    for (name, _) in fields {
                        set.insert(name.clone());
                    }
                }
                IrAction::Toggle(_)
                | IrAction::Set(..)
                | IrAction::PickFile { .. }
                | IrAction::OpenMenu => {}
            }
        }
        Ir::TextInput { placeholder, action, .. } => {
            set.insert(placeholder.clone());
            if let Some(IrAction::Navigate(t)) = action {
                set.insert(t.clone());
            }
            if let Some(IrAction::Submit { endpoint, fields }) = action {
                set.insert(endpoint.clone());
                for (name, _) in fields {
                    set.insert(name.clone());
                }
            }
        }
        Ir::Slider { action, .. } => {
            if let Some(IrAction::Navigate(t)) = action {
                set.insert(t.clone());
            }
            if let Some(IrAction::Submit { endpoint, fields }) = action {
                set.insert(endpoint.clone());
                for (name, _) in fields {
                    set.insert(name.clone());
                }
            }
        }
        Ir::Menu { items } => {
            for item in items {
                set.insert(item.label.clone());
                if let Some(icon) = &item.icon {
                    set.insert(icon.clone());
                }
                if let Some(t) = &item.target {
                    set.insert(t.clone());
                }
                if let Some(IrAction::Navigate(t)) = &item.action {
                    set.insert(t.clone());
                }
                if let Some(IrAction::Submit { endpoint, fields }) = &item.action {
                    set.insert(endpoint.clone());
                    for (name, _) in fields {
                        set.insert(name.clone());
                    }
                }
            }
        }
        Ir::Keys { target } => {
            set.insert(target.clone());
        }
        Ir::Live { target, .. } => {
            set.insert(target.clone());
        }
        Ir::Sensitive { .. } => {}
        Ir::Closing { target } => {
            set.insert(target.clone());
        }
        Ir::Page { color } => {
            if let PartialColor::Token(name) = color {
                set.insert(name.clone());
            }
        }
        Ir::Key { combo, target, action } => {
            set.insert(combo.clone());
            if let Some(t) = target {
                set.insert(t.clone());
            }
            if let Some(IrAction::Navigate(t)) = action {
                set.insert(t.clone());
            }
            if let Some(IrAction::Submit { endpoint, fields }) = action {
                set.insert(endpoint.clone());
                for (name, _) in fields {
                    set.insert(name.clone());
                }
            }
        }
        Ir::Rectangle { .. } | Ir::Spacer { .. } => {}
    }
}

fn emit(ir: &Ir, ctx: &mut EmitCtx, nodes: &mut Vec<Node>) -> Result<u32, DocError> {
    let style = if ir.styles().is_empty() { NO_STYLE } else { ctx.combo_index[ir.styles()] };
    let string_idx = ctx.string_idx;
    let node = match ir {
        Ir::Text { value, .. } => Node::Text { style, value: string_idx(value) },
        Ir::Image { source, .. } => Node::Image { style, source: string_idx(source) },
        Ir::Icon { name, size, .. } => {
            Node::Icon { style, name: string_idx(name), size: *size }
        }
        Ir::Row { gap, padding, target, children, .. }
        | Ir::Column { gap, padding, target, children, .. } => {
            let mut child_indices = Vec::with_capacity(children.len());
            for child in children {
                child_indices.push(emit(child, ctx, nodes)?);
            }
            let target = target.as_deref().map(string_idx).unwrap_or(NO_STYLE);
            if matches!(ir, Ir::Row { .. }) {
                Node::Row { style, gap: *gap, padding: *padding, target, children: child_indices }
            } else {
                Node::Column {
                    style,
                    gap: *gap,
                    padding: *padding,
                    target,
                    children: child_indices,
                }
            }
        }
        Ir::Rectangle { width, height, .. } => {
            Node::Rectangle { style, width: *width, height: *height }
        }
        Ir::Spacer { size, .. } => Node::Spacer { style, size: *size },
        Ir::Link { label, target, .. } => {
            Node::Link { style, label: string_idx(label), target: string_idx(target) }
        }
        Ir::Scroll { child, .. } => {
            let child_idx = emit(child, ctx, nodes)?;
            Node::Scroll { style, child: child_idx }
        }
        Ir::Chrome { child, .. } => {
            let child_idx = emit(child, ctx, nodes)?;
            Node::Chrome { style, child: child_idx }
        }
        Ir::Button { label, icon, action, .. } => {
            let doc_action = build_doc_action(action, ctx)?;
            let action_idx = ctx.actions.len() as u16;
            ctx.actions.push(doc_action);
            Node::Button {
                style,
                label: string_idx(label),
                icon: icon.as_deref().map(string_idx).unwrap_or(NO_STYLE),
                action: action_idx,
            }
        }
        Ir::TextInput { bind, placeholder, action, multiline, .. } => {
            let state = (ctx.state_ref)(bind)?;
            if !matches!((ctx.state_type)(state), ActionValue::Str(_)) {
                return Err(err(format!("text_input bind {bind:?}: state is not a string")));
            }
            let action_idx = match action {
                None => rill_doc_no_style(),
                Some(a) => {
                    let da = build_doc_action(a, ctx)?;
                    let idx = ctx.actions.len() as u16;
                    ctx.actions.push(da);
                    idx
                }
            };
            Node::TextInput {
                style,
                bind: state,
                placeholder: string_idx(placeholder),
                action: action_idx,
                multiline: *multiline,
            }
        }
        Ir::Code { bind, lang, .. } => {
            let state = (ctx.state_ref)(bind)?;
            if !matches!((ctx.state_type)(state), ActionValue::Str(_)) {
                return Err(err(format!("code bind {bind:?}: state is not a string")));
            }
            Node::Code { style, bind: state, lang: string_idx(lang) }
        }
        Ir::Slider { bind, min, max, step, action, .. } => {
            let state = (ctx.state_ref)(bind)?;
            if !matches!((ctx.state_type)(state), ActionValue::Num(_)) {
                return Err(err(format!("slider bind {bind:?}: state is not a number")));
            }
            let action_idx = match action {
                None => rill_doc_no_style(),
                Some(a) => {
                    let da = build_doc_action(a, ctx)?;
                    let idx = ctx.actions.len() as u16;
                    ctx.actions.push(da);
                    idx
                }
            };
            Node::Slider { style, bind: state, min: *min, max: *max, step: *step, action: action_idx }
        }
        Ir::When { state, invert, child } => {
            let idx = (ctx.state_ref)(state)?;
            if !matches!((ctx.state_type)(idx), ActionValue::Bool(_)) {
                return Err(err(format!("when {state:?}: state is not a bool")));
            }
            let child_idx = emit(child, ctx, nodes)?;
            Node::When { state: idx, invert: *invert, child: child_idx }
        }
        Ir::Menu { items } => {
            let mut out_items = Vec::with_capacity(items.len());
            for item in items {
                let action_idx = match &item.action {
                    None => NO_STYLE,
                    Some(a) => {
                        let da = build_doc_action(a, ctx)?;
                        let idx = ctx.actions.len() as u16;
                        ctx.actions.push(da);
                        idx
                    }
                };
                out_items.push(crate::MenuItem {
                    label: string_idx(&item.label),
                    icon: item.icon.as_deref().map(string_idx).unwrap_or(NO_STYLE),
                    target: item.target.as_deref().map(string_idx).unwrap_or(NO_STYLE),
                    action: action_idx,
                    danger: item.danger,
                    separator: item.separator,
                });
            }
            Node::Menu { items: out_items }
        }
        Ir::Keys { target } => Node::Keys { target: string_idx(target) },
        Ir::Page { color } => Node::Page {
            color: match color {
                PartialColor::Literal(c) => ColorRef::Literal(*c),
                PartialColor::Token(tok) => ColorRef::Token(string_idx(tok)),
            },
        },
        Ir::Live { target, interval } => {
            Node::Live { target: string_idx(target), interval: *interval }
        }
        Ir::Sensitive { tier } => {
            Node::Sensitive { tier: *tier }
        }
        Ir::Closing { target } => Node::Closing { target: string_idx(target) },
        Ir::Key { combo, target, action } => {
            let action_idx = match action {
                None => NO_STYLE,
                Some(a) => {
                    let da = build_doc_action(a, ctx)?;
                    let idx = ctx.actions.len() as u16;
                    ctx.actions.push(da);
                    idx
                }
            };
            Node::Key {
                key: string_idx(combo),
                target: target.as_deref().map(string_idx).unwrap_or(NO_STYLE),
                action: action_idx,
            }
        }
    };
    if nodes.len() as u32 >= crate::MAX_NODES {
        return Err(err("too many nodes"));
    }
    nodes.push(node);
    Ok((nodes.len() - 1) as u32)
}
