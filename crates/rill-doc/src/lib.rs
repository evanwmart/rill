//! The Rill binary document format (`specs/document-format.md`).
//!
//! * [`compile`] — KDL source → canonical `.rill` bytes (deterministic);
//! * [`decode`] — strict validation → [`Document`];
//! * [`encode`] — [`Document`] → bytes (the compiler's back half; exposed
//!   for tests and tooling).

mod codec;
mod compile;

pub use codec::{decode, encode, KNOWN_STYLE_BITS};
pub use compile::{Compiled, compile};
pub use rill_protocol::ActionValue;
// `Color` and the live-refresh floor are the draw vocabulary's, not the
// document's: a colour is what a frame carries, and a document merely names
// one. Re-exported so `rill_doc::Color` keeps working for everything that
// already says it.
pub use rill_draw::{Color, MIN_LIVE_INTERVAL_MS};

use std::fmt;

pub const MAGIC: [u8; 4] = *b"RDOC";
/// Document format version. Bump this whenever the *layout* of the encoding
/// changes, not merely when a field is added — a reader that misparses old
/// bytes reports nonsense like "string index 256 out of range", which sends
/// you hunting a bug in the document instead of rebuilding a stale binary.
///
/// 2: style bitmap widened from 16 to 32 bits (the 16 were exactly used up).
pub const VERSION: u8 = 8;
pub const HEADER_LEN: usize = 32;
pub const MAX_DOC_SIZE: usize = 16 * 1024 * 1024;
pub const MAX_NODES: u32 = 65_536;
/// Deepest nesting a document may declare. Every consumer walks the tree by
/// recursion — resolve, measure, layout, paint — so depth is stack frames, and
/// `MAX_NODES` alone permits a 65k-deep single-child chain that overflows the
/// stack and aborts the process. 256 is far past any authored layout (the
/// deepest in-tree page is under 20) and shallow enough that the deepest walk
/// costs kilobytes of stack.
pub const MAX_DEPTH: u32 = 256;
/// style_ref value meaning "no style".
pub const NO_STYLE: u16 = 0xFFFF;

/// Node types < this are critical (unknown → reject); ≥ are ignorable
/// (unknown → skip node). See document-format.md §6.
pub const IGNORABLE_TYPE_START: u16 = 0x8000;

/// Validates a key-combo string: optional modifiers then one key name,
/// '+'-separated — "down", "delete", "ctrl+shift+n". The canonical form is
/// the one the viewer builds when a key arrives (modifiers in ctrl, shift
/// order), so anything else is rejected here rather than silently never
/// matching. "alt" is absent on purpose: hosts don't deliver it yet.
pub fn validate_key_combo(combo: &str) -> Result<(), String> {
    let mut parts = combo.split('+').rev();
    let key = parts.next().filter(|k| !k.is_empty()).ok_or("empty key name")?;
    if key.len() > 16 || !key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(format!("bad key name {key:?} (lowercase ascii, digits, '-')"));
    }
    let mods: Vec<&str> = parts.collect();
    match mods.as_slice() {
        [] | ["ctrl"] | ["shift"] | ["shift", "ctrl"] => Ok(()),
        m => Err(format!(
            "bad modifiers {:?} (canonical order: ctrl+shift+<key>)",
            m.iter().rev().collect::<Vec<_>>()
        )),
    }
}

#[derive(Debug)]
pub struct DocError(pub String);

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DocError {}

pub(crate) fn err(m: impl Into<String>) -> DocError {
    DocError(m.into())
}

/// Escape a string as a KDL double-quoted string **literal** — the surrounding
/// quotes included. Use this for *every* value interpolated into generated KDL,
/// especially remote-influenced strings (app names, titles, error text): it is
/// the one correct escaper, replacing ad-hoc `{:?}` (Rust Debug is not KDL) and
/// per-call `esc` helpers. Quotes, backslashes, and newlines/tabs are escaped;
/// other control characters collapse to a space so nothing can break out of the
/// string or inject nodes.
pub fn kdl_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A sizing value (document-format.md §5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dimension {
    Auto,
    Px(f32),
    Fill(f32),
}

/// A style color: either a baked-in literal, or a reference to a semantic
/// theme token (`accent`, `surface`, …) resolved by the client against the
/// active theme at render time. Token references are how an app opts into
/// the user's theme without any cascade — see `specs/theming.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRef {
    Literal(Color),
    /// String index of the token name.
    Token(u16),
}

