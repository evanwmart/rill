//! Layout: resolved tree → positioned [`DrawCommand`]s.
//!
//! Deliberately simple, documented semantics (v1):
//!
//! * Columns flow top-to-bottom; children are block-like (width = available
//!   width unless a node has an explicit px size).
//! * Rows flow left-to-right in three passes: px children fixed, leaf auto
//!   children (text, links, images) measured at intrinsic width against
//!   remaining space, and flexible children — fill rects/spacers, auto
//!   spacers, and **any nested Row/Column/Scroll** — share the leftover
//!   equally by weight. Containers flex rather than content-size because a
//!   container's "natural" width is unbounded; give one structure with its
//!   own padding and children. Row height = tallest child.
//! * Text wraps at available width via the backend's [`TextMeasurer`].
//! * `fill` on column children and spacers only takes effect when the
//!   column's height is definite; in normal document flow (unbounded
//!   height) they collapse to zero.
//! * Scroll clips to the viewport height when definite; otherwise it is a
//!   transparent container (static/headless rendering).

use crate::tree::{ResolvedNode, ResolvedStyle, UiTree};
use rill_draw::DrawCommand;
use rill_doc::Align;
use crate::{ActionValue, Color, Dimension, Rect};

/// Backend-provided text measurement. Must wrap exactly like the backend's
/// painter (same engine ⇒ same numbers).
pub trait TextMeasurer {
    fn measure(
        &mut self,
        text: &str,
        font_size: f32,
        font_weight: u16,
        font_family: &str,
        max_width: f32,
    ) -> LineMetrics;
}

/// A measured text block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineMetrics {
    pub width: f32,
    pub height: f32,
}

/// Backend-provided image knowledge: natural pixel size once a resource is
/// loaded. `None` lays out the documented placeholder box.
pub trait ImageSizer {
    fn natural_size(&mut self, source: &str) -> Option<(f32, f32)>;
}

/// An [`ImageSizer`] that knows nothing (placeholders for every image).
pub struct NoImages;

impl ImageSizer for NoImages {
    fn natural_size(&mut self, _source: &str) -> Option<(f32, f32)> {
        None
    }
}


#[derive(Debug, Clone, Copy)]
pub struct LayoutOptions {
    pub viewport_width: f32,
    /// `None` = document flow: the page is as tall as its content.
    pub viewport_height: Option<f32>,
}

/// Lay out a resolved tree. Returns the command list and the total content
/// height (≥ viewport height when one was given).
#[allow(clippy::too_many_arguments)]
pub fn layout_document(
    tree: &UiTree,
    options: LayoutOptions,
    measurer: &mut dyn TextMeasurer,
    images: &mut dyn ImageSizer,
    state: &[ActionValue],
    focused: Option<u16>,
    // Caret byte-offset within the focused text input's string.
    caret: usize,
    // Selected byte range `(lo, hi)` in the focused input; `lo == hi` = none.
    selection: (usize, usize),
    // Cursor position in document space, for hover/press feedback.
    cursor: Option<(f32, f32)>,
    // Whether the mouse button is currently down.
    pressing: bool,
) -> (Vec<DrawCommand>, f32) {
    layout_document_with_scroll(
        tree, options, measurer, images, state, focused, caret, selection, cursor, pressing, &[],
    )
}

/// [`layout_document`], with per-region scroll offsets for the document's
/// `Scroll` nodes, in document order. The offsets shift each region's child
/// up under its clip, which is what makes a region *independently*
/// scrollable: the content inside moves, the rail beside it does not, and
/// every hit rect the shifted content emits is already in shifted
/// coordinates — clicking works without a second coordinate space.
#[allow(clippy::too_many_arguments)]
pub fn layout_document_with_scroll(
    tree: &UiTree,
    options: LayoutOptions,
    measurer: &mut dyn TextMeasurer,
    images: &mut dyn ImageSizer,
    state: &[ActionValue],
    focused: Option<u16>,
    caret: usize,
    selection: (usize, usize),
    cursor: Option<(f32, f32)>,
    pressing: bool,
    scroll_offsets: &[f32],
) -> (Vec<DrawCommand>, f32) {
    let mut body = Vec::new();
    let mut groups = std::collections::HashMap::new();
    measure_groups(&tree.root, measurer, &mut groups);
    let size = layout_node(
        &tree.root,
        &mut Ctx {
            backdrops: 0,
            groups,
            measurer,
            images,
            state,
            focused,
            caret,
            selection,
            cursor,
            pressing,
            scroll_offsets,
            scroll_seen: 0,
        },
        Rect { x: 0.0, y: 0.0, w: options.viewport_width, h: 0.0 },
        options.viewport_height,
        &mut body,
    );
    let total_height = size.1.max(options.viewport_height.unwrap_or(0.0));
    let mut commands = vec![DrawCommand::Rect {
        rect: Rect { x: 0.0, y: 0.0, w: options.viewport_width, h: total_height },
        color: tree.defaults.page_background,
        corner_radius: 0.0,
    }];
    commands.extend(body);
    (commands, total_height)
}

/// Lay the document's chrome into the rect the *window* gave it — the
/// titlebar strip, whose height is the window's to decide, not the page's.
/// Returns the commands, hit regions included, in that rect's coordinates.
///
/// Separate from [`layout_document`] because it is a different contract: no
/// page background, a definite height nothing may exceed, and no scrolling.
#[allow(clippy::too_many_arguments)]
pub fn layout_chrome(
    tree: &UiTree,
    rect: Rect,
    measurer: &mut dyn TextMeasurer,
    images: &mut dyn ImageSizer,
    state: &[ActionValue],
    // Focus state, so a text input living in the chrome — a location field —
    // renders its caret and selection exactly like one in the page.
    focused: Option<u16>,
    caret: usize,
    selection: (usize, usize),
    cursor: Option<(f32, f32)>,
    pressing: bool,
) -> Vec<DrawCommand> {
    let Some(chrome) = &tree.chrome else { return Vec::new() };
    let mut out = Vec::new();
    let mut groups = std::collections::HashMap::new();
    measure_groups(chrome, measurer, &mut groups);
    layout_node(
        chrome,
        &mut Ctx {
            backdrops: 0,
            groups,
            measurer,
            images,
            state,
            focused,
            caret,
            selection,
            cursor,
            pressing,
            // Chrome never scrolls — it is the strip the window lent.
            scroll_offsets: &[],
            scroll_seen: 0,
        },
        rect,
        Some(rect.h),
        &mut out,
    );
    out
}

/// The style a leaf paints with, if it is a measurable leaf.
fn leaf_style(node: &ResolvedNode) -> Option<&ResolvedStyle> {
    match node {
        ResolvedNode::Text { style, .. }
        | ResolvedNode::Link { style, .. }
        | ResolvedNode::Icon { style, .. }
        | ResolvedNode::Button { style, .. } => Some(style),
        _ => None,
    }
}

