//! The draw-command vocabulary and its wire codec.
//!
//! What a window is made of — [`DrawCommand`] and the geometry it is built
//! from — plus [`stream`], the byte format those commands travel and are
//! stored as. Nothing here lays anything out, resolves a style, or measures a
//! glyph: this crate is the *shape* of a frame, so that the things which
//! produce frames and the things which keep them do not have to agree about
//! anything else.
//!
//! It exists because they didn't. `rill-history` — a durable on-disk log —
//! reached up into `rill-ui` to decode a stored frame, dragging a KDL parser,
//! a layout engine, a text shaper and the icon set into a storage crate for
//! the sake of one `decode()`. A `.rhs` file holds `rill-draw` blobs; that is
//! a sentence that should be true of a small, versioned, fuzzed format crate,
//! and not of the layout engine that happens to emit them.
//!
//! The coupling that remains is honest and deliberate: a stored frame speaks
//! this vocabulary, and always will. What this crate buys is that the
//! vocabulary can be frozen while the engine above it churns.

pub mod stream;

pub use rill_protocol::ActionValue;

/// A rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn inset(&self, by: f32) -> Rect {
        Rect {
            x: self.x + by,
            y: self.y + by,
            w: (self.w - 2.0 * by).max(0.0),
            h: (self.h - 2.0 * by).max(0.0),
        }
    }
}

/// A point in logical pixels — the vertices of a [`DrawCommand::Path`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
}

use std::fmt;

/// RGBA color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Parse `#rrggbb` / `#rrggbbaa`.
    pub fn parse_hex(s: &str) -> Option<Color> {
        let hex = s.strip_prefix('#')?;
        if hex.len() != 6 && hex.len() != 8 {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Color {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if hex.len() == 8 { byte(6)? } else { 0xFF },
        })
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 0xFF {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

/// The fastest a page may ask to be reloaded. Sixteen milliseconds is a
/// 60Hz refresh — below that a client is fetching faster than it can paint,
/// and a document that asks for it is describing a busy loop rather than a
/// live view.
pub const MIN_LIVE_INTERVAL_MS: u16 = 16;

/// A resolved declarative action (string indices resolved to owned values;
/// state slots stay as indices into the runtime state vector).
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Open the innermost declared menu at the activation point. Resolved by
    /// the viewport (which knows the menu areas); inert anywhere else.
    OpenMenu,
    Navigate(String),
    Toggle(u16),
    Set(u16, ActionValue),
    Submit { endpoint: String, fields: Vec<(String, u16)> },
    /// Request a file via the broker; content fills the given string slot.
    PickFile { into: u16 },
}

/// One resolved context-menu entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub label: String,
    pub icon: Option<String>,
    pub target: Option<String>,
    pub action: Option<UiAction>,
    pub danger: bool,
    pub separator: bool,
}