/// A declared state variable (document-format.md): typed by its initial
/// value; the complete state space of a document is this table.
#[derive(Debug, Clone, PartialEq)]
pub struct StateVar {
    pub name_idx: u16,
    pub initial: ActionValue,
}

/// A declarative action, referenced by Button nodes.
#[derive(Debug, Clone, PartialEq)]
pub enum DocAction {
    /// Navigate to a resource path.
    Navigate { target: u16 },
    /// Set a state slot to a literal value (type must match the slot).
    SetState { state: u16, value: ActionValue },
    /// Toggle a bool state slot.
    Toggle { state: u16 },
    /// Submit named fields (drawn from state slots) to an endpoint; the
    /// response is a document.
    Submit { endpoint: u16, fields: Vec<(u16, u16)> },
    /// Request a file through the capability broker; its text content is
    /// placed into the given string state slot (application-model.md §10).
    PickFile { into: u16 },
    /// Open the innermost declared menu at the activation point — how a
    /// visible affordance (the ⋯ pip) opens the same menu right-click does.
    /// Presented by the host, not the page; carries nothing.
    OpenMenu,
}

/// A resolved (flattened) style — partial: unset properties fall to renderer
/// defaults. No inheritance exists anywhere in the model.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Style {
    /// String index of the style's debug name.
    pub name_idx: u16,
    pub color: Option<ColorRef>,
    pub background: Option<ColorRef>,
    pub font_size: Option<f32>,
    pub font_weight: Option<u16>,
    pub corner_radius: Option<f32>,
    /// String index of the font family.
    pub font_family: Option<u16>,
    /// Horizontal alignment of text within the space it was given.
    pub align: Option<Align>,
    /// Explicit size for a container. Leaf nodes (rect, spacer) carry their
    /// own; this is what lets a row hold a fixed sidebar beside a filling
    /// pane, which containers otherwise cannot express because they always
    /// flex.
    pub width: Option<Dimension>,
    pub height: Option<Dimension>,
    /// Whether a link paints its underline. Links default to underlined
    /// because a document's links should look like links; a list row styled
    /// as a row should not.
    pub underline: Option<bool>,
    /// String index of a type-scale token (`sm`, `lg`, …). Takes precedence
    /// over `font_size`, so a page that names a step follows the theme's
    /// scale while one that gives a number keeps its number.
    pub size_token: Option<u16>,
    /// String indices of space-scale tokens for a container's padding and
    /// gap. Spacing lives in the style for the same reason width does: the
    /// node format has no room to grow, and the style bitmap does.
    pub padding_token: Option<u16>,
    pub gap_token: Option<u16>,
    /// Literal padding/gap, for the cases a scale step cannot express —
    /// chiefly zero, which has no token and never should have one.
    pub padding_px: Option<f32>,
    pub gap_px: Option<f32>,
    /// Per-axis padding overrides. A toolbar is the canonical case: wide
    /// horizontal insets to line up with the pane below, near-zero vertical
    /// so its controls fit the strip. Same number-or-scale-step rule as
    /// `padding`; the axis value wins over the uniform one.
    pub padding_x_token: Option<u16>,
    pub padding_x_px: Option<f32>,
    pub padding_y_token: Option<u16>,
    pub padding_y_px: Option<f32>,
    /// Measure group (string index): every element sharing a group is laid
    /// out at the width of the group's widest member — table columns whose
    /// width comes from content, not from a hand-typed number.
    pub measure_group: Option<u16>,
    /// String index of an elevation step. The renderer has always drawn
    /// shadows — the compositor lifts every window with one — but a document
    /// had no way to ask for depth.
    pub shadow_token: Option<u16>,
    /// Hairline outline: width in logical px, and a colour that resolves
    /// like any other.
    pub border: Option<f32>,
    pub border_color: Option<ColorRef>,
    /// Lay a row out as a wrapping grid: children flow left to right and
    /// start a new line when the next one will not fit. The one shape a file
    /// manager is actually built from, and a row could not make.
    pub wrap: Option<bool>,
    /// How a row places children shorter than the row itself. Rows have always
    /// hung children from the top, which is why a label beside a button looks
    /// like it is floating above it.
    pub valign: Option<Align>,
    /// Clip overrun text to one line ending in `…` instead of wrapping. A
    /// wrapped label in a fixed-size tile pushes the grid out of rhythm; a
    /// truncated one does not.
    pub ellipsis: Option<bool>,
    /// Frosted-glass blur behind this box. The renderer has always drawn it —
    /// rill-vector's own titlebar frosts the desktop through it — but a
    /// document could not ask. Pair it with a translucent background: hosts
    /// without a backdrop no-op it, and the fill has to carry the panel
    /// either way.
    pub backdrop: Option<f32>,
    /// String index of the style to swap in while the pointer is inside this
    /// element. Static pages are why a UI reads as printed rather than
    /// alive; this is the cheapest way to answer the cursor.
    pub hover: Option<u16>,
}

