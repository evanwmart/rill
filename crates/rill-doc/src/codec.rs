//! `.rill` encode/decode (document-format.md §2–§6). Decode is strict: any
//! non-canonical byte sequence is rejected, including unsorted string
//! tables, torn nodes, non-tree references, and non-finite dimensions.

use crate::{MAX_MENU_ITEMS, MenuItem, 
    ActionValue, Align, Color, ColorRef, Dimension, DocAction, DocError, Document, HEADER_LEN,
    IGNORABLE_TYPE_START, MAGIC, MAX_DEPTH, MAX_DOC_SIZE, MAX_NODES, MIN_LIVE_INTERVAL_MS, NO_STYLE, Node,
    StateVar, Style,
    VERSION, err,
};
use rill_protocol::validate_path;

const MAX_SUBMIT_FIELDS: usize = 16;

fn read_color_ref(
    r: &mut Reader,
    what: &str,
    check_str: &impl Fn(u16, &str) -> Result<u16, DocError>,
) -> Result<ColorRef, DocError> {
    match r.u8()? {
        0 => Ok(ColorRef::Literal(r.color()?)),
        1 => Ok(ColorRef::Token(check_str(r.u16()?, what)?)),
        t => Err(err(format!("{what}: unknown color tag {t}"))),
    }
}

/// A style color is a 1-byte tag (0 = literal, 1 = token) then its payload:
/// 4 RGBA bytes for a literal, or a 2-byte token string index.
fn write_color_ref(out: &mut Vec<u8>, c: ColorRef) {
    match c {
        ColorRef::Literal(c) => {
            out.push(0);
            out.extend_from_slice(&[c.r, c.g, c.b, c.a]);
        }
        ColorRef::Token(idx) => {
            out.push(1);
            out.extend_from_slice(&idx.to_be_bytes());
        }
    }
}

fn write_value(out: &mut Vec<u8>, value: &ActionValue) -> Result<(), DocError> {
    match value {
        ActionValue::Str(v) => {
            // Same ceiling as an ACTION field, and for the same reason: a
            // state slot and the field it submits are two ends of one
            // value, so a document that could hold more than it could send
            // would only fail later, at the save.
            if v.len() > rill_protocol::MAX_FIELD_STRING {
                return Err(err("state string value too long"));
            }
            out.push(1);
            out.extend_from_slice(&(v.len() as u16).to_be_bytes());
            out.extend_from_slice(v.as_bytes());
        }
        ActionValue::Num(n) => {
            if !n.is_finite() {
                return Err(err("non-finite state number"));
            }
            out.push(2);
            out.extend_from_slice(&n.to_be_bytes());
        }
        ActionValue::Bool(b) => {
            out.push(3);
            out.push(*b as u8);
        }
    }
    Ok(())
}

// Style bitmap bits (document-format.md §4).
const S_COLOR: u32 = 1 << 0;
const S_BACKGROUND: u32 = 1 << 1;
const S_FONT_SIZE: u32 = 1 << 2;
const S_FONT_WEIGHT: u32 = 1 << 3;
const S_CORNER: u32 = 1 << 4;
const S_FAMILY: u32 = 1 << 5;
const S_ALIGN: u32 = 1 << 6;
const S_WIDTH: u32 = 1 << 7;
const S_HEIGHT: u32 = 1 << 8;
const S_UNDERLINE: u32 = 1 << 9;
const S_SIZE_TOKEN: u32 = 1 << 10;
const S_PADDING_TOKEN: u32 = 1 << 11;
const S_GAP_TOKEN: u32 = 1 << 12;
const S_SHADOW: u32 = 1 << 13;
const S_BORDER: u32 = 1 << 14;
const S_HOVER: u32 = 1 << 15;
const S_BACKDROP: u32 = 1 << 16;
const S_WRAP: u32 = 1 << 17;
const S_PADDING_PX: u32 = 1 << 18;
const S_GAP_PX: u32 = 1 << 19;
const S_VALIGN: u32 = 1 << 20;
const S_ELLIPSIS: u32 = 1 << 21;
const S_PADDING_X_TOKEN: u32 = 1 << 22;
const S_PADDING_X_PX: u32 = 1 << 23;
const S_PADDING_Y_TOKEN: u32 = 1 << 24;
const S_PADDING_Y_PX: u32 = 1 << 25;
const S_MEASURE_GROUP: u32 = 1 << 26;
/// Every style-property bit this build understands. Public so a test can ask
/// which bits are still free instead of guessing one — a guessed bit becomes
/// a real property eventually, and then the test is asserting about the
/// wrong thing while still passing for a while.
pub const KNOWN_STYLE_BITS: u32 = S_KNOWN;

const S_KNOWN: u32 = S_COLOR
    | S_BACKGROUND
    | S_FONT_SIZE
    | S_FONT_WEIGHT
    | S_CORNER
    | S_FAMILY
    | S_ALIGN
    | S_WIDTH
    | S_HEIGHT
    | S_UNDERLINE
    | S_SIZE_TOKEN
    | S_PADDING_TOKEN
    | S_GAP_TOKEN
    | S_SHADOW
    | S_BORDER
    | S_HOVER
    | S_BACKDROP
    | S_WRAP
    | S_PADDING_PX
    | S_GAP_PX
    | S_VALIGN
    | S_ELLIPSIS
    | S_PADDING_X_TOKEN
    | S_PADDING_X_PX
    | S_PADDING_Y_TOKEN
    | S_PADDING_Y_PX
    | S_MEASURE_GROUP;

// ------------------------------------------------------------------ encode