/// Pre-pass for measure groups: every element sharing a `group` is laid out
/// at the width of the group's widest member — table columns sized by
/// content. One walk, measuring only grouped leaves at unbounded width.
fn measure_groups(
    node: &ResolvedNode,
    measurer: &mut dyn TextMeasurer,
    out: &mut std::collections::HashMap<String, f32>,
) {
    if let Some(style) = leaf_style(node)
        && let Some(group) = &style.measure_group
    {
        let w = match node {
            ResolvedNode::Text { style, value } => {
                measurer
                    .measure(value, style.font_size, style.font_weight, &style.font_family, f32::MAX)
                    .width
            }
            ResolvedNode::Link { style, label, .. } | ResolvedNode::Button { style, label, .. } => {
                measurer
                    .measure(label, style.font_size, style.font_weight, &style.font_family, f32::MAX)
                    .width
            }
            ResolvedNode::Icon { style, size, .. } => match size {
                Dimension::Px(v) => *v,
                _ => style.font_size,
            },
            _ => 0.0,
        };
        let entry = out.entry(group.clone()).or_insert(0.0);
        *entry = entry.max(w);
    }
    match node {
        ResolvedNode::Row { children, .. } | ResolvedNode::Column { children, .. } => {
            for c in children {
                measure_groups(c, measurer, out);
            }
        }
        ResolvedNode::Scroll { child, .. }
        | ResolvedNode::When { child, .. }
        | ResolvedNode::Chrome { child, .. } => measure_groups(child, measurer, out),
        _ => {}
    }
}

struct Ctx<'a> {
    /// Backdrops emitted so far. The wire caps them per frame because each
    /// one costs the compositor a blur chain; a document that asks for more
    /// would otherwise build a frame that cannot be encoded, which is a much
    /// worse failure than a panel that is merely not frosted.
    backdrops: usize,
    /// Widths of measure groups (widest member), from the pre-pass.
    groups: std::collections::HashMap<String, f32>,
    measurer: &'a mut dyn TextMeasurer,
    images: &'a mut dyn ImageSizer,
    state: &'a [ActionValue],
    focused: Option<u16>,
    caret: usize,
    selection: (usize, usize),
    cursor: Option<(f32, f32)>,
    pressing: bool,
    /// Per-region scroll offsets, in document order of the Scroll nodes;
    /// `scroll_seen` is the walk's index into it. Empty means every region
    /// sits at its top, which is also what every caller without scrolling
    /// gets.
    scroll_offsets: &'a [f32],
    scroll_seen: usize,
}

impl Ctx<'_> {
    /// Is the cursor within `rect` right now?
    fn cursor_in(&self, rect: &Rect) -> bool {
        matches!(self.cursor, Some((x, y))
            if x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h)
    }
}

const IMAGE_PLACEHOLDER: (f32, f32) = (200.0, 150.0);

/// The text a leaf actually draws. Normally the string itself; with
/// `ellipsis` set, the longest prefix that fits on one line plus `…`.
///
/// Measured against an unbounded width first, because the measurer wraps —
/// asking it whether the text fits inside `avail` would only ever tell us
/// how tall the wrap became, never that it overran.
fn fitted(
    text: &str,
    style: &ResolvedStyle,
    avail: f32,
    ctx: &mut Ctx,
) -> Option<String> {
    if !style.ellipsis || avail <= 0.0 {
        return None;
    }
    let width = |s: &str, ctx: &mut Ctx| {
        ctx.measurer.measure(s, style.font_size, style.font_weight, &style.font_family, f32::MAX)
            .width
    };
    if width(text, ctx) <= avail {
        return None;
    }
    // Longest prefix that still fits with the ellipsis appended. Binary search
    // over char boundaries — byte indices would split multi-byte characters.
    let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let (mut lo, mut hi) = (0usize, bounds.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate = format!("{}\u{2026}", &text[..bounds[mid.min(bounds.len() - 1)]]);
        if width(&candidate, ctx) <= avail { lo = mid } else { hi = mid - 1 }
    }
    Some(format!("{}\u{2026}", &text[..bounds[lo.min(bounds.len() - 1)]]))
}

/// Where a leaf narrower than its slot should sit. Alignment is resolved into
/// an x-offset here, rather than carried to paint time: the backends already
/// know how to draw at a position, and a measured offset keeps every host
/// identical by construction.
///
/// Every leaf that sizes to its own content goes through this, so that a
/// centred tile — icon over label — lines up without the app doing arithmetic.
fn align_x(style: &ResolvedStyle, frame: &Rect, used_w: f32) -> f32 {
    let slack = (frame.w - used_w).max(0.0);
    frame.x + slack * style.align.unwrap_or_default().leading_fraction()
}

/// Whether this child is the one a column should hand its leftover height to:
/// a container that asked to fill, or a spacer, whose whole job is to be the
/// slack. First match wins — two of them would need weights, and nothing has
/// asked for that.
fn absorbs_column_slack(node: &ResolvedNode) -> bool {
    matches!(container_height(node), Some(Dimension::Fill(_)))
        || matches!(node, ResolvedNode::Spacer { size: Dimension::Auto | Dimension::Fill(_) })
        // A scroll region is the natural slack-taker: it exists to fit
        // however much room there is and scroll the rest.
        || matches!(node, ResolvedNode::Scroll { .. })
}

/// The explicit height a container's style asked for, if any. Only containers
/// carry one: leaves size to their content.
fn container_height(node: &ResolvedNode) -> Option<Dimension> {
    match node {
        ResolvedNode::Row { style, .. }
        | ResolvedNode::Column { style, .. }
        | ResolvedNode::Scroll { style, .. } => style.height,
        _ => None,
    }
}