/// Where text sits within its line box. Resolved at layout time into an
/// x-offset, so it costs nothing at paint time and needs no new DrawCommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    pub fn from_u8(v: u8) -> Option<Align> {
        match v {
            0 => Some(Align::Left),
            1 => Some(Align::Center),
            2 => Some(Align::Right),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Align::Left => 0,
            Align::Center => 1,
            Align::Right => 2,
        }
    }

    /// Fraction of the leftover space that goes before the text.
    pub fn leading_fraction(self) -> f32 {
        match self {
            Align::Left => 0.0,
            Align::Center => 0.5,
            Align::Right => 1.0,
        }
    }
}

/// One context-menu entry. A separator renders as a rule; an item has a
/// label and exactly one of `target`/`action` (`NO_STYLE` = absent), an
/// optional icon, and `danger` for destructive verbs (styled apart, per the
/// kit's rule that destruction never looks like its neighbours).
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub label: u16,
    pub icon: u16,
    pub target: u16,
    pub action: u16,
    pub danger: bool,
    pub separator: bool,
}

/// Menu-size cap: a context menu longer than this is a design bug.
pub const MAX_MENU_ITEMS: usize = 32;


/// A decoded node. String/style/child references are table indices.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Text { style: u16, value: u16 },
    Image { style: u16, source: u16 },
    /// `target` (string index, NO_STYLE = none) makes the whole container a
    /// click target — a file row opens wherever you click it, not only on
    /// its label. Interactive children win inside it (hit-testing is
    /// document order, and they come first).
    Row { style: u16, gap: Dimension, padding: Dimension, target: u16, children: Vec<u32> },
    Column { style: u16, gap: Dimension, padding: Dimension, target: u16, children: Vec<u32> },
    Rectangle { style: u16, width: Dimension, height: Dimension },
    Spacer { style: u16, size: Dimension },
    Link { style: u16, label: u16, target: u16 },
    Scroll { style: u16, child: u32 },
    /// Interactive: performs its action (by table index) when activated.
    /// `icon` (string index; NO_STYLE = none) draws a named glyph before —
    /// or instead of — the label, so a toolbar control is a real icon
    /// rather than a unicode character wearing a button.
    Button { style: u16, label: u16, icon: u16, action: u16 },
    /// Text input bound to a string state slot; `action` (0xFFFF = none)
    /// fires on Enter (single-line only); `multiline` wraps and grows and
    /// makes Enter insert a newline.
    TextInput { style: u16, bind: u16, placeholder: u16, action: u16, multiline: bool },
    /// An editable code surface: the bound state rendered as a highlighted
    /// mono grid with a line-number gutter, caret and all — one mode, the
    /// way an editor is one thing. `lang` names the language (usually the
    /// file extension); the client's lexer decides what that means, and an
    /// unknown language is rendered plain, which is the correct amount of
    /// colour for a language nobody can lex.
    Code { style: u16, bind: u16, lang: u16 },
    /// A horizontal value control bound to a numeric state slot. Dragging
    /// (or clicking the track) writes the pointer's position into the slot,
    /// quantized to `step` (0 = continuous) and clamped to `min..=max`;
    /// releasing fires `action` (0xFFFF = none) — typically a submit whose
    /// field reads the same slot. A value is data like any other: the slider
    /// carries its range in the document, and what the number *means* stays
    /// at the far end.
    Slider { style: u16, bind: u16, min: f32, max: f32, step: f32, action: u16 },
    /// Shows its child iff bool state slot == !invert.
    When { state: u16, invert: bool, child: u32 },
    /// A named glyph from the built-in set, drawn as strokes. Named rather
    /// than carrying path data: an icon a document could draw itself would be
    /// arbitrary untrusted geometry, and the set is small enough to ship.
    Icon { style: u16, name: u16, size: Dimension },
    /// Content the *window chrome* draws instead of the document body: a
    /// toolbar that lives in the titlebar. The host decides where it goes;
    /// the document only says which nodes belong there.
    Chrome { style: u16, child: u32 },
    /// The element's context menu: what right-click (and the same menu via
    /// an `open-menu` control) offers here. Declared data like every other
    /// affordance — the *host* presents it, so menus look and behave the
    /// same in every app. Zero-size in layout; belongs inside the container
    /// it describes.
    Menu { items: Vec<MenuItem> },
    /// A keyboard binding the page declares: while this document is front
    /// and no input is focused, pressing `key` (a combo string like "down"
    /// or "ctrl+shift+n") follows `target` or performs `action` — exactly
    /// one of the two is set (the other is NO_STYLE). Declarative like every
    /// other affordance: what a key means is page content, inspectable and
    /// remotable, not viewer configuration. Invisible; zero-size in layout.
    Key { key: u16, target: u16, action: u16 },
    /// The page asks for the whole keyboard: while this document is front,
    /// every keystroke the host does not reserve is delivered to `target` as
    /// an action carrying the key name, its text, and the modifiers. For
    /// documents that are a keyboard surface in their own right — a
    /// terminal, an editor, a game — where enumerating bindings is not
    /// possible because the meaning of a key belongs to the far end.
    ///
    /// The host keeps `ctrl+shift+<key>`, which is already the canonical
    /// binding namespace, so there is always a way out of a capturing page.
    /// Invisible; zero-size in layout.
    Keys { target: u16 },
    /// The page re-fetches itself: the client reloads `target` every
    /// `interval` milliseconds and swaps the result in place, keeping scroll
    /// and focus. What lets a document show something that changes without
    /// anybody touching it — output arriving, a log growing, a meter moving
    /// — while the protocol stays a client-driven request/response.
    /// The target may carry `{w}` and `{h}`, which the client replaces with
    /// the pixel size of the area the document was laid into — the one thing
    /// a served page cannot otherwise know about the window it landed in. A
    /// terminal needs it to size its grid; a chart needs it to pick buckets.
    ///
    /// Invisible; zero-size in layout.
    Live { target: u16, interval: u16 },
    /// The document classifies itself: `sensitive tier=N` declares that what
    /// this page shows records at tier N, not T0 (specs/history.md decision
    /// 4). The served document is the app's only channel to its own viewer,
    /// so this node is the server→client leg of the tier chain; the client
    /// carries it the rest of the way over `rill_stream_v1::set_tier`,
    /// latched with the next frame.
    ///
    /// **Critical, deliberately** — the one declaration node in the critical
    /// half. `closing` is ignorable because skipping it degrades to a
    /// timeout; skipping *this* records the page at T0, a fail-open on a
    /// classification control. A viewer too old to understand it must
    /// refuse the document rather than render it at the wrong tier.
    ///
    /// Invisible; zero-size in layout.
    Sensitive { tier: u8 },
    /// An action to fire, best-effort, when the window showing this page
    /// closes. The app names its own goodbye: a terminal ends its session
    /// instead of waiting out the idle reaper, a music player stops
    /// playback. Hosts fire it with a short budget on the way out and never
    /// wait on the answer — a crashed client still falls back to whatever
    /// timeout the app keeps, so this is a courtesy, not the lifetime.
    ///
    /// A declaration, not an element: read from the document, never gated
    /// by `when`. Invisible; zero-size in layout.
    ///
    /// The first assignment in the *ignorable* type half (0x8000+): a
    /// viewer that predates it skips the node and keeps the timeout
    /// behaviour, which is precisely the degradation the split was built
    /// for. (0x8000 itself stays unassigned, mirroring the 0x0000 canary.)
    Closing { target: u16 },
    /// The colour behind the whole page, overriding the theme's `page`.
    ///
    /// A document normally sits on the desktop's page colour and has no say
    /// in it, which is right for a document. It is wrong for a page that
    /// *is* a surface — a terminal, a viewer, a canvas — where the window's
    /// own material should show through instead of a panel being painted
    /// over it. An alpha of zero means "paint nothing here", and a host with
    /// glass takes its body tint from this colour, so a clear page is a
    /// clear window. Invisible; zero-size in layout.
    Page { color: ColorRef },
    /// An unknown node from the ignorable type range: skipped when
    /// rendering, preserved for tree accounting.
    UnknownIgnorable { node_type: u16 },
}