/// What a backend paints, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum DrawCommand {
    Rect { rect: Rect, color: Color, corner_radius: f32 },
    /// A soft blurred fill behind `rect` — drop shadow (elevation) or, with
    /// zero blur offset and a colored fill, a glow (focus rings, window
    /// emphasis). `spread` dilates the shape before blurring. Painted before
    /// the content it sits behind.
    Shadow { rect: Rect, color: Color, blur: f32, spread: f32, corner_radius: f32 },
    /// An edge-only glow: a luminous ring hugging `rect`'s outside, zero
    /// coverage inside — focus rings around translucent windows, neon
    /// accents. `blur` is how far the light falls off outward.
    Glow { rect: Rect, color: Color, blur: f32, corner_radius: f32 },
    /// A hairline outline on a (possibly rounded) box. The one shape the
    /// existing primitives could not make: Glow lights only *outside* an
    /// edge, and a pair of rects cannot round a corner. Rides the same SDF
    /// as Rect — a ring is `|distance| < width/2`.
    Border { rect: Rect, color: Color, width: f32, corner_radius: f32 },
    /// A stroked polyline in logical units — the one shape the rect
    /// primitives cannot express. Line and area charts, gauges, sparklines,
    /// and (with enough short segments) any curve. Joins and caps are round,
    /// which is what a plotted line wants and costs nothing: each segment is
    /// drawn as a capsule, so overlapping ends *are* the join.
    ///
    /// `closed` connects the last point back to the first. A single point is
    /// a dot of diameter `width`.
    Path { points: Vec<Point>, color: Color, width: f32, closed: bool },
    /// Filled closed contours (even-odd), flattened — what a glyph-style
    /// icon is made of. `contours` partitions `points` into rings.
    FillPath { points: Vec<Point>, contours: Vec<u32>, color: Color },
    Text {
        rect: Rect,
        text: String,
        color: Color,
        font_size: f32,
        font_weight: u16,
        font_family: String,
    },
    /// The backend resolves/loads the image resource itself.
    Image { rect: Rect, source: String },
    /// Frosted glass: paint a blurred copy of whatever the *host* has already
    /// composited behind `rect` (D6, wgpu-renderer.md). Content painted after
    /// it sits on the frosted pane. Only backdrop-capable hosts (the
    /// compositor) can honor it; others no-op — pair it with a translucent
    /// fill so the panel stays legible either way.
    Backdrop { rect: Rect, blur: f32, corner_radius: f32 },
    /// Clip subsequent commands to `rect`. `radius > 0` rounds the clip:
    /// the rect still bounds it (scissor-cheap), the curve is enforced
    /// per-fragment — this is how a window masks its content to its shape
    /// while shadows and glows, pushed *outside* the clip pair, hug the
    /// same shape from without.
    PushClip { rect: Rect, radius: f32 },
    PopClip,
    /// A page-declared keyboard binding (not painted, no geometry): while
    /// this frame is current and no input is focused, `key` follows `target`
    /// or performs `action` — exactly one is set.
    KeyBind { key: String, target: Option<String>, action: Option<UiAction> },
    /// The page wants every key the host does not reserve, delivered to this
    /// endpoint. Carries no geometry: a keyboard has no position.
    KeyCapture { target: String },
    /// The page reloads itself from this endpoint on a clock.
    LiveRefresh { target: String, interval: u16 },
    /// The element's context menu (not painted): right-click (or an
    /// open-menu control) inside `rect` offers `items`, presented by the
    /// host so menus look and behave the same in every app. Innermost wins:
    /// children's areas precede their parents' in the list.
    MenuArea { rect: Rect, items: Vec<MenuItem> },
    /// An independently scrollable region: its viewport rect and the height
    /// of the content inside it. Emitted by layout so the host can route
    /// wheel input to the region under the cursor and clamp its offset —
    /// and stripped by the viewport before the frame leaves the process, so
    /// it never reaches a wire or a compositor. The clip that bounds the
    /// region's paint is the ordinary PushClip that follows it.
    ScrollArea { rect: Rect, content: f32 },
    /// Interactive hit region (not painted).
    LinkArea { rect: Rect, target: String },
    /// Button hit region carrying its resolved action (not painted).
    ActionArea { rect: Rect, action: UiAction },
    /// Text-input hit region: clicking focuses the bound state slot;
    /// `on_enter` fires when Enter is pressed while focused (single-line);
    /// `multiline` inputs insert a newline on Enter instead.
    InputArea {
        rect: Rect,
        state: u16,
        on_enter: Option<UiAction>,
        multiline: bool,
        /// A code surface: Tab indents instead of moving focus. A form's
        /// Tab walks fields — that contract stays; an editor's Tab is a
        /// character, and both are right where they live.
        tab_inserts: bool,
        // Text metrics so a click can be mapped to a caret position.
        font_size: f32,
        font_weight: u16,
        font_family: String,
        pad_x: f32,
        pad_y: f32,
    },
    /// Slider hit region (not painted): press/drag maps pointer x across
    /// `rect` to a value in `min..=max` (quantized to `step`, 0 = smooth)
    /// written into the numeric `state` slot; `on_release` fires when the
    /// drag ends.
    SliderArea {
        rect: Rect,
        state: u16,
        min: f32,
        max: f32,
        step: f32,
        on_release: Option<UiAction>,
    },
}