pub fn encode(doc: &Document) -> Result<Vec<u8>, DocError> {
    let mut out = vec![0u8; HEADER_LEN];

    for s in &doc.strings {
        let len = u16::try_from(s.len()).map_err(|_| err("string too long"))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    for style in &doc.styles {
        out.extend_from_slice(&style.name_idx.to_be_bytes());
        let mut bitmap = 0u32;
        if style.color.is_some() {
            bitmap |= S_COLOR;
        }
        if style.background.is_some() {
            bitmap |= S_BACKGROUND;
        }
        if style.font_size.is_some() {
            bitmap |= S_FONT_SIZE;
        }
        if style.font_weight.is_some() {
            bitmap |= S_FONT_WEIGHT;
        }
        if style.corner_radius.is_some() {
            bitmap |= S_CORNER;
        }
        if style.font_family.is_some() {
            bitmap |= S_FAMILY;
        }
        if style.align.is_some() {
            bitmap |= S_ALIGN;
        }
        if style.width.is_some() {
            bitmap |= S_WIDTH;
        }
        if style.height.is_some() {
            bitmap |= S_HEIGHT;
        }
        if style.underline.is_some() {
            bitmap |= S_UNDERLINE;
        }
        if style.size_token.is_some() {
            bitmap |= S_SIZE_TOKEN;
        }
        if style.padding_token.is_some() {
            bitmap |= S_PADDING_TOKEN;
        }
        if style.gap_token.is_some() {
            bitmap |= S_GAP_TOKEN;
        }
        if style.shadow_token.is_some() {
            bitmap |= S_SHADOW;
        }
        if style.border.is_some() {
            bitmap |= S_BORDER;
        }
        if style.hover.is_some() {
            bitmap |= S_HOVER;
        }
        if style.backdrop.is_some() {
            bitmap |= S_BACKDROP;
        }
        if style.wrap.is_some() {
            bitmap |= S_WRAP;
        }
        if style.padding_px.is_some() {
            bitmap |= S_PADDING_PX;
        }
        if style.valign.is_some() {
            bitmap |= S_VALIGN;
        }
        if style.ellipsis.is_some() {
            bitmap |= S_ELLIPSIS;
        }
        if style.padding_x_token.is_some() {
            bitmap |= S_PADDING_X_TOKEN;
        }
        if style.padding_x_px.is_some() {
            bitmap |= S_PADDING_X_PX;
        }
        if style.padding_y_token.is_some() {
            bitmap |= S_PADDING_Y_TOKEN;
        }
        if style.padding_y_px.is_some() {
            bitmap |= S_PADDING_Y_PX;
        }
        if style.measure_group.is_some() {
            bitmap |= S_MEASURE_GROUP;
        }
        if style.gap_px.is_some() {
            bitmap |= S_GAP_PX;
        }
        out.extend_from_slice(&bitmap.to_be_bytes());
        // Payload length, so a decoder that does not know every bit can skip
        // to the next style instead of giving up on the document. New bits
        // are only ever assigned upward, so an older reader consumes the
        // properties it knows and jumps the rest.
        let len_slot = out.len();
        out.extend_from_slice(&0u16.to_be_bytes());
        let payload_start = out.len();
        if let Some(c) = style.color {
            write_color_ref(&mut out, c);
        }
        if let Some(c) = style.background {
            write_color_ref(&mut out, c);
        }
        if let Some(v) = style.font_size {
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(v) = style.font_weight {
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(v) = style.corner_radius {
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(v) = style.font_family {
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(v) = style.align {
            out.push(v.as_u8());
        }
        if let Some(v) = style.width {
            encode_dim(&mut out, v)?;
        }
        if let Some(v) = style.height {
            encode_dim(&mut out, v)?;
        }
        if let Some(v) = style.underline {
            out.push(v as u8);
        }
        for idx in [style.size_token, style.padding_token, style.gap_token]
            .into_iter()
            .flatten()
        {
            out.extend_from_slice(&idx.to_be_bytes());
        }
        if let Some(idx) = style.shadow_token {
            out.extend_from_slice(&idx.to_be_bytes());
        }
        if let Some(w) = style.border {
            out.extend_from_slice(&w.to_be_bytes());
            // A border always carries its colour; an outline the theme cannot
            // see would be the same mistake raw sizes were.
            write_color_ref(&mut out, style.border_color.unwrap_or(ColorRef::Literal(Color {
                r: 0, g: 0, b: 0, a: 0x30,
            })));
        }
        if let Some(idx) = style.hover {
            out.extend_from_slice(&idx.to_be_bytes());
        }
        if let Some(blur) = style.backdrop {
            out.extend_from_slice(&blur.to_be_bytes());
        }
        if let Some(v) = style.wrap {
            out.push(v as u8);
        }
        for v in [style.padding_px, style.gap_px].into_iter().flatten() {
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(v) = style.valign {
            out.push(v as u8);
        }
        if let Some(v) = style.ellipsis {
            out.push(v as u8);
        }
        for v in [style.padding_x_token, style.padding_y_token].into_iter().flatten() {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for v in [style.padding_x_px, style.padding_y_px].into_iter().flatten() {
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(v) = style.measure_group {
            out.extend_from_slice(&v.to_be_bytes());
        }
        let payload_len = u16::try_from(out.len() - payload_start)
            .map_err(|_| err("style payload too large"))?;
        out[len_slot..len_slot + 2].copy_from_slice(&payload_len.to_be_bytes());
    }

    for state in &doc.states {
        out.extend_from_slice(&state.name_idx.to_be_bytes());
        write_value(&mut out, &state.initial)?;
    }
    for action in &doc.actions {
        match action {
            DocAction::Navigate { target } => {
                out.push(1);
                out.extend_from_slice(&target.to_be_bytes());
            }
            DocAction::SetState { state, value } => {
                out.push(2);
                out.extend_from_slice(&state.to_be_bytes());
                write_value(&mut out, value)?;
            }
            DocAction::Toggle { state } => {
                out.push(3);
                out.extend_from_slice(&state.to_be_bytes());
            }
            DocAction::Submit { endpoint, fields } => {
                if fields.len() > MAX_SUBMIT_FIELDS {
                    return Err(err("too many submit fields"));
                }
                out.push(4);
                out.extend_from_slice(&endpoint.to_be_bytes());
                out.push(fields.len() as u8);
                for (name, state) in fields {
                    out.extend_from_slice(&name.to_be_bytes());
                    out.extend_from_slice(&state.to_be_bytes());
                }
            }
            DocAction::PickFile { into } => {
                out.push(5);
                out.extend_from_slice(&into.to_be_bytes());
            }
            DocAction::OpenMenu => out.push(6),
        }
    }

    for node in &doc.nodes {
        let mut body = Vec::new();
        let style_ref = match node {
            Node::Button { style, .. }
            | Node::TextInput { style, .. }
            | Node::Code { style, .. }
            | Node::Slider { style, .. } => *style,
            Node::Text { style, .. }
            | Node::Image { style, .. }
            | Node::Row { style, .. }
            | Node::Column { style, .. }
            | Node::Rectangle { style, .. }
            | Node::Spacer { style, .. }
            | Node::Link { style, .. }
            | Node::Scroll { style, .. }
            | Node::Chrome { style, .. }
            | Node::Icon { style, .. } => *style,
            Node::UnknownIgnorable { .. } => {
                return Err(err("cannot encode an unknown node"));
            }
            Node::When { .. }
            | Node::Key { .. }
            | Node::Menu { .. }
            | Node::Keys { .. }
            | Node::Live { .. }
            | Node::Sensitive { .. }
            | Node::Closing { .. }
            | Node::Page { .. } => NO_STYLE,
        };
        body.extend_from_slice(&style_ref.to_be_bytes());
        match node {
            Node::Text { value, .. } => body.extend_from_slice(&value.to_be_bytes()),
            Node::Image { source, .. } => body.extend_from_slice(&source.to_be_bytes()),
            Node::Row { gap, padding, target, children, .. }
            | Node::Column { gap, padding, target, children, .. } => {
                encode_dim(&mut body, *gap)?;
                encode_dim(&mut body, *padding)?;
                body.extend_from_slice(&target.to_be_bytes());
                let n = u16::try_from(children.len()).map_err(|_| err("too many children"))?;
                body.extend_from_slice(&n.to_be_bytes());
                for c in children {
                    body.extend_from_slice(&c.to_be_bytes());
                }
            }
            Node::Rectangle { width, height, .. } => {
                encode_dim(&mut body, *width)?;
                encode_dim(&mut body, *height)?;
            }
            Node::Spacer { size, .. } => encode_dim(&mut body, *size)?,
            Node::Link { label, target, .. } => {
                body.extend_from_slice(&label.to_be_bytes());
                body.extend_from_slice(&target.to_be_bytes());
            }
            Node::Scroll { child, .. } | Node::Chrome { child, .. } => {
                body.extend_from_slice(&child.to_be_bytes())
            }
            Node::Button { label, icon, action, .. } => {
                body.extend_from_slice(&label.to_be_bytes());
                body.extend_from_slice(&icon.to_be_bytes());
                body.extend_from_slice(&action.to_be_bytes());
            }
            Node::TextInput { bind, placeholder, action, multiline, .. } => {
                body.extend_from_slice(&bind.to_be_bytes());
                body.extend_from_slice(&placeholder.to_be_bytes());
                body.extend_from_slice(&action.to_be_bytes());
                body.push(*multiline as u8);
            }
            Node::Code { bind, lang, .. } => {
                body.extend_from_slice(&bind.to_be_bytes());
                body.extend_from_slice(&lang.to_be_bytes());
            }
            Node::Slider { bind, min, max, step, action, .. } => {
                body.extend_from_slice(&bind.to_be_bytes());
                body.extend_from_slice(&min.to_be_bytes());
                body.extend_from_slice(&max.to_be_bytes());
                body.extend_from_slice(&step.to_be_bytes());
                body.extend_from_slice(&action.to_be_bytes());
            }
            Node::Icon { name, size, .. } => {
                body.extend_from_slice(&name.to_be_bytes());
                encode_dim(&mut body, *size)?;
            }
            Node::When { state, invert, child } => {
                body.extend_from_slice(&state.to_be_bytes());
                body.push(*invert as u8);
                body.extend_from_slice(&child.to_be_bytes());
            }
            Node::Key { key, target, action } => {
                body.extend_from_slice(&key.to_be_bytes());
                body.extend_from_slice(&target.to_be_bytes());
                body.extend_from_slice(&action.to_be_bytes());
            }
            Node::Keys { target } => body.extend_from_slice(&target.to_be_bytes()),
            Node::Closing { target } => body.extend_from_slice(&target.to_be_bytes()),
            Node::Page { color } => write_color_ref(&mut body, *color),
            Node::Live { target, interval } => {
                body.extend_from_slice(&target.to_be_bytes());
                body.extend_from_slice(&interval.to_be_bytes());
            }
            Node::Sensitive { tier } => body.push(*tier),
            Node::Menu { items } => {
                if items.len() > MAX_MENU_ITEMS {
                    return Err(err("too many menu items"));
                }
                body.push(items.len() as u8);
                for item in items {
                    body.extend_from_slice(&item.label.to_be_bytes());
                    body.extend_from_slice(&item.icon.to_be_bytes());
                    body.extend_from_slice(&item.target.to_be_bytes());
                    body.extend_from_slice(&item.action.to_be_bytes());
                    body.push(item.danger as u8 | (item.separator as u8) << 1);
                }
            }
            Node::UnknownIgnorable { .. } => unreachable!(),
        }
        out.extend_from_slice(&node.type_code().to_be_bytes());
        let body_len = u16::try_from(body.len()).map_err(|_| err("node body too large"))?;
        out.extend_from_slice(&body_len.to_be_bytes());
        out.extend_from_slice(&body);
    }

    let total = u32::try_from(out.len()).map_err(|_| err("document too large"))?;
    if out.len() > MAX_DOC_SIZE {
        return Err(err("document exceeds size limit"));
    }
    out[0..4].copy_from_slice(&MAGIC);
    out[4] = VERSION;
    let string_count = u16::try_from(doc.strings.len()).map_err(|_| err("too many strings"))?;
    let style_count = u16::try_from(doc.styles.len()).map_err(|_| err("too many styles"))?;
    let node_count = u32::try_from(doc.nodes.len()).map_err(|_| err("too many nodes"))?;
    let state_count = u16::try_from(doc.states.len()).map_err(|_| err("too many states"))?;
    let action_count = u16::try_from(doc.actions.len()).map_err(|_| err("too many actions"))?;
    out[24..26].copy_from_slice(&state_count.to_be_bytes());
    out[26..28].copy_from_slice(&action_count.to_be_bytes());
    out[8..12].copy_from_slice(&total.to_be_bytes());
    out[12..14].copy_from_slice(&string_count.to_be_bytes());
    out[14..16].copy_from_slice(&style_count.to_be_bytes());
    out[16..20].copy_from_slice(&node_count.to_be_bytes());
    out[20..24].copy_from_slice(&doc.root.to_be_bytes());
    // [5..8] and [28..32] stay zero (reserved).

    // The encoder's output must satisfy the decoder — canonical by
    // construction, verified here so a buggy caller cannot ship bytes the
    // ecosystem would reject.
    decode(&out)?;
    Ok(out)
}

fn encode_dim(out: &mut Vec<u8>, dim: Dimension) -> Result<(), DocError> {
    let (tag, value) = match dim {
        Dimension::Auto => (0u8, 0.0f32),
        Dimension::Px(v) => (1, v),
        Dimension::Fill(v) => (2, v),
    };
    if !value.is_finite() {
        return Err(err("non-finite dimension"));
    }
    out.push(tag);
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

// ------------------------------------------------------------------ decode

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DocError> {
        let end = self.pos.checked_add(n).ok_or_else(|| err("overflow"))?;
        if end > self.bytes.len() {
            return Err(err("truncated document"));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DocError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DocError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, DocError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn f32_finite(&mut self, what: &str) -> Result<f32, DocError> {
        let v = f32::from_be_bytes(self.take(4)?.try_into().unwrap());
        if !v.is_finite() {
            return Err(err(format!("non-finite {what}")));
        }
        Ok(v)
    }

    fn color(&mut self) -> Result<Color, DocError> {
        let b = self.take(4)?;
        Ok(Color { r: b[0], g: b[1], b: b[2], a: b[3] })
    }

    fn value(&mut self, what: &str) -> Result<ActionValue, DocError> {
        match self.u8()? {
            1 => {
                let len = self.u16()? as usize;
                if len > rill_protocol::MAX_FIELD_STRING {
                    return Err(err(format!("{what}: string value too long")));
                }
                Ok(ActionValue::Str(
                    std::str::from_utf8(self.take(len)?)
                        .map_err(|_| err(format!("{what}: not UTF-8")))?
                        .to_string(),
                ))
            }
            2 => {
                let n = f64::from_be_bytes(self.take(8)?.try_into().unwrap());
                if !n.is_finite() {
                    return Err(err(format!("{what}: non-finite number")));
                }
                Ok(ActionValue::Num(n))
            }
            3 => match self.u8()? {
                0 => Ok(ActionValue::Bool(false)),
                1 => Ok(ActionValue::Bool(true)),
                b => Err(err(format!("{what}: bad bool byte {b}"))),
            },
            t => Err(err(format!("{what}: unknown value tag {t}"))),
        }
    }

    fn dimension(&mut self, what: &str) -> Result<Dimension, DocError> {
        let tag = self.u8()?;
        let value = self.f32_finite(what)?;
        match tag {
            0 if value == 0.0 => Ok(Dimension::Auto),
            0 => Err(err(format!("{what}: auto with nonzero value"))),
            1 => Ok(Dimension::Px(value)),
            2 if value > 0.0 => Ok(Dimension::Fill(value)),
            2 => Err(err(format!("{what}: fill weight must be positive"))),
            t => Err(err(format!("{what}: unknown dimension tag {t}"))),
        }
    }
}

pub fn decode(bytes: &[u8]) -> Result<Document, DocError> {
    if bytes.len() > MAX_DOC_SIZE {
        return Err(err("document exceeds size limit"));
    }
    if bytes.len() < HEADER_LEN {
        return Err(err("too small to be a document"));
    }
    if bytes[0..4] != MAGIC {
        return Err(err("bad magic"));
    }
    if bytes[4] != VERSION {
        return Err(err(format!("unsupported document version {}", bytes[4])));
    }
    if bytes[5..8] != [0; 3] || bytes[28..32] != [0; 4] {
        return Err(err("reserved header bytes nonzero"));
    }
    let total = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if total != bytes.len() {
        return Err(err("total size field does not match file size"));
    }
    let string_count = u16::from_be_bytes(bytes[12..14].try_into().unwrap());
    let style_count = u16::from_be_bytes(bytes[14..16].try_into().unwrap());
    let node_count = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let root = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    let state_count = u16::from_be_bytes(bytes[24..26].try_into().unwrap());
    let action_count = u16::from_be_bytes(bytes[26..28].try_into().unwrap());
    if node_count == 0 || node_count > MAX_NODES {
        return Err(err("node count out of range"));
    }
    if root >= node_count {
        return Err(err("root index out of range"));
    }

    let mut r = Reader { bytes, pos: HEADER_LEN };

    // Strings: strictly ascending bytewise (canonical, deduplicated).
    let mut strings = Vec::with_capacity(string_count as usize);
    for i in 0..string_count {
        let len = r.u16()? as usize;
        let s = std::str::from_utf8(r.take(len)?)
            .map_err(|_| err(format!("string {i}: not UTF-8")))?
            .to_string();
        if let Some(prev) = strings.last()
            && <String as AsRef<[u8]>>::as_ref(prev) >= s.as_bytes()
        {
            return Err(err("string table not strictly sorted"));
        }
        strings.push(s);
    }
    let check_str = |idx: u16, what: &str| -> Result<u16, DocError> {
        if (idx as usize) < strings.len() {
            Ok(idx)
        } else {
            Err(err(format!("{what}: string index {idx} out of range")))
        }
    };

    // Styles.
    let mut warnings: Vec<String> = Vec::new();
    let mut styles = Vec::with_capacity(style_count as usize);
    for i in 0..style_count {
        let name_idx = check_str(r.u16()?, "style name")?;
        // 32 bits, because the 16-bit map filled up: colour, background,
        // size, weight, corner, family, align, width, height, underline,
        // three scale tokens, shadow, border, hover — exactly sixteen. Widened
        // while the cost was one recompile rather than after it was not.
        let bitmap = r.u32()?;
        let payload_len = r.u16()? as usize;
        let payload_start = r.pos;
        let payload_end = payload_start
            .checked_add(payload_len)
            .filter(|end| *end <= r.bytes.len())
            .ok_or_else(|| err(format!("style {i}: payload runs past the document")))?;
        // An unknown property is decoration this build has not heard of:
        // skipping it renders the page slightly plainer, which beats refusing
        // the page. Reported rather than swallowed, so a version skew is
        // visible in the log instead of only in the design.
        let unknown = bitmap & !S_KNOWN;
        if unknown != 0 {
            warnings.push(format!(
                "style {i}: ignoring unknown property bits {unknown:#010x} \
                 (document written by a newer build)"
            ));
        }
        let mut style = Style { name_idx, ..Style::default() };
        if bitmap & S_COLOR != 0 {
            style.color = Some(read_color_ref(&mut r, "style color", &check_str)?);
        }
        if bitmap & S_BACKGROUND != 0 {
            style.background = Some(read_color_ref(&mut r, "style background", &check_str)?);
        }
        if bitmap & S_FONT_SIZE != 0 {
            style.font_size = Some(r.f32_finite("font size")?);
        }
        if bitmap & S_FONT_WEIGHT != 0 {
            let w = r.u16()?;
            if !(1..=1000).contains(&w) {
                return Err(err(format!("style {i}: font weight {w} out of range")));
            }
            style.font_weight = Some(w);
        }
        if bitmap & S_CORNER != 0 {
            let v = r.f32_finite("corner radius")?;
            if v < 0.0 {
                return Err(err(format!("style {i}: negative corner radius")));
            }
            style.corner_radius = Some(v);
        }
        if bitmap & S_FAMILY != 0 {
            style.font_family = Some(check_str(r.u16()?, "font family")?);
        }
        if bitmap & S_ALIGN != 0 {
            let v = r.u8()?;
            style.align = Some(
                crate::Align::from_u8(v)
                    .ok_or_else(|| err(format!("style {i}: bad align {v}")))?,
            );
        }
        if bitmap & S_WIDTH != 0 {
            style.width = Some(r.dimension("style width")?);
        }
        if bitmap & S_HEIGHT != 0 {
            style.height = Some(r.dimension("style height")?);
        }
        if bitmap & S_UNDERLINE != 0 {
            style.underline = Some(match r.u8()? {
                0 => false,
                1 => true,
                b => return Err(err(format!("style {i}: bad underline byte {b}"))),
            });
        }
        if bitmap & S_SIZE_TOKEN != 0 {
            style.size_token = Some(check_str(r.u16()?, "size token")?);
        }
        if bitmap & S_PADDING_TOKEN != 0 {
            style.padding_token = Some(check_str(r.u16()?, "padding token")?);
        }
        if bitmap & S_GAP_TOKEN != 0 {
            style.gap_token = Some(check_str(r.u16()?, "gap token")?);
        }
        if bitmap & S_SHADOW != 0 {
            style.shadow_token = Some(check_str(r.u16()?, "shadow token")?);
        }
        if bitmap & S_BORDER != 0 {
            let w = r.f32_finite("border width")?;
            if !(0.0..=64.0).contains(&w) {
                return Err(err(format!("style {i}: border width {w} out of range")));
            }
            style.border = Some(w);
            style.border_color = Some(read_color_ref(&mut r, "border colour", &check_str)?);
        }
        if bitmap & S_HOVER != 0 {
            style.hover = Some(check_str(r.u16()?, "hover style")?);
        }
        if bitmap & S_BACKDROP != 0 {
            let blur = r.f32_finite("backdrop blur")?;
            if !(0.0..=256.0).contains(&blur) {
                return Err(err(format!("style {i}: backdrop blur {blur} out of range")));
            }
            style.backdrop = Some(blur);
        }
        if bitmap & S_WRAP != 0 {
            style.wrap = Some(match r.u8()? {
                0 => false,
                1 => true,
                b => return Err(err(format!("style {i}: bad wrap byte {b}"))),
            });
        }
        if bitmap & S_PADDING_PX != 0 {
            style.padding_px = Some(r.f32_finite("style padding")?);
        }
        if bitmap & S_GAP_PX != 0 {
            style.gap_px = Some(r.f32_finite("style gap")?);
        }
        if bitmap & S_VALIGN != 0 {
            let v = r.u8()?;
            style.valign =
                Some(Align::from_u8(v).ok_or_else(|| err(format!("style: bad valign {v}")))?);
        }
        if bitmap & S_ELLIPSIS != 0 {
            style.ellipsis = Some(match r.u8()? {
                0 => false,
                1 => true,
                b => return Err(err(format!("style {i}: bad ellipsis byte {b}"))),
            });
        }
        if bitmap & S_PADDING_X_TOKEN != 0 {
            style.padding_x_token = Some(check_str(r.u16()?, "style padding-x")?);
        }
        if bitmap & S_PADDING_Y_TOKEN != 0 {
            style.padding_y_token = Some(check_str(r.u16()?, "style padding-y")?);
        }
        if bitmap & S_PADDING_X_PX != 0 {
            style.padding_x_px = Some(r.f32_finite("style padding-x")?);
        }
        if bitmap & S_PADDING_Y_PX != 0 {
            style.padding_y_px = Some(r.f32_finite("style padding-y")?);
        }
        if bitmap & S_MEASURE_GROUP != 0 {
            style.measure_group = Some(check_str(r.u16()?, "style group")?);
        }
        // Known properties must not have overrun what the writer declared.
        if r.pos > payload_end {
            return Err(err(format!("style {i}: properties overrun the payload")));
        }
        r.pos = payload_end;
        styles.push(style);
    }
    let check_style = |idx: u16| -> Result<u16, DocError> {
        if idx == NO_STYLE || (idx as usize) < styles.len() {
            Ok(idx)
        } else {
            Err(err(format!("style reference {idx} out of range")))
        }
    };

    // States: the document's complete declared state space.
    let mut states = Vec::with_capacity(state_count as usize);
    for i in 0..state_count {
        let name_idx = check_str(r.u16()?, "state name")?;
        let initial = r.value(&format!("state {i}"))?;
        states.push(StateVar { name_idx, initial });
    }
    let check_state = |idx: u16, what: &str| -> Result<u16, DocError> {
        if (idx as usize) < states.len() {
            Ok(idx)
        } else {
            Err(err(format!("{what}: state index {idx} out of range")))
        }
    };
    let state_is_bool = |idx: u16| matches!(states[idx as usize].initial, ActionValue::Bool(_));
    let state_is_str = |idx: u16| matches!(states[idx as usize].initial, ActionValue::Str(_));
    let state_is_num = |idx: u16| matches!(states[idx as usize].initial, ActionValue::Num(_));

    // Actions.
    let mut actions = Vec::with_capacity(action_count as usize);
    for i in 0..action_count {
        let what = format!("action {i}");
        let action = match r.u8()? {
            1 => {
                let target = check_str(r.u16()?, &what)?;
                validate_path(&strings[target as usize])
                    .map_err(|e| err(format!("{what}: target: {e}")))?;
                DocAction::Navigate { target }
            }
            2 => {
                let state = check_state(r.u16()?, &what)?;
                let value = r.value(&what)?;
                if std::mem::discriminant(&value)
                    != std::mem::discriminant(&states[state as usize].initial)
                {
                    return Err(err(format!("{what}: set value type mismatch")));
                }
                DocAction::SetState { state, value }
            }
            3 => {
                let state = check_state(r.u16()?, &what)?;
                if !state_is_bool(state) {
                    return Err(err(format!("{what}: toggle target is not bool")));
                }
                DocAction::Toggle { state }
            }
            4 => {
                let endpoint = check_str(r.u16()?, &what)?;
                validate_path(&strings[endpoint as usize])
                    .map_err(|e| err(format!("{what}: endpoint: {e}")))?;
                let count = r.u8()? as usize;
                if count > MAX_SUBMIT_FIELDS {
                    return Err(err(format!("{what}: too many fields")));
                }
                let mut fields = Vec::with_capacity(count);
                for _ in 0..count {
                    let name = check_str(r.u16()?, &what)?;
                    let state = check_state(r.u16()?, &what)?;
                    fields.push((name, state));
                }
                DocAction::Submit { endpoint, fields }
            }
            5 => {
                let into = check_state(r.u16()?, &what)?;
                if !state_is_str(into) {
                    return Err(err(format!("{what}: pick_file target is not a string")));
                }
                DocAction::PickFile { into }
            }
            6 => DocAction::OpenMenu,
            k => return Err(err(format!("{what}: unknown action kind {k}"))),
        };
        actions.push(action);
    }
    let check_action = |idx: u16| -> Result<u16, DocError> {
        if (idx as usize) < actions.len() {
            Ok(idx)
        } else {
            Err(err(format!("action reference {idx} out of range")))
        }
    };

    // Nodes.
    let mut nodes: Vec<Node> = Vec::with_capacity(node_count as usize);
    let mut ref_counts = vec![0u32; node_count as usize];
    for i in 0..node_count {
        let node_type = r.u16()?;
        let body_len = r.u16()? as usize;
        let body = r.take(body_len)?;
        let mut br = Reader { bytes: body, pos: 0 };

        let mut check_child = |idx: u32| -> Result<u32, DocError> {
            if idx >= i {
                return Err(err(format!(
                    "node {i}: child index {idx} not less than parent (non-canonical)"
                )));
            }
            ref_counts[idx as usize] += 1;
            Ok(idx)
        };

        let node = match node_type {
            0x0001..=0x0015 => {
                let style = check_style(br.u16()?)?;
                match node_type {
                    0x0001 => Node::Text { style, value: check_str(br.u16()?, "text")? },
                    0x0002 => {
                        let source = check_str(br.u16()?, "image source")?;
                        validate_path(&strings[source as usize])
                            .map_err(|e| err(format!("node {i}: image source: {e}")))?;
                        Node::Image { style, source }
                    }
                    0x0003 | 0x0004 => {
                        let gap = br.dimension("gap")?;
                        let padding = br.dimension("padding")?;
                        let target = {
                            let t = br.u16()?;
                            if t == NO_STYLE {
                                t
                            } else {
                                let t = check_str(t, "container target")?;
                                validate_path(&strings[t as usize])
                                    .map_err(|e| err(format!("node {i}: target: {e}")))?;
                                t
                            }
                        };
                        let n = br.u16()?;
                        let mut children = Vec::with_capacity(n as usize);
                        for _ in 0..n {
                            children.push(check_child(br.u32()?)?);
                        }
                        if node_type == 0x0003 {
                            Node::Row { style, gap, padding, target, children }
                        } else {
                            Node::Column { style, gap, padding, target, children }
                        }
                    }
                    0x0005 => Node::Rectangle {
                        style,
                        width: br.dimension("width")?,
                        height: br.dimension("height")?,
                    },
                    0x0006 => Node::Spacer { style, size: br.dimension("size")? },
                    0x0007 => {
                        let label = check_str(br.u16()?, "link label")?;
                        let target = check_str(br.u16()?, "link target")?;
                        validate_path(&strings[target as usize])
                            .map_err(|e| err(format!("node {i}: link target: {e}")))?;
                        Node::Link { style, label, target }
                    }
                    0x0008 => Node::Scroll { style, child: check_child(br.u32()?)? },
                    0x000D => Node::Chrome { style, child: check_child(br.u32()?)? },
                    0x0009 => Node::Button {
                        style,
                        label: check_str(br.u16()?, "button label")?,
                        icon: {
                            let i = br.u16()?;
                            if i == NO_STYLE { i } else { check_str(i, "button icon")? }
                        },
                        action: check_action(br.u16()?)?,
                    },
                    0x000A => {
                        let bind = check_state(br.u16()?, "text input bind")?;
                        if !state_is_str(bind) {
                            return Err(err(format!("node {i}: text input bound to non-string state")));
                        }
                        let placeholder = check_str(br.u16()?, "placeholder")?;
                        let action = br.u16()?;
                        if action != NO_STYLE {
                            check_action(action)?;
                        }
                        let multiline = match br.u8()? {
                            0 => false,
                            1 => true,
                            b => return Err(err(format!("node {i}: bad multiline byte {b}"))),
                        };
                        Node::TextInput { style, bind, placeholder, action, multiline }
                    }
                    0x0013 => {
                        let bind = check_state(br.u16()?, "slider bind")?;
                        if !state_is_num(bind) {
                            return Err(err(format!("node {i}: slider bound to non-number state")));
                        }
                        let min = br.f32_finite("slider min")?;
                        let max = br.f32_finite("slider max")?;
                        let step = br.f32_finite("slider step")?;
                        if min >= max {
                            return Err(err(format!("node {i}: slider min {min} is not below max {max}")));
                        }
                        if step < 0.0 || step > max - min {
                            return Err(err(format!("node {i}: slider step {step} outside its range")));
                        }
                        let action = br.u16()?;
                        if action != NO_STYLE {
                            check_action(action)?;
                        }
                        Node::Slider { style, bind, min, max, step, action }
                    }
                    0x000B => {
                        let state = check_state(br.u16()?, "when state")?;
                        if !state_is_bool(state) {
                            return Err(err(format!("node {i}: when bound to non-bool state")));
                        }
                        let invert = match br.u8()? {
                            0 => false,
                            1 => true,
                            b => return Err(err(format!("node {i}: bad invert byte {b}"))),
                        };
                        Node::When { state, invert, child: check_child(br.u32()?)? }
                    }
                    0x000C => Node::Icon {
                        style,
                        name: check_str(br.u16()?, "icon name")?,
                        size: br.dimension("icon size")?,
                    },
                    0x000E => {
                        if style != NO_STYLE {
                            return Err(err(format!("node {i}: key nodes carry no style")));
                        }
                        let key = check_str(br.u16()?, "key combo")?;
                        crate::validate_key_combo(&strings[key as usize])
                            .map_err(|e| err(format!("node {i}: key: {e}")))?;
                        let target = br.u16()?;
                        if target != NO_STYLE {
                            let t = check_str(target, "key target")?;
                            validate_path(&strings[t as usize])
                                .map_err(|e| err(format!("node {i}: key target: {e}")))?;
                        }
                        let action = br.u16()?;
                        if action != NO_STYLE {
                            check_action(action)?;
                        }
                        // Exactly one meaning: a key that does two things (or
                        // nothing) is a bug in the encoder, not a preference.
                        if (target == NO_STYLE) == (action == NO_STYLE) {
                            return Err(err(format!(
                                "node {i}: key needs exactly one of target/action"
                            )));
                        }
                        Node::Key { key, target, action }
                    }
                    0x000F => {
                        if style != NO_STYLE {
                            return Err(err(format!("node {i}: menu nodes carry no style")));
                        }
                        let count = br.u8()? as usize;
                        if count > MAX_MENU_ITEMS {
                            return Err(err(format!("node {i}: too many menu items")));
                        }
                        let mut items = Vec::with_capacity(count);
                        for _ in 0..count {
                            let label = check_str(br.u16()?, "menu label")?;
                            let icon = {
                                let v = br.u16()?;
                                if v == NO_STYLE { v } else { check_str(v, "menu icon")? }
                            };
                            let target = {
                                let v = br.u16()?;
                                if v != NO_STYLE {
                                    let t = check_str(v, "menu target")?;
                                    validate_path(&strings[t as usize])
                                        .map_err(|e| err(format!("node {i}: menu target: {e}")))?;
                                }
                                v
                            };
                            let action = {
                                let v = br.u16()?;
                                if v != NO_STYLE {
                                    check_action(v)?;
                                }
                                v
                            };
                            let flags = br.u8()?;
                            if flags > 0b11 {
                                return Err(err(format!("node {i}: bad menu flags {flags}")));
                            }
                            let (danger, separator) = (flags & 1 != 0, flags & 2 != 0);
                            let has_wire = (target != NO_STYLE) as u8 + (action != NO_STYLE) as u8;
                            if separator {
                                if has_wire != 0 || danger || !strings[label as usize].is_empty() {
                                    return Err(err(format!("node {i}: separator carries data")));
                                }
                            } else {
                                // An item does exactly one thing, and says what.
                                if has_wire != 1 || strings[label as usize].is_empty() {
                                    return Err(err(format!(
                                        "node {i}: menu item needs a label and exactly one of target/action"
                                    )));
                                }
                            }
                            items.push(MenuItem { label, icon, target, action, danger, separator });
                        }
                        if items.is_empty() {
                            return Err(err(format!("node {i}: empty menu")));
                        }
                        Node::Menu { items }
                    }
                    0x0010 => {
                        if style != NO_STYLE {
                            return Err(err(format!("node {i}: keys nodes carry no style")));
                        }
                        let target = check_str(br.u16()?, "keys target")?;
                        validate_path(&strings[target as usize])
                            .map_err(|e| err(format!("node {i}: keys target: {e}")))?;
                        Node::Keys { target }
                    }
                    0x0011 => {
                        if style != NO_STYLE {
                            return Err(err(format!("node {i}: live nodes carry no style")));
                        }
                        let target = check_str(br.u16()?, "live target")?;
                        validate_path(&strings[target as usize])
                            .map_err(|e| err(format!("node {i}: live target: {e}")))?;
                        let interval = br.u16()?;
                        // A page that asks to be reloaded faster than the
                        // client can serve it is asking for a busy loop.
                        if interval < MIN_LIVE_INTERVAL_MS {
                            return Err(err(format!(
                                "node {i}: live interval {interval}ms is below the {MIN_LIVE_INTERVAL_MS}ms floor"
                            )));
                        }
                        Node::Live { target, interval }
                    }
                    0x0014 => {
                        if style != NO_STYLE {
                            return Err(err(format!("node {i}: sensitive nodes carry no style")));
                        }
                        let tier = br.u8()?;
                        // The closed set, enforced on decode: a tier this
                        // build does not know must fail the document, not
                        // pass through as a number nothing downstream
                        // classifies — the fail-closed rule the tier model
                        // is built on. (tier=0 is refused at compile as a
                        // no-op; refused here too so the wire agrees.)
                        if !(1..=2).contains(&tier) {
                            return Err(err(format!("node {i}: sensitive tier {tier} unknown")));
                        }
                        Node::Sensitive { tier }
                    }
                    0x0012 => {
                        if style != NO_STYLE {
                            return Err(err(format!("node {i}: page nodes carry no style")));
                        }
                        Node::Page { color: read_color_ref(&mut br, "page background", &check_str)? }
                    }
                    0x0015 => {
                        let bind = br.u16()?;
                        let lang = br.u16()?;
                        let _ = check_str(lang, "code lang")?;
                        Node::Code { style, bind, lang }
                    }
                    _ => unreachable!(),
                }
            }
            // Known ignorable types decode like any other; the generic skip
            // below is only for types this build has never heard of.
            0x8001 => {
                let style = check_style(br.u16()?)?;
                if style != NO_STYLE {
                    return Err(err(format!("node {i}: closing nodes carry no style")));
                }
                let target = check_str(br.u16()?, "closing target")?;
                validate_path(&strings[target as usize])
                    .map_err(|e| err(format!("node {i}: closing target: {e}")))?;
                Node::Closing { target }
            }
            t if t >= IGNORABLE_TYPE_START => {
                // Ignorable unknown: skip the body (length prefix makes this
                // mechanical), keep a placeholder for index accounting.
                br.pos = br.bytes.len();
                Node::UnknownIgnorable { node_type: t }
            }
            t => {
                return Err(err(format!(
                    "node {i}: unknown critical node type {t:#06x} — document requires a newer viewer"
                )));
            }
        };
        if br.pos != br.bytes.len() {
            return Err(err(format!("node {i}: body length mismatch")));
        }
        nodes.push(node);
    }

    if r.pos != bytes.len() {
        return Err(err("trailing bytes after node table"));
    }

    // Tree property: root referenced zero times, everything else exactly once.
    for (i, &count) in ref_counts.iter().enumerate() {
        let expected = if i as u32 == root { 0 } else { 1 };
        if count != expected {
            return Err(err(format!(
                "node {i}: referenced {count} times (expected {expected}) — not a tree"
            )));
        }
    }

    // Depth. Children always precede their parent, so one forward pass gives
    // every node's height without recursing — which is the point: the walkers
    // downstream of here *do* recurse, and an unbounded chain would overflow
    // their stack rather than fail a check.
    let mut height = vec![1u32; nodes.len()];
    for i in 0..nodes.len() {
        let deepest = nodes[i].children().iter().map(|&c| height[c as usize]).max().unwrap_or(0);
        height[i] = deepest + 1;
        if height[i] > MAX_DEPTH {
            return Err(err(format!(
                "node {i}: nesting deeper than {MAX_DEPTH} — document rejected before any walker recurses"
            )));
        }
    }

    Ok(Document { strings, styles, states, actions, nodes, root, warnings })
}