impl Node {
    pub fn type_code(&self) -> u16 {
        match self {
            Node::Text { .. } => 0x0001,
            Node::Image { .. } => 0x0002,
            Node::Row { .. } => 0x0003,
            Node::Column { .. } => 0x0004,
            Node::Rectangle { .. } => 0x0005,
            Node::Spacer { .. } => 0x0006,
            Node::Link { .. } => 0x0007,
            Node::Scroll { .. } => 0x0008,
            Node::Button { .. } => 0x0009,
            Node::TextInput { .. } => 0x000A,
            Node::Code { .. } => 0x0015,
            Node::When { .. } => 0x000B,
            Node::Icon { .. } => 0x000C,
            Node::Chrome { .. } => 0x000D,
            Node::Key { .. } => 0x000E,
            Node::Menu { .. } => 0x000F,
            Node::Keys { .. } => 0x0010,
            Node::Live { .. } => 0x0011,
            Node::Sensitive { .. } => 0x0014,
            Node::Page { .. } => 0x0012,
            Node::Slider { .. } => 0x0013,
            Node::Closing { .. } => 0x8001,
            Node::UnknownIgnorable { node_type } => *node_type,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Node::Text { .. } => "Text",
            Node::Image { .. } => "Image",
            Node::Row { .. } => "Row",
            Node::Column { .. } => "Column",
            Node::Rectangle { .. } => "Rectangle",
            Node::Spacer { .. } => "Spacer",
            Node::Link { .. } => "Link",
            Node::Scroll { .. } => "Scroll",
            Node::Button { .. } => "Button",
            Node::TextInput { .. } => "TextInput",
            Node::Code { .. } => "Code",
            Node::When { .. } => "When",
            Node::Icon { .. } => "Icon",
            Node::Chrome { .. } => "Chrome",
            Node::Key { .. } => "Key",
            Node::Menu { .. } => "Menu",
            Node::Keys { .. } => "Keys",
            Node::Live { .. } => "Live",
            Node::Sensitive { .. } => "Sensitive",
            Node::Page { .. } => "Page",
            Node::Slider { .. } => "Slider",
            Node::Closing { .. } => "Closing",
            Node::UnknownIgnorable { .. } => "(ignorable unknown)",
        }
    }

