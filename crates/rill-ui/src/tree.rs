//! Document → resolved UI tree: style references become concrete values
//! (style-table lookup + defaults — the runtime half of the no-cascade
//! model), string indices become owned strings, ignorable-unknown nodes
//! disappear.

use std::collections::HashMap;

use rill_doc::{Color, ColorRef, Dimension, DocAction, Document, NO_STYLE, Node};
use rill_draw::{MenuItem, UiAction};


/// Renderer defaults plus the active theme: the token tables an app's
/// `color=accent` / `font=ui` references resolve against, and the
/// enforced-override flag. This is the "named-token lookup, not cascade"
/// model from `specs/theming.md` — one lookup at render time, no
/// inheritance. Growing `Defaults` into the token table is the plan the
/// spec calls for.
#[derive(Debug, Clone)]
pub struct Defaults {
    pub page_background: Color,
    pub text_color: Color,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_family: String,
    pub link_color: Color,
    /// Semantic color tokens (`accent`, `surface`, `text-muted`, …).
    pub color_tokens: HashMap<String, Color>,
    /// Semantic font tokens (`ui`, `mono`, `display`) → concrete family.
    pub font_tokens: HashMap<String, String>,
    /// Type scale (`xs`…`xxl`) → font size. Colours have always been
    /// themeable; sizes were raw numbers, so every page re-invented its own
    /// and cohesion could only be re-achieved by hand. Naming them puts type
    /// under the same control as colour.
    pub size_tokens: HashMap<String, f32>,
    /// Space scale (`xs`…`xl`) → padding/gap. Same argument as the type
    /// scale, and it makes density a theme decision: one table swap re-spaces
    /// every document at once.
    pub space_tokens: HashMap<String, f32>,
    /// What a container gets when the document says nothing about spacing.
    /// This is what makes cohesion the default rather than opt-in: a page
    /// that never thinks about spacing looks like the system instead of
    /// looking cramped.
    pub container_padding: f32,
    pub container_gap: f32,
    /// Elevation scale (`sm`…`lg`) → shadow blur. Two or three steps is the
    /// whole vocabulary a UI needs; more reads as noise.
    pub shadow_tokens: HashMap<String, f32>,
    /// Enforced override: the user's theme re-skins even apps that hardcode
    /// literal colors (a uniform desktop). Off = cooperative: token apps
    /// follow, hardcoded apps keep their own look. The user's choice always
    /// wins — an inert token-referencing document cannot defeat it.
    pub enforce: bool,
}

impl Defaults {
    /// Resolve a semantic color token against the active theme.
    pub fn token(&self, name: &str) -> Option<Color> {
        self.color_tokens.get(name).copied()
    }

    /// Resolve a type-scale token.
    pub fn size_token(&self, name: &str) -> Option<f32> {
        self.size_tokens.get(name).copied()
    }

    /// Resolve a space-scale token.
    pub fn space_token(&self, name: &str) -> Option<f32> {
        self.space_tokens.get(name).copied()
    }

    /// Resolve an elevation token.
    pub fn shadow_token(&self, name: &str) -> Option<f32> {
        self.shadow_tokens.get(name).copied()
    }
}