/// Lay out one node at `frame.x/y` with `frame.w` available width and an
/// optional definite height. Returns (width, height) actually used.
fn layout_node(
    node: &ResolvedNode,
    ctx: &mut Ctx,
    frame: Rect,
    avail_h: Option<f32>,
    out: &mut Vec<DrawCommand>,
) -> (f32, f32) {
    match node {
        ResolvedNode::Text { style, value } => {
            let clipped = fitted(value, style, frame.w, ctx);
            let value = clipped.as_ref().unwrap_or(value);
            let m = ctx.measurer.measure(
                value, style.font_size, style.font_weight, &style.font_family, frame.w,
            );
            let used_w = m.width.min(frame.w);
            // The wrap box stays the full width — only the origin moves — so a
            // wrapped paragraph still wraps in the same place.
            let x = align_x(style, &frame, used_w);
            background(ctx, style, x, frame.y, used_w, m.height, out);
            out.push(DrawCommand::Text {
                rect: Rect { x, y: frame.y, w: frame.w, h: m.height },
                text: value.clone(),
                color: style.color,
                font_size: style.font_size,
                font_weight: style.font_weight,
                font_family: style.font_family.clone(),
            });
            // An underline a style asked for by name. Only explicit asks:
            // the resolved default (`None`) belongs to links.
            if style.underline == Some(true) {
                out.push(DrawCommand::Rect {
                    rect: Rect { x, y: frame.y + m.height - 1.0, w: used_w, h: 1.0 },
                    color: style.color,
                    corner_radius: 0.0,
                });
            }
            // Intrinsic width (for row auto-sizing); the wrap box stays frame.w.
            (used_w, m.height)
        }
        ResolvedNode::Link { style, label, target } => {
            let clipped = fitted(label, style, frame.w, ctx);
            let label = clipped.as_ref().unwrap_or(label);
            let m = ctx.measurer.measure(
                label, style.font_size, style.font_weight, &style.font_family, frame.w,
            );
            let used_w = m.width.min(frame.w);
            let rect =
                Rect { x: align_x(style, &frame, used_w), y: frame.y, w: used_w, h: m.height };
            background(ctx, style, rect.x, rect.y, rect.w, rect.h, out);
            out.push(DrawCommand::Text {
                rect,
                text: label.clone(),
                color: style.color,
                font_size: style.font_size,
                font_weight: style.font_weight,
                font_family: style.font_family.clone(),
            });
            // Underline only a bare link, and only if the style still wants
            // one. A link with a background is acting as a button — the fill
            // already signals it's clickable — and a link acting as a list row
            // wants to look like a row, not like prose.
            if style.background.is_none() && style.underline.unwrap_or(true) {
                out.push(DrawCommand::Rect {
                    rect: Rect { x: rect.x, y: rect.y + rect.h - 1.0, w: rect.w, h: 1.0 },
                    color: style.color,
                    corner_radius: 0.0,
                });
            }
            out.push(DrawCommand::LinkArea { rect, target: target.clone() });
            (rect.w, rect.h)
        }
        ResolvedNode::Icon { style, name, size } => {
            // An icon is geometry, not a font glyph: the named strokes are
            // placed and scaled here, and become the same capsule Paths a
            // chart line uses. Nothing new reaches the renderer.
            let size = match size {
                Dimension::Px(v) => *v,
                // Unsized icons sit on the text they label.
                _ => style.font_size,
            };
            if let Some(icon) = crate::icons::icon(name) {
                let x = align_x(style, &frame, size);
                // An icon honours a style background like text does — its
                // bounds are a box a style may paint (chips, trace modes).
                background(ctx, style, x, frame.y, size, size, out);
                let (points, contours) = icon.at(x, frame.y, size);
                out.push(DrawCommand::FillPath { points, contours, color: style.color });
            }
            // An unknown name occupies its space silently rather than
            // collapsing the row it sits in — a missing glyph should not
            // reflow the page around it.
            (size, size)
        }
        ResolvedNode::Image { style, source } => {
            // Natural size when loaded, scaled down to fit; placeholder box
            // until then. A style may size the box instead — `width`,
            // `height`, or both — which is what makes a thumbnail grid
            // expressible: without it the only way to get a small picture was
            // to build a narrow frame around it.
            let (nat_w, nat_h) = ctx
                .images
                .natural_size(source)
                .filter(|(w, h)| *w > 0.0 && *h > 0.0)
                .unwrap_or(IMAGE_PLACEHOLDER);
            let px = |d: &Option<rill_doc::Dimension>, avail: f32| match d {
                Some(rill_doc::Dimension::Px(v)) => Some(v.max(1.0).min(avail)),
                Some(rill_doc::Dimension::Fill(_)) => Some(avail),
                Some(rill_doc::Dimension::Auto) | None => None,
            };
            // The box: declared axes win, an undeclared axis follows the
            // picture's aspect, and no declaration at all is the old rule.
            // Height has no natural budget to clamp against, so Fill height
            // falls back to aspect rather than inventing one.
            let box_w = px(&style.width, frame.w);
            let box_h = match &style.height {
                Some(rill_doc::Dimension::Px(v)) => Some(v.max(1.0)),
                _ => None,
            };
            let (w, h) = match (box_w, box_h) {
                (Some(w), Some(h)) => (w, h),
                (Some(w), None) => (w, nat_h * (w / nat_w)),
                (None, Some(h)) => {
                    let w = (nat_w * (h / nat_h)).min(frame.w);
                    (w, h)
                }
                (None, None) => {
                    let w = nat_w.min(frame.w);
                    (w, nat_h * (w / nat_w))
                }
            };
            background(ctx, style, frame.x, frame.y, w, h, out);
            // The picture goes inside the box at its own shape — contained,
            // centred — because a 4:3 photograph in a square thumbnail slot
            // should letterbox, not squash. The style's background is the
            // mat. When only one axis (or neither) was declared the box
            // already has the picture's aspect and this is the whole box.
            let s = (w / nat_w).min(h / nat_h);
            let (dw, dh) = (nat_w * s, nat_h * s);
            out.push(DrawCommand::Image {
                rect: Rect {
                    x: frame.x + (w - dw) / 2.0,
                    y: frame.y + (h - dh) / 2.0,
                    w: dw,
                    h: dh,
                },
                source: source.clone(),
            });
            (w, h)
        }
        ResolvedNode::Rectangle { style, width, height } => {
            let w = match width {
                Dimension::Px(v) => v.min(frame.w),
                Dimension::Auto => frame.w,
                Dimension::Fill(_) => frame.w,
            };
            let h = match height {
                Dimension::Px(v) => *v,
                Dimension::Auto | Dimension::Fill(_) => 0.0,
            };
            let color = style.background.unwrap_or(style.color);
            out.push(DrawCommand::Rect {
                rect: Rect { x: frame.x, y: frame.y, w, h },
                color,
                corner_radius: style.corner_radius,
            });
            (w, h)
        }
        ResolvedNode::Spacer { size } => match size {
            Dimension::Px(v) => (*v, *v),
            // Width is resolved by the row pass, which never hands a spacer a
            // definite height. A column does, and only to the one child it
            // picked to absorb the slack — so a height here means "you are
            // that child". Without it a spacer would push things apart in a
            // row and collapse in a column.
            Dimension::Auto | Dimension::Fill(_) => (0.0, avail_h.unwrap_or(0.0)),
        },
        ResolvedNode::Column { style, gap, padding, target, children } => {
            let pad = style.padding.unwrap_or_else(|| dim_px(*padding));
            let (pad_x, pad_y) = (style.padding_x.unwrap_or(pad), style.padding_y.unwrap_or(pad));
            let gap = style.gap.unwrap_or_else(|| dim_px(*gap));
            let inner_x = frame.x + pad_x;
            let inner_w = (frame.w - 2.0 * pad_x).max(0.0);

            // Background painted after children are measured (needs height),
            // but must come first in paint order: reserve a slot.
            let bg_slot = out.len();

            // A child asking to fill takes whatever the others leave. That
            // needs the others' heights first, so measure them into a scratch
            // buffer that is thrown away — the alternative is threading an
            // intrinsic-height pass through every node type, and a column
            // containing a fill child is rare enough to pay for a second
            // measure instead. Only the first such child fills; a column with
            // two of them would need weights, which nothing has asked for.
            let mut forced: Option<(usize, f32)> = None;
            if let Some(definite) = avail_h
                && let Some(idx) = children.iter().position(absorbs_column_slack)
            {
                let mut scratch = Vec::new();
                let mut used = 0.0;
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        used += gap;
                    }
                    if i == idx {
                        continue;
                    }
                    let (_, h) = layout_node(
                        child,
                        ctx,
                        Rect { x: inner_x, y: 0.0, w: inner_w, h: 0.0 },
                        None,
                        &mut scratch,
                    );
                    used += h;
                }
                forced = Some((idx, (definite - 2.0 * pad_y - used).max(0.0)));
            }

            let mut cursor = frame.y + pad_y;
            let mut first = true;
            for (i, child) in children.iter().enumerate() {
                let gap_added = !first;
                if gap_added {
                    cursor += gap;
                }
                let child_h = match forced {
                    Some((idx, h)) if idx == i => Some(h),
                    _ => None,
                };
                let (_, h) = layout_node(
                    child,
                    ctx,
                    Rect { x: inner_x, y: cursor, w: inner_w, h: 0.0 },
                    child_h,
                    out,
                );
                // A child that rendered nothing — a closed `when`, a
                // collapsed spacer — is geometrically absent: it costs no
                // gap and does not end the "first child" state.
                if h == 0.0 {
                    if gap_added {
                        cursor -= gap;
                    }
                } else {
                    first = false;
                }
                cursor += h;
            }
            let mut height = cursor + pad_y - frame.y;
            if let Some(definite) = avail_h {
                height = height.max(definite);
            }
            let painted = hovered(ctx, style, frame.x, frame.y, frame.w, height);
            insert_background(ctx, painted, frame.x, frame.y, frame.w, height, bg_slot, out);
            if let Some(target) = target {
                // The whole container is the click target; interactive
                // children still win, because hit-testing is document order
                // and they were emitted first.
                out.push(DrawCommand::LinkArea {
                    rect: Rect { x: frame.x, y: frame.y, w: frame.w, h: height },
                    target: target.clone(),
                });
            }
            for child in children.iter() {
                if let ResolvedNode::Menu { items } = child {
                    out.push(DrawCommand::MenuArea {
                        rect: Rect { x: frame.x, y: frame.y, w: frame.w, h: height },
                        items: items.clone(),
                    });
                }
            }
            (frame.w, height)
        }
        ResolvedNode::Row { style, gap, padding, target, children } => {
            let pad = style.padding.unwrap_or_else(|| dim_px(*padding));
            let (pad_x, pad_y) = (style.padding_x.unwrap_or(pad), style.padding_y.unwrap_or(pad));
            let gap = style.gap.unwrap_or_else(|| dim_px(*gap));
            let inner_x = frame.x + pad_x;
            let inner_w = (frame.w - 2.0 * pad_x).max(0.0);

            // A wrapping row is a grid: children keep their own width and
            // start a new line when the next one will not fit. Distinct from
            // an ordinary row, which shares one line out among its children —
            // a grid tile must not stretch to fill, or a folder of three
            // items would show three enormous ones.
            if style.wrap {
                let bg_slot = out.len();
                let (mut x, mut y) = (inner_x, frame.y + pad_y);
                let mut line_h: f32 = 0.0;
                let mut first_on_line = true;
                for child in children {
                    let w = wrap_child_width(child, ctx, inner_w);
                    if !first_on_line && x + gap + w > inner_x + inner_w + 0.5 {
                        x = inner_x;
                        y += line_h + gap;
                        line_h = 0.0;
                        first_on_line = true;
                    }
                    if !first_on_line {
                        x += gap;
                    }
                    first_on_line = false;
                    let (_, h) = layout_node(
                        child,
                        ctx,
                        Rect { x, y, w, h: 0.0 },
                        None,
                        out,
                    );
                    line_h = line_h.max(h);
                    x += w;
                }
                let mut height = (y + line_h + pad_y) - frame.y;
                if let Some(definite) = avail_h {
                    height = height.max(definite);
                }
                let painted = hovered(ctx, style, frame.x, frame.y, frame.w, height);
                insert_background(ctx, painted, frame.x, frame.y, frame.w, height, bg_slot, out);
                if let Some(target) = target {
                    out.push(DrawCommand::LinkArea {
                        rect: Rect { x: frame.x, y: frame.y, w: frame.w, h: height },
                        target: target.clone(),
                    });
                }
                for child in children.iter() {
                    if let ResolvedNode::Menu { items } = child {
                        out.push(DrawCommand::MenuArea {
                            rect: Rect { x: frame.x, y: frame.y, w: frame.w, h: height },
                            items: items.clone(),
                        });
                    }
                }
                return (frame.w, height);
            }

            let gaps_total = gap * (children.len().saturating_sub(1)) as f32;

            // Pass 1: px-width children and measurement of auto children.
            #[derive(Clone, Copy)]
            enum Slot {
                Fixed(f32),
                Fill(f32),
            }
            let mut slots = Vec::with_capacity(children.len());
            let mut fixed_total = 0.0;
            let mut fill_total = 0.0;
            for child in children {
                let slot = match child {
                    ResolvedNode::Spacer { size: Dimension::Px(v) } => Slot::Fixed(*v),
                    ResolvedNode::Spacer { size: Dimension::Auto } => Slot::Fill(1.0),
                    ResolvedNode::Spacer { size: Dimension::Fill(w) } => Slot::Fill(*w),
                    ResolvedNode::Rectangle { width: Dimension::Px(v), .. } => Slot::Fixed(*v),
                    ResolvedNode::Rectangle { width: Dimension::Fill(w), .. } => Slot::Fill(*w),
                    ResolvedNode::Rectangle { width: Dimension::Auto, .. } => Slot::Fixed(0.0),
                    // Containers have no bounded intrinsic width, so they flex
                    // by default — but a style may pin one, which is what lets
                    // a row hold a fixed sidebar beside a filling pane.
                    ResolvedNode::Row { style, .. }
                    | ResolvedNode::Column { style, .. }
                    | ResolvedNode::Scroll { style, .. }
                    | ResolvedNode::TextInput { style, .. } => match style.width {
                        Some(Dimension::Px(v)) => Slot::Fixed(v.max(0.0)),
                        Some(Dimension::Fill(w)) => Slot::Fill(w.max(0.0)),
                        _ => Slot::Fill(1.0),
                    },
                    ResolvedNode::Button { style, .. }
                        if matches!(style.width, Some(Dimension::Fill(_))) =>
                    {
                        let Some(Dimension::Fill(f)) = style.width else { unreachable!() };
                        Slot::Fill(f.max(0.0))
                    }
                    leaf => {
                        // Leaf intrinsic width, measured against remaining space.
                        // A grouped leaf takes its group's width instead: the
                        // widest member sizes the whole column.
                        let remaining =
                            (inner_w - fixed_total - gaps_total).max(0.0);
                        let group_w = leaf_style(leaf)
                            .and_then(|st| st.measure_group.as_ref())
                            .and_then(|g| ctx.groups.get(g))
                            .copied();
                        let w = match group_w {
                            Some(w) => w,
                            None => {
                                let mut probe = Vec::new();
                                layout_node(
                                    leaf,
                                    ctx,
                                    Rect { x: 0.0, y: 0.0, w: remaining, h: 0.0 },
                                    None,
                                    &mut probe,
                                )
                                .0
                            }
                        };
                        Slot::Fixed(w.min(remaining))
                    }
                };
                match slot {
                    Slot::Fixed(w) => fixed_total += w,
                    Slot::Fill(f) => fill_total += f,
                }
                slots.push(slot);
            }
            let leftover = (inner_w - fixed_total - gaps_total).max(0.0);
            let width_of = |slot: &Slot| match slot {
                Slot::Fixed(w) => *w,
                Slot::Fill(f) if fill_total > 0.0 => leftover * f / fill_total,
                Slot::Fill(_) => 0.0,
            };

            // A row that centres (or bottoms) its children has to know how
            // tall the tallest one is before it can place any of them, so it
            // measures once into a scratch buffer first. Paid only when the
            // style asks: the default is still one pass, top-aligned.
            //
            // The offset must be known *before* the real layout rather than
            // applied after it — a child computes its own hover and hit
            // regions as it lays out, and shifting the commands afterwards
            // would leave both a few pixels above the pixels they describe.
            let measured = (style.valign != Align::Left).then(|| {
                let mut scratch = Vec::new();
                let heights: Vec<f32> = children
                    .iter()
                    .zip(&slots)
                    .map(|(child, slot)| {
                        layout_node(
                            child,
                            ctx,
                            Rect { x: 0.0, y: 0.0, w: width_of(slot), h: 0.0 },
                            None,
                            &mut scratch,
                        )
                        .1
                    })
                    .collect();
                let tallest = heights.iter().copied().fold(0.0f32, f32::max);
                (tallest, heights)
            });

            // Pass 2: place children left→right.
            let bg_slot = out.len();
            let mut cursor = inner_x;
            let mut tallest: f32 = 0.0;
            let mut first = true;
            for (i, (child, slot)) in children.iter().zip(&slots).enumerate() {
                let gap_added = !first;
                if gap_added {
                    cursor += gap;
                }
                let w = width_of(slot);
                // A row is as tall as it was given, when it was given a
                // height — so a child asking to fill has something definite to
                // fill *to*. Without this a sidebar column inside a row could
                // never learn how tall the row is, and stopped at its content.
                let child_h = avail_h
                    .map(|definite| (definite - 2.0 * pad_y).max(0.0))
                    .filter(|_| {
                        matches!(container_height(child), Some(Dimension::Fill(_)))
                            // A scroll region without a bound is a
                            // contradiction — it exists to bound content —
                            // so it takes the height the row was given
                            // without having to say `height=fill`.
                            || matches!(child, ResolvedNode::Scroll { .. })
                    });
                // A filling container is as tall as the row, so it never needs
                // shifting; anything else drops by its share of the slack.
                let shift = match (&measured, child_h) {
                    (Some((row, heights)), None) => {
                        // Centre against the row's *own* height when it has
                        // one — a band filling a 44px strip centres in the
                        // strip, not against its tallest child, or the group
                        // hugs the top and the slack all lands below.
                        let span = avail_h.map(|d| (d - 2.0 * pad_y).max(*row)).unwrap_or(*row);
                        (span - heights[i]).max(0.0) * style.valign.leading_fraction()
                    }
                    _ => 0.0,
                };
                let (_, h) = layout_node(
                    child,
                    ctx,
                    Rect { x: cursor, y: frame.y + pad_y + shift, w, h: 0.0 },
                    child_h,
                    out,
                );
                if w == 0.0 && h == 0.0 {
                    // Geometrically absent (see the column loop).
                    if gap_added {
                        cursor -= gap;
                    }
                } else {
                    first = false;
                }
                tallest = tallest.max(h + shift);
                cursor += w;
            }
            let mut height = tallest + 2.0 * pad_y;
            if let Some(definite) = avail_h {
                height = height.max(definite);
            }
            let painted = hovered(ctx, style, frame.x, frame.y, frame.w, height);
            insert_background(ctx, painted, frame.x, frame.y, frame.w, height, bg_slot, out);
            if let Some(target) = target {
                out.push(DrawCommand::LinkArea {
                    rect: Rect { x: frame.x, y: frame.y, w: frame.w, h: height },
                    target: target.clone(),
                });
            }
            for child in children.iter() {
                if let ResolvedNode::Menu { items } = child {
                    out.push(DrawCommand::MenuArea {
                        rect: Rect { x: frame.x, y: frame.y, w: frame.w, h: height },
                        items: items.clone(),
                    });
                }
            }
            (frame.w, height)
        }
        // Chrome is lifted out of the tree by `resolve`. One reaching here is
        // a second chrome node in the same document; the window has only one
        // strip to lend, so it draws nothing.
        ResolvedNode::Chrome { .. } => (0.0, 0.0),
        // A menu is metadata on its container; the container emits the area.
        ResolvedNode::Menu { .. } => (0.0, 0.0),
        ResolvedNode::Keys { target } => {
            out.push(DrawCommand::KeyCapture { target: target.clone() });
            (0.0, 0.0)
        }
        ResolvedNode::Live { target, interval } => {
            out.push(DrawCommand::LiveRefresh { target: target.clone(), interval: *interval });
            (0.0, 0.0)
        }
        ResolvedNode::Key { combo, target, action } => {
            out.push(DrawCommand::KeyBind {
                key: combo.clone(),
                target: target.clone(),
                action: action.clone(),
            });
            (0.0, 0.0)
        }
        ResolvedNode::When { state, invert, child } => {
            let on = match ctx.state.get(*state as usize) {
                Some(ActionValue::Bool(b)) => *b != *invert,
                _ => false,
            };
            if on {
                layout_node(child, ctx, frame, avail_h, out)
            } else {
                (0.0, 0.0)
            }
        }
        ResolvedNode::Button { style, label, icon, action } => {
            // A button's padding is the style's to set, like any container's —
            // the constants are only the default for an unstyled button. The
            // horizontal is doubled because a button wants to read wider than
            // tall; a style that needs exact control can grow the label.
            let (pad_x, pad_y) = (
                style.padding_x.or(style.padding.map(|p| p * 2.0)).unwrap_or(14.0),
                style.padding_y.or(style.padding).unwrap_or(7.0),
            );
            let m = ctx.measurer.measure(
                label, style.font_size, style.font_weight, &style.font_family, frame.w,
            );
            // An icon sits on the label's line box; with no label it *is*
            // the content. Gap between the two only when both exist.
            let icon_size = if icon.is_some() { m.height } else { 0.0 };
            let icon_gap = if icon.is_some() && !label.is_empty() { 4.0 } else { 0.0 };
            let content_w = icon_size + icon_gap + if label.is_empty() { 0.0 } else { m.width };
            // A style may pin the width — an icon button wants to be square,
            // and a row of them wants one rhythm regardless of glyph width.
            // The content centres in a pinned box instead of hugging padding.
            let w = match style.width {
                Some(Dimension::Px(v)) => v.min(frame.w),
                // A filling button takes the slot the row dealt it — equal
                // controls sharing a bar say width="fill" and nothing else.
                Some(Dimension::Fill(_)) => frame.w,
                _ => (content_w + 2.0 * pad_x).min(frame.w),
            };
            let h = m.height + 2.0 * pad_y;
            let rect = Rect { x: align_x(style, &frame, w), y: frame.y, w, h };
            // A declared hover style owns the feedback wholesale, exactly
            // as it does for containers; geometry still comes from the
            // base, so the button cannot resize under the cursor.
            let painted = hovered(ctx, style, rect.x, rect.y, rect.w, rect.h);
            // Default button chrome when the style doesn't provide one.
            let (base_bg, label_color) = match painted.background {
                Some(bg) => (bg, painted.color),
                // Neutral on purpose: a default that smuggles a hue in
                // defeats any theme that chose not to have one.
                None => (
                    Color { r: 0x30, g: 0x30, b: 0x30, a: 0xFF },
                    Color { r: 0xF2, g: 0xF2, b: 0xF2, a: 0xFF },
                ),
            };
            // Shade-based hover/press feedback for buttons without a hover
            // style of their own — the behaviour every button had before.
            let bg = if style.hover.is_none() && ctx.cursor_in(&rect) {
                if ctx.pressing { shade(base_bg, -16) } else { shade(base_bg, 20) }
            } else {
                base_bg
            };
            out.push(DrawCommand::Rect {
                rect,
                color: bg,
                corner_radius: style.corner_radius,
            });
            // Content placement: centred by default (the button look), or
            // aligned when the style says so — a file-tree row is a
            // full-width button whose label hangs left.
            let content_x = match style.align {
                Some(a) => {
                    rect.x
                        + pad_x
                        + (w - content_w - 2.0 * pad_x).max(0.0) * a.leading_fraction()
                }
                None => rect.x + ((w - content_w) / 2.0).max(0.0),
            };
            if let Some(name) = icon
                && let Some(glyph) = crate::icons::icon(name)
            {
                let (points, contours) = glyph.at(content_x, rect.y + pad_y, icon_size);
                out.push(DrawCommand::FillPath { points, contours, color: label_color });
            }
            if !label.is_empty() {
                out.push(DrawCommand::Text {
                    rect: Rect {
                        x: content_x + icon_size + icon_gap,
                        y: rect.y + pad_y,
                        w: m.width.min(w),
                        h: m.height,
                    },
                    text: label.clone(),
                    color: label_color,
                    font_size: style.font_size,
                    font_weight: style.font_weight,
                    font_family: style.font_family.clone(),
                });
            }
            out.push(DrawCommand::ActionArea { rect, action: action.clone() });
            (w, h)
        }
        ResolvedNode::Slider { style, bind, min, max, step, on_release } => {
            // The control is font-sized so it sits naturally beside labels:
            // the thumb is a text line tall, the track a third of that.
            let thumb = style.font_size.max(8.0);
            let track_h = (thumb / 3.0).max(3.0);
            let w = match style.width {
                Some(Dimension::Px(px)) => px.min(frame.w),
                _ => frame.w,
            };
            let h = thumb + 4.0;
            let rect = Rect { x: align_x(style, &frame, w), y: frame.y, w, h };

            let value = match ctx.state.get(*bind as usize) {
                Some(ActionValue::Num(n)) => *n as f32,
                _ => *min,
            };
            let span = max - min;
            let fraction = ((value - min) / span).clamp(0.0, 1.0);
            // The thumb travels inside the rect, so the usable track is one
            // thumb narrower than the control.
            let travel = (w - thumb).max(0.0);
            let thumb_x = rect.x + fraction * travel;

            // Track wears the style's background, fill and thumb its colour —
            // neutral defaults, same argument as the button's.
            let track_color =
                style.background.unwrap_or(Color { r: 0x30, g: 0x30, b: 0x30, a: 0xFF });
            let active_color = style.color;
            let track = Rect {
                x: rect.x,
                y: rect.y + (h - track_h) / 2.0,
                w,
                h: track_h,
            };
            out.push(DrawCommand::Rect {
                rect: track,
                color: track_color,
                corner_radius: track_h / 2.0,
            });
            if fraction > 0.0 {
                out.push(DrawCommand::Rect {
                    rect: Rect { w: (thumb_x - rect.x) + thumb / 2.0, ..track },
                    color: active_color,
                    corner_radius: track_h / 2.0,
                });
            }
            let thumb_rect =
                Rect { x: thumb_x, y: rect.y + (h - thumb) / 2.0, w: thumb, h: thumb };
            // Hover/press feedback matches the button's: lighten, then darken.
            let thumb_color = if ctx.cursor_in(&rect) {
                if ctx.pressing { shade(active_color, -16) } else { shade(active_color, 20) }
            } else {
                active_color
            };
            if ctx.focused == Some(*bind) {
                out.push(DrawCommand::Glow {
                    rect: thumb_rect,
                    color: thumb_color,
                    blur: 4.0,
                    corner_radius: thumb / 2.0,
                });
            }
            out.push(DrawCommand::Rect {
                rect: thumb_rect,
                color: thumb_color,
                corner_radius: thumb / 2.0,
            });
            out.push(DrawCommand::SliderArea {
                rect,
                state: *bind,
                min: *min,
                max: *max,
                step: *step,
                on_release: on_release.clone(),
            });
            (w, h)
        }
        ResolvedNode::Code { style, bind, lang, class_colors, gutter, ws } => {
            // The editable code surface: the bound state as a highlighted
            // mono grid — gutter, indent dots, caret — one mode. Behaviour
            // is the multiline input's, wholesale: this arm only *renders*
            // differently, and emits the same InputArea with the gutter
            // folded into pad_x, so the existing click-to-caret and key
            // machinery need never know the text has colours.
            const PAD_X: f32 = 8.0;
            const PAD_Y: f32 = 6.0;
            let current = match ctx.state.get(*bind as usize) {
                Some(ActionValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            // The grid: mono metrics whatever the style says — a code
            // surface that is not monospace is a contradiction in terms.
            let m0 = ctx.measurer.measure("0", style.font_size, style.font_weight, "mono", f32::MAX);
            let (cell, line_h) = (m0.width.max(1.0), m0.height);
            let lines: Vec<&str> = if current.is_empty() {
                vec![""]
            } else {
                current.split('\n').collect()
            };
            let digits = lines.len().to_string().len().max(3);
            let gutter_w = (digits as f32 + 2.0) * cell;
            let h = lines.len() as f32 * line_h + 2.0 * PAD_Y;
            let rect = Rect { x: frame.x, y: frame.y, w: frame.w, h };
            if let Some(bg) = style.background {
                out.push(DrawCommand::Rect { rect, color: bg, corner_radius: style.corner_radius });
            }
            let focused = ctx.focused == Some(*bind);
            let text_x = rect.x + gutter_w + PAD_X;
            let lang = crate::code::lang_of(&format!("f.{lang}"));

            // Selection first, behind the glyphs.
            if focused {
                let (lo, hi) =
                    (ctx.selection.0.min(current.len()), ctx.selection.1.min(current.len()));
                if lo < hi {
                    let mut ls = 0usize;
                    for (li, line) in lines.iter().enumerate() {
                        let le = ls + line.len();
                        let (a, b) = (lo.max(ls), hi.min(le));
                        if a < b {
                            let x0 = ctx
                                .measurer
                                .measure(&line[..a - ls], style.font_size, style.font_weight, "mono", f32::MAX)
                                .width;
                            let x1 = ctx
                                .measurer
                                .measure(&line[..b - ls], style.font_size, style.font_weight, "mono", f32::MAX)
                                .width;
                            out.push(DrawCommand::Rect {
                                rect: Rect {
                                    x: text_x + x0,
                                    y: rect.y + PAD_Y + li as f32 * line_h,
                                    w: (x1 - x0).max(1.0),
                                    h: line_h,
                                },
                                color: Color { r: 0x4A, g: 0x6A, b: 0xDA, a: 0x55 },
                                corner_radius: 0.0,
                            });
                        }
                        ls = le + 1;
                    }
                }
            }

            // What a control byte looks like when a file carries one: the
            // escape as ␛, tabs as the arrow, the rest as a dot. One char
            // per char, so the grid arithmetic never learns they were odd.
            fn visible(t: &str) -> String {
                t.chars()
                    .map(|c| match c {
                        '\t' => '⇥',
                        '\x1b' => '␛',
                        c if (c as u32) < 0x20 => '·',
                        c => c,
                    })
                    .collect()
            }
            let mut lex = crate::code::LineState::default();
            for (li, line) in lines.iter().enumerate() {
                let y = rect.y + PAD_Y + li as f32 * line_h;
                let mut text = |x: f32, t: String, color: Color| {
                    out.push(DrawCommand::Text {
                        // Wider than any line: the renderer wraps text at
                        // its rect, and a wrapped line here paints over the
                        // line below — the grid owns the line breaks, so
                        // the renderer must never invent one. Overflow is
                        // clipped by the pane's scroll region; a horizontal
                        // scroll is the recorded gap.
                        rect: Rect { x, y, w: 1_000_000.0, h: line_h },
                        text: t,
                        color,
                        font_size: style.font_size,
                        font_weight: style.font_weight,
                        font_family: "mono".into(),
                    });
                };
                text(rect.x + PAD_X, format!("{:>digits$}", li + 1), *gutter);
                // Leading whitespace as countable dots on its own cells.
                let indent_end = line.len() - line.trim_start().len();
                if indent_end > 0 {
                    let dots: String = line[..indent_end]
                        .chars()
                        .map(|c| if c == '\t' { '⇥' } else { '·' })
                        .collect();
                    text(text_x, dots, *ws);
                }
                let rest = &line[indent_end..];
                if !rest.is_empty() {
                    let rest_x = text_x + indent_end as f32 * cell;
                    match lang {
                        None => text(rest_x, visible(rest), class_colors[0]),
                        Some(lang) => {
                            let mut x = rest_x;
                            for (class, range) in crate::code::spans(rest, lang, &mut lex) {
                                let slice = visible(&rest[range]);
                                let slice = slice.as_str();
                                let w = ctx
                                    .measurer
                                    .measure(slice, style.font_size, style.font_weight, "mono", f32::MAX)
                                    .width;
                                let color = class_colors[match class {
                                    crate::code::Class::Plain => 0,
                                    crate::code::Class::Comment => 1,
                                    crate::code::Class::String => 2,
                                    crate::code::Class::Number => 3,
                                    crate::code::Class::Keyword => 4,
                                }];
                                text(x, slice.to_string(), color);
                                x += w;
                            }
                        }
                    }
                }
            }

            // The caret, on its cell.
            if focused {
                let caret = ctx.caret.min(current.len());
                let line_start = current[..caret].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let line_no = current[..caret].bytes().filter(|&b| b == b'\n').count() as f32;
                let prefix = &current[line_start..caret];
                let cx = ctx
                    .measurer
                    .measure(prefix, style.font_size, style.font_weight, "mono", f32::MAX)
                    .width;
                out.push(DrawCommand::Rect {
                    rect: Rect {
                        x: text_x + cx,
                        y: rect.y + PAD_Y + line_no * line_h,
                        w: 2.0,
                        h: line_h,
                    },
                    color: style.color,
                    corner_radius: 0.0,
                });
            }

            out.push(DrawCommand::InputArea {
                rect,
                state: *bind,
                on_enter: None,
                multiline: true,
                tab_inserts: true,
                font_size: style.font_size,
                font_weight: style.font_weight,
                font_family: "mono".into(),
                pad_x: gutter_w + PAD_X,
                pad_y: PAD_Y,
            });
            (rect.w, h)
        }
        ResolvedNode::TextInput { style, bind, placeholder, on_enter, multiline } => {
            const PAD_X: f32 = 8.0;
            const PAD_Y: f32 = 6.0;
            let focused = ctx.focused == Some(*bind);
            let current = match ctx.state.get(*bind as usize) {
                Some(ActionValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            let inner_w = (frame.w - 2.0 * PAD_X).max(1.0);
            let line_h = ctx
                .measurer
                .measure("x", style.font_size, style.font_weight, &style.font_family, inner_w)
                .height;

            // Height: single line, or wrapped content (min 3 lines) if multiline.
            let content_h = if *multiline && !current.is_empty() {
                ctx.measurer
                    .measure(&current, style.font_size, style.font_weight, &style.font_family, inner_w)
                    .height
                    .max(line_h * 3.0)
            } else if *multiline {
                line_h * 3.0
            } else {
                line_h
            };
            let h = content_h + 2.0 * PAD_Y;
            let rect = Rect { x: frame.x, y: frame.y, w: frame.w, h };

            // Field chrome: the style's if it has one, otherwise a default.
            // The fill used to be hardcoded white, which on a dark page is a
            // glaring slab and reads as an unstyled web form — the one place
            // the theme could not reach.
            // Border only when the style asks or focus needs showing — an
            // unbordered field is a decision, not an absence (the old default
            // drew light grey around every input a style left border-less).
            let border = if focused {
                Some(Color { r: 0x4A, g: 0x6A, b: 0xDA, a: 0xFF })
            } else if style.border > 0.0 {
                Some(style.border_color)
            } else {
                None
            };
            let fill = style
                .background
                .unwrap_or(Color { r: 0xFF, g: 0xFF, b: 0xFF, a: 0xFF });
            let radius = style.corner_radius;
            if let Some(border) = border {
                out.push(DrawCommand::Rect { rect, color: border, corner_radius: radius });
            }
            out.push(DrawCommand::Rect {
                rect: rect.inset(if focused { 2.0 } else if style.border > 0.0 { 1.0 } else { 0.0 }),
                color: fill,
                corner_radius: (radius - 1.0).max(0.0),
            });

            // Selection highlight (behind the text), one rect per logical line
            // the selection covers.
            if focused {
                let (lo, hi) = (ctx.selection.0.min(current.len()), ctx.selection.1.min(current.len()));
                if lo < hi {
                    let mut ls = 0usize;
                    for (line_no, line) in current.split('\n').enumerate() {
                        let le = ls + line.len();
                        let (a, b) = (lo.max(ls), hi.min(le));
                        if a < b {
                            let m = &mut *ctx.measurer;
                            let w = |m: &mut dyn TextMeasurer, t: &str| {
                                m.measure(t, style.font_size, style.font_weight, &style.font_family, f32::MAX).width
                            };
                            let x0 = w(m, &line[..a - ls]);
                            let x1 = w(m, &line[..b - ls]);
                            out.push(DrawCommand::Rect {
                                rect: Rect {
                                    x: rect.x + PAD_X + x0,
                                    y: rect.y + PAD_Y + line_no as f32 * line_h,
                                    w: (x1 - x0).max(1.0),
                                    h: line_h,
                                },
                                color: Color { r: 0x4A, g: 0x6A, b: 0xDA, a: 0x55 },
                                corner_radius: 0.0,
                            });
                        }
                        ls = le + 1;
                    }
                }
            }

            let (text, color) = if current.is_empty() {
                (placeholder.clone(), Color { r: 0x9A, g: 0x9A, b: 0xB0, a: 0xFF })
            } else {
                (current.clone(), style.color)
            };
            if !text.is_empty() {
                out.push(DrawCommand::Text {
                    rect: Rect { x: rect.x + PAD_X, y: rect.y + PAD_Y, w: inner_w, h: content_h },
                    text,
                    color,
                    font_size: style.font_size,
                    font_weight: style.font_weight,
                    font_family: style.font_family.clone(),
                });
            }
            // Caret at the insertion point. The caret byte-offset is clamped to
            // the string and measured on its own logical line: x is the width of
            // the text from that line's start up to the caret, y is the number
            // of preceding newlines. (Visual wrapping of one long logical line
            // isn't accounted for — precise wrap positioning is future work.)
            if focused {
                let caret = ctx.caret.min(current.len());
                let line_start = current[..caret].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let line_no = current[..caret].bytes().filter(|&b| b == b'\n').count() as f32;
                let prefix = &current[line_start..caret];
                let prefix_w = if prefix.is_empty() {
                    0.0
                } else {
                    ctx.measurer
                        .measure(prefix, style.font_size, style.font_weight, &style.font_family, f32::MAX)
                        .width
                };
                let caret_x = rect.x + PAD_X + prefix_w + 1.0;
                let caret_y = rect.y + PAD_Y + line_no * line_h;
                out.push(DrawCommand::Rect {
                    rect: Rect {
                        // NOTE (pixels-vs-vectors, see rill-north-star): this
                        // `.round()` snaps to a whole *logical* pixel so the thin
                        // bar isn't anti-aliased across neighbours. It's a
                        // pragmatic 1× fix that bakes a device-pixel decision
                        // into the resolution-independent DrawCommand stream —
                        // wrong for HiDPI/fractional scaling and for remoting
                        // (the viewer's grid differs). The principled home for
                        // pixel-snapping is the local renderer, which knows the
                        // device pixel ratio. Revisit with HiDPI / the wgpu
                        // backend / command-stream remoting.
                        x: caret_x.min(rect.x + rect.w - 3.0).round(),
                        y: caret_y.round(),
                        w: 2.0,
                        h: line_h,
                    },
                    color: Color { r: 0x2A, g: 0x2A, b: 0x44, a: 0xFF },
                    corner_radius: 0.0,
                });
            }
            out.push(DrawCommand::InputArea {
                rect,
                state: *bind,
                on_enter: on_enter.clone(),
                multiline: *multiline,
                tab_inserts: false,
                font_size: style.font_size,
                font_weight: style.font_weight,
                font_family: style.font_family.clone(),
                pad_x: PAD_X,
                pad_y: PAD_Y,
            });
            (frame.w, h)
        }
        ResolvedNode::Scroll { style, child } => {
            match avail_h {
                Some(viewport_h) => {
                    // This region's own offset: content shifts up under the
                    // clip, so what is scrolled away paints (and hit-tests)
                    // outside the visible rect — the caller trims those hit
                    // rects, the clip trims the paint.
                    let offset = ctx
                        .scroll_offsets
                        .get(ctx.scroll_seen)
                        .copied()
                        .unwrap_or(0.0)
                        .max(0.0);
                    ctx.scroll_seen += 1;
                    background(ctx, style, frame.x, frame.y, frame.w, viewport_h, out);
                    let region = Rect { x: frame.x, y: frame.y, w: frame.w, h: viewport_h };
                    let marker = out.len();
                    out.push(DrawCommand::ScrollArea { rect: region, content: 0.0 });
                    out.push(DrawCommand::PushClip { rect: region, radius: 0.0 });
                    let shifted = Rect { y: frame.y - offset, ..frame };
                    let (_, content_h) = layout_node(child, ctx, shifted, None, out);
                    out.push(DrawCommand::PopClip);
                    // The marker learns the content height it announced —
                    // known only after the child laid out.
                    if let DrawCommand::ScrollArea { content, .. } = &mut out[marker] {
                        *content = content_h;
                    }
                    (frame.w, viewport_h)
                }
                // Unbounded flow: transparent container.
                None => layout_node(child, ctx, frame, None, out),
            }
        }
    }
}

/// Lighten (positive) or darken (negative) a color by `delta` per channel.
fn shade(c: Color, delta: i16) -> Color {
    let adj = |v: u8| (v as i16 + delta).clamp(0, 255) as u8;
    Color { r: adj(c.r), g: adj(c.g), b: adj(c.b), a: c.a }
}

fn dim_px(d: Dimension) -> f32 {
    match d {
        Dimension::Px(v) => v,
        Dimension::Auto | Dimension::Fill(_) => 0.0,
    }
}

/// Everything a styled box paints, in order: the shadow it casts, its fill,
/// then its outline. Kept in one place so a container and a leaf cannot drift
/// apart about what "styled" means.
/// How wide a child is when it is a grid tile. A grid keeps each child at
/// its own width rather than sharing the line out, so a container has to say
/// how wide it is — `Fill` has no meaning here and falls back to the whole
/// line, which looks wrong loudly rather than silently.
fn wrap_child_width(child: &ResolvedNode, ctx: &mut Ctx, line_w: f32) -> f32 {
    let styled = match child {
        ResolvedNode::Row { style, .. }
        | ResolvedNode::Column { style, .. }
        | ResolvedNode::Scroll { style, .. }
        | ResolvedNode::TextInput { style, .. } => style.width,
        _ => None,
    };
    match styled {
        Some(Dimension::Px(v)) => v.max(0.0).min(line_w),
        Some(_) | None => match child {
            ResolvedNode::Rectangle { width: Dimension::Px(v), .. } => (*v).min(line_w),
            ResolvedNode::Spacer { size: Dimension::Px(v), .. } => (*v).min(line_w),
            ResolvedNode::Row { .. }
            | ResolvedNode::Column { .. }
            | ResolvedNode::Scroll { .. }
            | ResolvedNode::TextInput { .. } => line_w,
            leaf => {
                let mut probe = Vec::new();
                let (w, _) = layout_node(
                    leaf,
                    ctx,
                    Rect { x: 0.0, y: 0.0, w: line_w, h: 0.0 },
                    None,
                    &mut probe,
                );
                w.min(line_w)
            }
        },
    }
}

/// The style a container should paint with: its hover variant while the
/// pointer is inside it, otherwise itself. Only the *painted* style swaps —
/// spacing and sizing come from the base, so a row cannot resize under the
/// cursor and chase it away.
fn hovered<'a>(
    ctx: &Ctx,
    style: &'a ResolvedStyle,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> &'a ResolvedStyle {
    match &style.hover {
        Some(variant) if ctx.cursor_in(&Rect { x, y, w, h }) => variant,
        _ => style,
    }
}

fn box_commands(
    ctx: &mut Ctx,
    style: &ResolvedStyle,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Vec<DrawCommand> {
    let rect = Rect { x, y, w, h };
    let mut out = Vec::new();
    if let Some(blur) = style.shadow.filter(|b| *b > 0.0) {
        // Offset down by a third of the blur: light comes from above, and a
        // shadow centred on its box reads as a glow instead of as depth.
        out.push(DrawCommand::Shadow {
            rect: Rect { y: y + blur / 3.0, ..rect },
            color: Color { r: 0, g: 0, b: 0, a: 0x55 },
            blur,
            spread: 0.0,
            corner_radius: style.corner_radius,
        });
    }
    // Frost sits under the fill: the fill is what tints it, and a panel with
    // an opaque background would hide it entirely.
    if let Some(blur) = style.backdrop.filter(|b| *b > 0.0)
        && ctx.backdrops < crate::stream::MAX_BACKDROPS
    {
        ctx.backdrops += 1;
        out.push(DrawCommand::Backdrop { rect, blur, corner_radius: style.corner_radius });
    }
    if let Some(color) = style.background {
        out.push(DrawCommand::Rect { rect, color, corner_radius: style.corner_radius });
    }
    if style.border > 0.0 {
        out.push(DrawCommand::Border {
            rect,
            color: style.border_color,
            width: style.border,
            corner_radius: style.corner_radius,
        });
    }
    out
}

fn background(
    ctx: &mut Ctx,
    style: &ResolvedStyle,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    out: &mut Vec<DrawCommand>,
) {
    let cmds = box_commands(ctx, style, x, y, w, h);
    out.extend(cmds);
}

/// Insert a container's background at `slot` (before its children's
/// commands) once its final height is known.
#[allow(clippy::too_many_arguments)] // a rect is four of them
fn insert_background(
    ctx: &mut Ctx,
    style: &ResolvedStyle,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    slot: usize,
    out: &mut Vec<DrawCommand>,
) {
    for (i, cmd) in box_commands(ctx, style, x, y, w, h).into_iter().enumerate() {
        out.insert(slot + i, cmd);
    }
}