    pub fn children(&self) -> &[u32] {
        match self {
            Node::Row { children, .. } | Node::Column { children, .. } => children,
            Node::Scroll { child, .. }
            | Node::When { child, .. }
            | Node::Chrome { child, .. } => {
                std::slice::from_ref(child)
            }
            _ => &[],
        }
    }
}

/// A fully validated document.
#[derive(Debug, Clone)]
pub struct Document {
    pub strings: Vec<String>,
    pub styles: Vec<Style>,
    pub states: Vec<StateVar>,
    pub actions: Vec<DocAction>,
    pub nodes: Vec<Node>,
    pub root: u32,
    /// Non-fatal things noticed while decoding — chiefly properties written
    /// by a newer build that this one skipped. Not part of the document's
    /// identity: two documents that differ only here are the same document,
    /// so this is excluded from equality and never re-encoded.
    pub warnings: Vec<String>,
}

/// Equality is over the document itself. `warnings` records what *this*
/// build made of the bytes, not what the document says, so two decoders
/// disagreeing about an unknown property still agree about the document.
impl PartialEq for Document {
    fn eq(&self, other: &Self) -> bool {
        self.strings == other.strings
            && self.styles == other.styles
            && self.states == other.states
            && self.actions == other.actions
            && self.nodes == other.nodes
            && self.root == other.root
    }
}

impl Document {
    pub fn string(&self, idx: u16) -> &str {
        &self.strings[idx as usize]
    }
}