/// The default type scale. A ratio near 1.25 between steps, rounded to whole
/// pixels because glyph rasterization is happier on integers, and stopping at
/// five steps so a page has to choose rather than fine-tune.
pub fn default_size_scale() -> HashMap<String, f32> {
    [("xs", 11.0), ("sm", 13.0), ("md", 15.0), ("lg", 20.0), ("xl", 26.0)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// The default space scale: a 4px rhythm, doubling-ish, so anything laid out
/// on it lines up with anything else laid out on it.
pub fn default_space_scale() -> HashMap<String, f32> {
    [("xs", 4.0), ("sm", 8.0), ("md", 12.0), ("lg", 20.0), ("xl", 32.0)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// The default elevation scale. Restrained on purpose: depth reads when it
/// is rare, and a page where everything floats is as flat as one where
/// nothing does.
pub fn default_shadow_scale() -> HashMap<String, f32> {
    [("sm", 8.0), ("md", 18.0), ("lg", 32.0)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

impl Default for Defaults {
    fn default() -> Defaults {
        Defaults {
            page_background: Color { r: 0xFA, g: 0xFA, b: 0xF7, a: 0xFF },
            text_color: Color { r: 0x22, g: 0x22, b: 0x28, a: 0xFF },
            font_size: 15.0,
            font_weight: 400,
            font_family: String::new(), // backend's default family
            link_color: Color { r: 0x2A, g: 0x5A, b: 0xDA, a: 0xFF },
            color_tokens: HashMap::new(),
            font_tokens: HashMap::new(),
            size_tokens: default_size_scale(),
            space_tokens: default_space_scale(),
            container_padding: 12.0, // md
            container_gap: 8.0,      // sm
            shadow_tokens: default_shadow_scale(),
            enforce: false,
        }
    }
}

/// A node's fully resolved visual properties. One style lookup, defaults
/// filled — no inheritance, by design (document-format.md §4).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedStyle {
    pub color: Color,
    pub background: Option<Color>,
    pub font_size: f32,
    pub font_weight: u16,
    pub corner_radius: f32,
    pub font_family: String,
    /// Horizontal placement of text inside the width it was given.
    /// `None` means the style never said: boxes fall back to leading,
    /// buttons centre their content — the two defaults that existed before
    /// alignment was expressible on buttons at all.
    pub align: Option<rill_doc::Align>,
    /// Explicit container size, if the style set one.
    pub width: Option<rill_doc::Dimension>,
    pub height: Option<rill_doc::Dimension>,
    /// Whether to paint an underline. `None` means the style never said:
    /// links get one (their oldest affordance), plain text does not.
    /// `Some` is an explicit ask either way — which is what lets a
    /// terminal underline a run of text without every label on the
    /// desktop sprouting a line.
    pub underline: Option<bool>,
    /// Container padding/gap from the theme's space scale, when the style
    /// named a step. `None` leaves the node's own value alone.
    pub padding: Option<f32>,
    pub gap: Option<f32>,
    /// Shadow blur, when the style named an elevation step.
    pub shadow: Option<f32>,
    /// Frosted-glass blur behind the box.
    pub backdrop: Option<f32>,
    /// Lay this row out as a wrapping grid.
    pub wrap: bool,
    /// Where a row puts a child shorter than the row itself.
    /// Per-axis padding; the axis value wins over uniform `padding`.
    pub padding_x: Option<f32>,
    pub padding_y: Option<f32>,
    /// Measure group: laid out at the width of the group's widest member.
    pub measure_group: Option<String>,
    pub valign: rill_doc::Align,
    /// Truncate overrun text to one line ending in `…`.
    pub ellipsis: bool,
    /// Hairline outline width and colour.
    pub border: f32,
    pub border_color: Color,
    /// The style to swap in while the pointer is inside. Boxed because a
    /// ResolvedStyle contains one, and a hover state does not itself hover.
    pub hover: Option<Box<ResolvedStyle>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedNode {
    Text { style: ResolvedStyle, value: String },
    Image { style: ResolvedStyle, source: String },
    Icon { style: ResolvedStyle, name: String, size: Dimension },
    Row { style: ResolvedStyle, gap: Dimension, padding: Dimension, target: Option<String>, children: Vec<ResolvedNode> },
    Column { style: ResolvedStyle, gap: Dimension, padding: Dimension, target: Option<String>, children: Vec<ResolvedNode> },
    Rectangle { style: ResolvedStyle, width: Dimension, height: Dimension },
    Spacer { size: Dimension },
    Link { style: ResolvedStyle, label: String, target: String },
    Scroll { style: ResolvedStyle, child: Box<ResolvedNode> },
    Button { style: ResolvedStyle, label: String, icon: Option<String>, action: UiAction },
    TextInput { style: ResolvedStyle, bind: u16, placeholder: String, on_enter: Option<UiAction>, multiline: bool },
    /// The editable code surface. Class colours are resolved here, once,
    /// from the theme's tokens — layout should not be looking tokens up.
    Code { style: ResolvedStyle, bind: u16, lang: String, class_colors: [Color; 5], gutter: Color, ws: Color },
    /// A horizontal value control on a numeric state slot; `on_release`
    /// fires when the drag ends.
    Slider { style: ResolvedStyle, bind: u16, min: f32, max: f32, step: f32, on_release: Option<UiAction> },
    When { state: u16, invert: bool, child: Box<ResolvedNode> },
    /// Content the window chrome draws. Hoisted out of the flow by
    /// [`resolve`]; one reaching layout is a second chrome node in the same
    /// document and renders as nothing.
    Chrome { style: ResolvedStyle, child: Box<ResolvedNode> },
    /// A page-declared keyboard binding. Zero-size in layout; carries either
    /// a target or an action, exactly like a link or a button — a key is
    /// just an affordance you can't see.
    Key { combo: String, target: Option<String>, action: Option<UiAction> },
    /// The enclosing element's context menu, host-presented. Zero-size; the
    /// container it sits in becomes its hit region.
    Menu { items: Vec<MenuItem> },
    /// The page has asked for the whole keyboard; every key the host does
    /// not reserve goes to this endpoint. Zero-size.
    Keys { target: String },
    /// The page reloads itself from `target` every `interval` ms. Zero-size.
    Live { target: String, interval: u16 },
}


#[derive(Debug, Clone, PartialEq)]
pub struct UiTree {
    pub root: ResolvedNode,
    /// What the document asked the *window* to draw — a toolbar in the
    /// titlebar. Lifted out of `root` so document layout never sees it and a
    /// host that has no chrome to lend simply ignores it.
    pub chrome: Option<ResolvedNode>,
    pub defaults: Defaults,
    /// The tier the document declared for itself (`sensitive tier=N`), 0
    /// when undeclared. Hoisted to the tree like the page background because
    /// it is a property of the document, not of any box in it — and because
    /// every consumer (the host's `set_tier`, the agent surface) must see
    /// the same answer without walking layout.
    pub tier: u8,
}

impl PartialEq for Defaults {
    fn eq(&self, other: &Self) -> bool {
        self.page_background == other.page_background
            && self.text_color == other.text_color
            && self.font_size == other.font_size
            && self.font_weight == other.font_weight
            && self.font_family == other.font_family
            && self.link_color == other.link_color
            && self.color_tokens == other.color_tokens
            && self.font_tokens == other.font_tokens
            && self.enforce == other.enforce
    }
}

/// Resolve a validated document into a render-ready tree.
/// The page background a document asked for, if it did. Read before the tree
/// is walked, because the answer belongs to the tree rather than to any node
/// inside it.
fn declared_page_background(doc: &Document, defaults: &Defaults) -> Option<Color> {
    doc.nodes.iter().find_map(|n| match n {
        Node::Page { color } => match color {
            rill_doc::ColorRef::Literal(c) => Some(*c),
            rill_doc::ColorRef::Token(idx) => defaults.token(doc.string(*idx)),
        },
        _ => None,
    })
}

pub fn resolve(doc: &Document, defaults: Defaults) -> UiTree {
    let mut defaults = defaults;
    if let Some(color) = declared_page_background(doc, &defaults) {
        defaults.page_background = color;
    }
    let mut root = resolve_node(doc, doc.root, &defaults).unwrap_or(ResolvedNode::Spacer {
        size: Dimension::Px(0.0), // root was ignorable-unknown: empty page
    });
    let chrome = take_chrome(&mut root);
    // Multiple declarations take the highest: raising is the only move in
    // the tier vocabulary, so two claims compose by ratchet, not by order.
    let tier = doc
        .nodes
        .iter()
        .filter_map(|n| match n {
            rill_doc::Node::Sensitive { tier } => Some(*tier),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    UiTree { root, chrome, defaults, tier }
}

/// Lift the first chrome subtree out of the flow, leaving a zero spacer in
/// its place. Done here rather than in layout so that every consumer of a
/// tree — layout, hit testing, the agent surface — sees the same split
/// between what the page draws and what the window draws.
fn take_chrome(node: &mut ResolvedNode) -> Option<ResolvedNode> {
    if let ResolvedNode::Chrome { .. } = node {
        let taken = std::mem::replace(node, ResolvedNode::Spacer { size: Dimension::Px(0.0) });
        let ResolvedNode::Chrome { child, .. } = taken else { unreachable!() };
        return Some(*child);
    }
    match node {
        ResolvedNode::Row { children, .. } | ResolvedNode::Column { children, .. } => {
            children.iter_mut().find_map(take_chrome)
        }
        ResolvedNode::Scroll { child, .. } | ResolvedNode::When { child, .. } => take_chrome(child),
        _ => None,
    }
}

/// Container spacing the document did not state. `Auto` is "the system
/// decides" — distinct from an explicit zero, which is why the compiler stops
/// baking unspecified spacing as `Px(0)`. Resolved here rather than in layout
/// so the backend keeps receiving concrete numbers.
fn spacing(dim: Dimension, default: f32) -> Dimension {
    match dim {
        Dimension::Auto => Dimension::Px(default),
        other => other,
    }
}

fn style_of(doc: &Document, style_ref: u16, defaults: &Defaults, link: bool) -> ResolvedStyle {
    style_of_inner(doc, style_ref, defaults, link, true)
}

/// Find a style by name — how a hover reference points at another style.
fn style_named(doc: &Document, name: &str) -> Option<u16> {
    doc.styles.iter().position(|s| doc.string(s.name_idx) == name).map(|i| i as u16)
}

fn style_of_inner(
    doc: &Document,
    style_ref: u16,
    defaults: &Defaults,
    link: bool,
    resolve_hover: bool,
) -> ResolvedStyle {
    let base = ResolvedStyle {
        color: if link { defaults.link_color } else { defaults.text_color },
        background: None,
        font_size: defaults.font_size,
        font_weight: defaults.font_weight,
        corner_radius: 0.0,
        font_family: defaults.font_family.clone(),
        align: None,
        width: None,
        height: None,
        underline: None,
        padding: None,
        gap: None,
        shadow: None,
        backdrop: None,
        padding_x: None,
        padding_y: None,
        measure_group: None,
        valign: rill_doc::Align::Left,
        ellipsis: false,
        wrap: false,
        border: 0.0,
        border_color: Color { r: 0, g: 0, b: 0, a: 0 },
        hover: None,
    };
    if style_ref == NO_STYLE {
        return base;
    }
    let s = &doc.styles[style_ref as usize];
    // Foreground: tokens always resolve against the theme; a literal keeps
    // the app's chosen color unless the user enforces an override, in which
    // case the role default (theme text/link) wins.
    let color = match s.color {
        Some(ColorRef::Token(idx)) => defaults.token(doc.string(idx)).unwrap_or(base.color),
        Some(ColorRef::Literal(_)) if defaults.enforce => base.color,
        Some(ColorRef::Literal(c)) => c,
        None => base.color,
    };
    // Background: tokens resolve; an enforced override re-skins a literal
    // panel to the `surface` token when the theme defines one.
    let background = match s.background {
        Some(ColorRef::Token(idx)) => defaults.token(doc.string(idx)),
        Some(ColorRef::Literal(_)) if defaults.enforce => defaults.token("surface"),
        Some(ColorRef::Literal(c)) => Some(c),
        // An elevation step brings its own surface. A style that names one
        // keeps it — this only fills the gap, so `shadow="md"` alone is
        // enough to make a card read as lifted.
        None => s
            .shadow_token
            .and_then(|i| defaults.token(&format!("elevation-{}", doc.string(i)))),
    };
    // A font family may itself be a semantic token (`ui`, `mono`, …).
    let font_family = match s.font_family {
        Some(idx) => {
            let name = doc.string(idx);
            defaults.font_tokens.get(name).cloned().unwrap_or_else(|| name.to_string())
        }
        None => base.font_family,
    };
    ResolvedStyle {
        color,
        background,
        font_size: s
            .size_token
            .and_then(|i| defaults.size_token(doc.string(i)))
            .or(s.font_size)
            .unwrap_or(base.font_size),
        font_weight: s.font_weight.unwrap_or(base.font_weight),
        corner_radius: s.corner_radius.unwrap_or(0.0),
        font_family,
        align: s.align.or(base.align),
        width: s.width,
        height: s.height,
        underline: s.underline.or(base.underline),
        // A literal wins over a step: a style that says 0 means 0.
        padding: s
            .padding_px
            .or_else(|| s.padding_token.and_then(|i| defaults.space_token(doc.string(i)))),
        gap: s.gap_px.or_else(|| s.gap_token.and_then(|i| defaults.space_token(doc.string(i)))),
        padding_x: s
            .padding_x_px
            .or_else(|| s.padding_x_token.and_then(|i| defaults.space_token(doc.string(i)))),
        padding_y: s
            .padding_y_px
            .or_else(|| s.padding_y_token.and_then(|i| defaults.space_token(doc.string(i)))),
        measure_group: s.measure_group.map(|i| doc.string(i).to_string()),
        shadow: s.shadow_token.and_then(|i| defaults.shadow_token(doc.string(i))),
        backdrop: s.backdrop,
        valign: s.valign.unwrap_or(base.valign),
        ellipsis: s.ellipsis.unwrap_or(base.ellipsis),
        wrap: s.wrap.unwrap_or(base.wrap),
        border: s.border.unwrap_or(0.0),
        border_color: match s.border_color {
            Some(ColorRef::Token(idx)) => defaults.token(doc.string(idx)).unwrap_or(base.color),
            Some(ColorRef::Literal(c)) => c,
            None => base.color,
        },
        // A hover state names another style. Resolved here, once, so layout
        // only has to choose between two finished styles. Not recursive: a
        // hover state that named its own hover would be a loop with nothing
        // to gain.
        hover: resolve_hover
            .then_some(s.hover)
            .flatten()
            .and_then(|idx| style_named(doc, doc.string(idx)))
            .map(|idx| Box::new(style_of_inner(doc, idx, defaults, link, false))),
    }
}

fn resolve_action(doc: &Document, action: &DocAction) -> UiAction {
    match action {
        DocAction::OpenMenu => UiAction::OpenMenu,
        DocAction::Navigate { target } => UiAction::Navigate(doc.string(*target).to_string()),
        DocAction::Toggle { state } => UiAction::Toggle(*state),
        DocAction::SetState { state, value } => UiAction::Set(*state, value.clone()),
        DocAction::Submit { endpoint, fields } => UiAction::Submit {
            endpoint: doc.string(*endpoint).to_string(),
            fields: fields
                .iter()
                .map(|(name, state)| (doc.string(*name).to_string(), *state))
                .collect(),
        },
        DocAction::PickFile { into } => UiAction::PickFile { into: *into },
    }
}

fn resolve_node(doc: &Document, index: u32, defaults: &Defaults) -> Option<ResolvedNode> {
    let node = &doc.nodes[index as usize];
    Some(match node {
        Node::Text { style, value } => ResolvedNode::Text {
            style: style_of(doc, *style, defaults, false),
            value: doc.string(*value).to_string(),
        },
        Node::Icon { style, name, size } => ResolvedNode::Icon {
            style: style_of(doc, *style, defaults, false),
            name: doc.string(*name).to_string(),
            size: *size,
        },
        Node::Image { style, source } => ResolvedNode::Image {
            style: style_of(doc, *style, defaults, false),
            source: doc.string(*source).to_string(),
        },
        Node::Row { style, gap, padding, target, children } => ResolvedNode::Row {
            style: style_of(doc, *style, defaults, false),
            gap: spacing(*gap, defaults.container_gap),
            padding: spacing(*padding, defaults.container_padding),
            target: (*target != rill_doc::NO_STYLE).then(|| doc.string(*target).to_string()),
            children: children
                .iter()
                .filter_map(|&c| resolve_node(doc, c, defaults))
                .collect(),
        },
        Node::Column { style, gap, padding, target, children } => ResolvedNode::Column {
            style: style_of(doc, *style, defaults, false),
            gap: spacing(*gap, defaults.container_gap),
            padding: spacing(*padding, defaults.container_padding),
            target: (*target != rill_doc::NO_STYLE).then(|| doc.string(*target).to_string()),
            children: children
                .iter()
                .filter_map(|&c| resolve_node(doc, c, defaults))
                .collect(),
        },
        Node::Rectangle { style, width, height } => ResolvedNode::Rectangle {
            style: style_of(doc, *style, defaults, false),
            width: *width,
            height: *height,
        },
        Node::Spacer { size, .. } => ResolvedNode::Spacer { size: *size },
        Node::Link { style, label, target } => ResolvedNode::Link {
            style: style_of(doc, *style, defaults, true),
            label: doc.string(*label).to_string(),
            target: doc.string(*target).to_string(),
        },
        Node::Chrome { style, child } => ResolvedNode::Chrome {
            style: style_of(doc, *style, defaults, false),
            child: Box::new(resolve_node(doc, *child, defaults)?),
        },
        Node::Scroll { style, child } => ResolvedNode::Scroll {
            style: style_of(doc, *style, defaults, false),
            child: Box::new(resolve_node(doc, *child, defaults)?),
        },
        Node::Button { style, label, icon, action } => ResolvedNode::Button {
            style: style_of(doc, *style, defaults, false),
            label: doc.string(*label).to_string(),
            icon: (*icon != rill_doc::NO_STYLE).then(|| doc.string(*icon).to_string()),
            action: resolve_action(doc, &doc.actions[*action as usize]),
        },
        Node::Code { style, bind, lang } => {
            let style = style_of(doc, *style, defaults, false);
            let tok = |name: &str, fallback: Color| defaults.token(name).unwrap_or(fallback);
            ResolvedNode::Code {
                bind: *bind,
                lang: doc.string(*lang).to_string(),
                // The class palette, dressed by the theme: keywords in the
                // accent, strings and numbers in the terminal's own greens
                // and cyans, comments muted. A theme that names none of
                // them degrades to the text colour — plain code, exactly
                // what un-highlighted code already was.
                class_colors: [
                    style.color,
                    tok(crate::code::Class::Comment.token(), style.color),
                    tok(crate::code::Class::String.token(), style.color),
                    tok(crate::code::Class::Number.token(), style.color),
                    tok(crate::code::Class::Keyword.token(), style.color),
                ],
                gutter: tok("text-muted", style.color),
                ws: tok("border", style.color),
                style,
            }
        }
        Node::TextInput { style, bind, placeholder, action, multiline } => ResolvedNode::TextInput {
            style: style_of(doc, *style, defaults, false),
            bind: *bind,
            placeholder: doc.string(*placeholder).to_string(),
            on_enter: (*action != rill_doc::NO_STYLE)
                .then(|| resolve_action(doc, &doc.actions[*action as usize])),
            multiline: *multiline,
        },
        Node::Slider { style, bind, min, max, step, action } => ResolvedNode::Slider {
            style: style_of(doc, *style, defaults, false),
            bind: *bind,
            min: *min,
            max: *max,
            step: *step,
            on_release: (*action != rill_doc::NO_STYLE)
                .then(|| resolve_action(doc, &doc.actions[*action as usize])),
        },
        Node::When { state, invert, child } => ResolvedNode::When {
            state: *state,
            invert: *invert,
            child: Box::new(resolve_node(doc, *child, defaults)?),
        },
        Node::Menu { items } => ResolvedNode::Menu {
            items: items
                .iter()
                .map(|item| MenuItem {
                    label: doc.string(item.label).to_string(),
                    icon: (item.icon != rill_doc::NO_STYLE)
                        .then(|| doc.string(item.icon).to_string()),
                    target: (item.target != rill_doc::NO_STYLE)
                        .then(|| doc.string(item.target).to_string()),
                    action: (item.action != rill_doc::NO_STYLE)
                        .then(|| resolve_action(doc, &doc.actions[item.action as usize])),
                    danger: item.danger,
                    separator: item.separator,
                })
                .collect(),
        },
        // Page carries no geometry and no children: it is answered during
        // resolve by changing the tree's own background, so nothing
        // downstream has to know it existed.
        Node::Page { .. } => return None,
        // Closing is a document-level declaration the host reads straight
        // off the decoded document (AppView::close_target); it has no place
        // in the resolved tree.
        Node::Closing { .. } => return None,
        // Hoisted to the tree in `resolve`; invisible to layout.
        Node::Sensitive { .. } => return None,
        Node::Keys { target } => ResolvedNode::Keys { target: doc.string(*target).to_string() },
        Node::Live { target, interval } => ResolvedNode::Live {
            target: doc.string(*target).to_string(),
            interval: *interval,
        },
        Node::Key { key, target, action } => ResolvedNode::Key {
            combo: doc.string(*key).to_string(),
            target: (*target != rill_doc::NO_STYLE).then(|| doc.string(*target).to_string()),
            action: (*action != rill_doc::NO_STYLE)
                .then(|| resolve_action(doc, &doc.actions[*action as usize])),
        },
        // Ignorable-unknown nodes vanish at resolution (their skip semantics).
        Node::UnknownIgnorable { .. } => return None,
    })
}
