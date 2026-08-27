//! Command-stream codec (wgpu-renderer.md milestone W2): a compact binary
//! encoding for a rendered frame's `Vec<DrawCommand>`.
//!
//! This is the wire primitive behind vector-native windows (W4), remoting,
//! session recording, and the agent surface — one frame's semantic draw list,
//! kilobytes instead of framebuffer megabytes, carrying hit-regions (links,
//! actions, inputs) alongside paint.
//!
//! Same discipline as the `.rill` codec (rill-doc): big-endian, tag bytes,
//! length-prefixed strings, and **strict** decoding — unknown tags, truncated
//! payloads, oversized strings, invalid UTF-8, non-finite floats, non-0/1
//! bools, unbalanced clips, and trailing bytes are all rejected. Encoding
//! validates the same limits, so every encoded stream decodes.

use std::fmt;

use crate::{ActionValue, Color, DrawCommand, MenuItem, Point, Rect, UiAction};

/// Stream magic + format version.
pub const STREAM_MAGIC: [u8; 4] = *b"RCS\x01";
/// Total stream size cap (a frame of UI commands is kilobytes; megabytes is
/// corruption or abuse).
pub const MAX_STREAM_SIZE: usize = 4 * 1024 * 1024;
/// Command-count cap.
pub const MAX_COMMANDS: usize = 65_536;
/// Cap for short strings (families, sources, targets, endpoints, field names).
pub const MAX_SHORT_STRING: usize = 1024;
/// Cap for a Text command's body.
pub const MAX_TEXT_STRING: usize = 64 * 1024;
/// Submit field-count cap (matches the `.rill` codec).
pub const MAX_SUBMIT_FIELDS: usize = 16;
/// Menu-item cap per area (mirrors the doc codec).
pub const MAX_MENU_ITEMS: usize = 32;
/// Backdrop commands per frame. Each one costs the compositor a blur chain
/// over the accumulated scene; without a cap, 65k of them would be a GPU DoS.
pub const MAX_BACKDROPS: usize = 32;
/// Backdrop blur-radius cap (logical units; roomy enough for max zoom).
pub const MAX_BACKDROP_BLUR: f32 = 256.0;
/// Points in a single path. Enough for a dense chart or a smooth circle;
/// a curve needing more than this wants simplifying first.
pub const MAX_PATH_POINTS: usize = 4096;
/// Path points per *frame*. Every segment is a GPU instance, so the real
/// guard is the frame-wide total, not the per-path one — the same reasoning
/// as [`MAX_BACKDROPS`].
pub const MAX_PATH_POINTS_TOTAL: usize = 65_536;
/// Stroke-width cap (logical units).
pub const MAX_PATH_WIDTH: f32 = 1024.0;
/// Widest border the stream will carry. Named because three places rely on
/// the same number — encode's check, decode's check, and the zoom clamp that
/// keeps a scaled frame encodable — and they were three copies of `64.0`.
pub const MAX_BORDER_WIDTH: f32 = 64.0;

#[derive(Debug)]
pub struct StreamError(pub String);

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StreamError {}

fn err(m: impl Into<String>) -> StreamError {
    StreamError(m.into())
}

// Command tags.
const T_RECT: u8 = 1;
const T_SHADOW: u8 = 2;
const T_TEXT: u8 = 3;
const T_IMAGE: u8 = 4;
const T_PUSH_CLIP: u8 = 5;
const T_POP_CLIP: u8 = 6;
const T_LINK_AREA: u8 = 7;
const T_ACTION_AREA: u8 = 8;
const T_INPUT_AREA: u8 = 9;
const T_BACKDROP: u8 = 10;
const T_GLOW: u8 = 11;
const T_PATH: u8 = 12;
const T_BORDER: u8 = 13;
const T_FILL_PATH: u8 = 14;
const T_KEY_BIND: u8 = 15;
const T_PUSH_CLIP_ROUNDED: u8 = 16;
const T_MENU_AREA: u8 = 17;
const T_KEY_CAPTURE: u8 = 18;
const T_LIVE_REFRESH: u8 = 19;
const T_SLIDER_AREA: u8 = 20;
const T_SCROLL_AREA: u8 = 21;

// Action tags.
const A_NAVIGATE: u8 = 1;
const A_TOGGLE: u8 = 2;
const A_SET: u8 = 3;
const A_SUBMIT: u8 = 4;
const A_PICK_FILE: u8 = 5;
const A_OPEN_MENU: u8 = 6;

// ---------------------------------------------------------------- encoding

fn write_f32(out: &mut Vec<u8>, v: f32, what: &str) -> Result<(), StreamError> {
    if !v.is_finite() {
        return Err(err(format!("non-finite {what}")));
    }
    out.extend_from_slice(&v.to_be_bytes());
    Ok(())
}

fn write_rect(out: &mut Vec<u8>, r: Rect) -> Result<(), StreamError> {
    write_f32(out, r.x, "rect x")?;
    write_f32(out, r.y, "rect y")?;
    write_f32(out, r.w, "rect w")?;
    write_f32(out, r.h, "rect h")
}

fn write_color(out: &mut Vec<u8>, c: Color) {
    out.extend_from_slice(&[c.r, c.g, c.b, c.a]);
}

/// u16-length short string.
fn write_str16(out: &mut Vec<u8>, s: &str, what: &str) -> Result<(), StreamError> {
    if s.len() > MAX_SHORT_STRING {
        return Err(err(format!("{what} too long ({} bytes)", s.len())));
    }
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// u32-length text body.
fn write_str32(out: &mut Vec<u8>, s: &str, what: &str) -> Result<(), StreamError> {
    if s.len() > MAX_TEXT_STRING {
        return Err(err(format!("{what} too long ({} bytes)", s.len())));
    }
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn write_value(out: &mut Vec<u8>, value: &ActionValue) -> Result<(), StreamError> {
    match value {
        ActionValue::Str(v) => {
            out.push(1);
            write_str16(out, v, "action string value")
        }
        ActionValue::Num(n) => {
            if !n.is_finite() {
                return Err(err("non-finite action number"));
            }
            out.push(2);
            out.extend_from_slice(&n.to_be_bytes());
            Ok(())
        }
        ActionValue::Bool(b) => {
            out.push(3);
            out.push(*b as u8);
            Ok(())
        }
    }
}

fn write_action(out: &mut Vec<u8>, action: &UiAction) -> Result<(), StreamError> {
    match action {
        UiAction::Navigate(target) => {
            out.push(A_NAVIGATE);
            write_str16(out, target, "navigate target")
        }
        UiAction::Toggle(slot) => {
            out.push(A_TOGGLE);
            out.extend_from_slice(&slot.to_be_bytes());
            Ok(())
        }
        UiAction::Set(slot, value) => {
            out.push(A_SET);
            out.extend_from_slice(&slot.to_be_bytes());
            write_value(out, value)
        }
        UiAction::Submit { endpoint, fields } => {
            if fields.len() > MAX_SUBMIT_FIELDS {
                return Err(err(format!("submit has {} fields", fields.len())));
            }
            out.push(A_SUBMIT);
            write_str16(out, endpoint, "submit endpoint")?;
            out.push(fields.len() as u8);
            for (name, slot) in fields {
                write_str16(out, name, "submit field name")?;
                out.extend_from_slice(&slot.to_be_bytes());
            }
            Ok(())
        }
        UiAction::PickFile { into } => {
            out.push(A_PICK_FILE);
            out.extend_from_slice(&into.to_be_bytes());
            Ok(())
        }
        UiAction::OpenMenu => {
            out.push(A_OPEN_MENU);
            Ok(())
        }
    }
}

/// Encode a command list. Fails (rather than emitting an undecodable stream)
/// on non-finite floats, oversized strings, or unbalanced clips.
pub fn encode(commands: &[DrawCommand]) -> Result<Vec<u8>, StreamError> {
    if commands.len() > MAX_COMMANDS {
        return Err(err(format!("{} commands exceeds cap", commands.len())));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&STREAM_MAGIC);
    out.extend_from_slice(&(commands.len() as u32).to_be_bytes());

    let mut clip_depth: u32 = 0;
    let mut backdrops: usize = 0;
    let mut path_points: usize = 0;
    for command in commands {
        match command {
            DrawCommand::Path { points, color, width, closed } => {
                if points.len() > MAX_PATH_POINTS {
                    return Err(err(format!(
                        "path of {} points exceeds cap {MAX_PATH_POINTS}",
                        points.len()
                    )));
                }
                path_points += points.len();
                if path_points > MAX_PATH_POINTS_TOTAL {
                    return Err(err(format!(
                        "more than {MAX_PATH_POINTS_TOTAL} path points in one frame"
                    )));
                }
                if !(0.0..=MAX_PATH_WIDTH).contains(width) {
                    return Err(err(format!("path width {width} out of range")));
                }
                out.push(T_PATH);
                out.extend_from_slice(&(points.len() as u32).to_be_bytes());
                for p in points {
                    write_f32(&mut out, p.x, "path x")?;
                    write_f32(&mut out, p.y, "path y")?;
                }
                write_color(&mut out, *color);
                write_f32(&mut out, *width, "path width")?;
                out.push(*closed as u8);
            }
            DrawCommand::FillPath { points, contours, color } => {
                if points.len() > MAX_PATH_POINTS {
                    return Err(err(format!(
                        "fill of {} points exceeds cap {MAX_PATH_POINTS}",
                        points.len()
                    )));
                }
                path_points += points.len();
                if path_points > MAX_PATH_POINTS_TOTAL {
                    return Err(err(format!(
                        "more than {MAX_PATH_POINTS_TOTAL} path points in one frame"
                    )));
                }
                if contours.iter().map(|c| *c as usize).sum::<usize>() != points.len() {
                    return Err(err("fill contours do not partition its points".to_string()));
                }
                out.push(T_FILL_PATH);
                out.extend_from_slice(&(points.len() as u32).to_be_bytes());
                for p in points {
                    write_f32(&mut out, p.x, "fill x")?;
                    write_f32(&mut out, p.y, "fill y")?;
                }
                out.extend_from_slice(&(contours.len() as u32).to_be_bytes());
                for c in contours {
                    out.extend_from_slice(&c.to_be_bytes());
                }
                write_color(&mut out, *color);
            }
            DrawCommand::Rect { rect, color, corner_radius } => {
                out.push(T_RECT);
                write_rect(&mut out, *rect)?;
                write_color(&mut out, *color);
                write_f32(&mut out, *corner_radius, "corner radius")?;
            }
            DrawCommand::Shadow { rect, color, blur, spread, corner_radius } => {
                out.push(T_SHADOW);
                write_rect(&mut out, *rect)?;
                write_color(&mut out, *color);
                write_f32(&mut out, *blur, "shadow blur")?;
                write_f32(&mut out, *spread, "shadow spread")?;
                write_f32(&mut out, *corner_radius, "corner radius")?;
            }
            DrawCommand::Text { rect, text, color, font_size, font_weight, font_family } => {
                out.push(T_TEXT);
                write_rect(&mut out, *rect)?;
                write_str32(&mut out, text, "text body")?;
                write_color(&mut out, *color);
                write_f32(&mut out, *font_size, "font size")?;
                out.extend_from_slice(&font_weight.to_be_bytes());
                write_str16(&mut out, font_family, "font family")?;
            }
            DrawCommand::Image { rect, source } => {
                out.push(T_IMAGE);
                write_rect(&mut out, *rect)?;
                write_str16(&mut out, source, "image source")?;
            }
            DrawCommand::Backdrop { rect, blur, corner_radius } => {
                backdrops += 1;
                if backdrops > MAX_BACKDROPS {
                    return Err(err(format!("more than {MAX_BACKDROPS} backdrops")));
                }
                if !(0.0..=MAX_BACKDROP_BLUR).contains(blur) {
                    return Err(err(format!("backdrop blur {blur} out of range")));
                }
                out.push(T_BACKDROP);
                write_rect(&mut out, *rect)?;
                write_f32(&mut out, *blur, "backdrop blur")?;
                write_f32(&mut out, *corner_radius, "corner radius")?;
            }
            DrawCommand::Border { rect, color, width, corner_radius } => {
                if !(0.0..=MAX_BORDER_WIDTH).contains(width) {
                    return Err(err(format!("border width {width} out of range")));
                }
                out.push(T_BORDER);
                write_rect(&mut out, *rect)?;
                write_color(&mut out, *color);
                write_f32(&mut out, *width, "border width")?;
                write_f32(&mut out, *corner_radius, "corner radius")?;
            }
            DrawCommand::Glow { rect, color, blur, corner_radius } => {
                out.push(T_GLOW);
                write_rect(&mut out, *rect)?;
                write_color(&mut out, *color);
                write_f32(&mut out, *blur, "glow blur")?;
                write_f32(&mut out, *corner_radius, "corner radius")?;
            }
            DrawCommand::PushClip { rect, radius } => {
                // Square clips keep the original tag, so recordings made
                // before rounded clips existed still decode byte-for-byte.
                if *radius > 0.0 {
                    out.push(T_PUSH_CLIP_ROUNDED);
                    write_rect(&mut out, *rect)?;
                    write_f32(&mut out, *radius, "clip radius")?;
                } else {
                    out.push(T_PUSH_CLIP);
                    write_rect(&mut out, *rect)?;
                }
                clip_depth += 1;
            }
            DrawCommand::PopClip => {
                clip_depth = clip_depth
                    .checked_sub(1)
                    .ok_or_else(|| err("PopClip without matching PushClip"))?;
                out.push(T_POP_CLIP);
            }
            DrawCommand::KeyBind { key, target, action } => {
                out.push(T_KEY_BIND);
                write_str16(&mut out, key, "key combo")?;
                match target {
                    Some(t) => {
                        out.push(1);
                        write_str16(&mut out, t, "key target")?;
                    }
                    None => out.push(0),
                }
                match action {
                    Some(a) => {
                        out.push(1);
                        write_action(&mut out, a)?;
                    }
                    None => out.push(0),
                }
            }
            DrawCommand::KeyCapture { target } => {
                out.push(T_KEY_CAPTURE);
                write_str16(&mut out, target, "key capture target")?;
            }
            DrawCommand::LiveRefresh { target, interval } => {
                out.push(T_LIVE_REFRESH);
                write_str16(&mut out, target, "live target")?;
                out.extend_from_slice(&interval.to_be_bytes());
            }
            DrawCommand::MenuArea { rect, items } => {
                if items.len() > MAX_MENU_ITEMS {
                    return Err(err(format!("menu of {} items exceeds cap", items.len())));
                }
                out.push(T_MENU_AREA);
                write_rect(&mut out, *rect)?;
                out.push(items.len() as u8);
                for item in items {
                    write_str16(&mut out, &item.label, "menu label")?;
                    match &item.icon {
                        Some(icon) => {
                            out.push(1);
                            write_str16(&mut out, icon, "menu icon")?;
                        }
                        None => out.push(0),
                    }
                    match &item.target {
                        Some(t) => {
                            out.push(1);
                            write_str16(&mut out, t, "menu target")?;
                        }
                        None => out.push(0),
                    }
                    match &item.action {
                        Some(a) => {
                            out.push(1);
                            write_action(&mut out, a)?;
                        }
                        None => out.push(0),
                    }
                    out.push(item.danger as u8 | (item.separator as u8) << 1);
                }
            }
            DrawCommand::LinkArea { rect, target } => {
                out.push(T_LINK_AREA);
                write_rect(&mut out, *rect)?;
                write_str16(&mut out, target, "link target")?;
            }
            DrawCommand::ActionArea { rect, action } => {
                out.push(T_ACTION_AREA);
                write_rect(&mut out, *rect)?;
                write_action(&mut out, action)?;
            }
            DrawCommand::SliderArea { rect, state, min, max, step, on_release } => {
                out.push(T_SLIDER_AREA);
                write_rect(&mut out, *rect)?;
                out.extend_from_slice(&state.to_be_bytes());
                write_f32(&mut out, *min, "slider min")?;
                write_f32(&mut out, *max, "slider max")?;
                write_f32(&mut out, *step, "slider step")?;
                match on_release {
                    Some(action) => {
                        out.push(1);
                        write_action(&mut out, action)?;
                    }
                    None => out.push(0),
                }
            }
            DrawCommand::ScrollArea { rect, content } => {
                out.push(T_SCROLL_AREA);
                write_rect(&mut out, *rect)?;
                out.extend_from_slice(&content.to_be_bytes());
            }
            DrawCommand::InputArea {
                rect,
                state,
                on_enter,
                multiline,
                // Input mapping happens host-side; the stream carries the
                // area for geometry only, so the editing flag stays home.
                tab_inserts: _,
                font_size,
                font_weight,
                font_family,
                pad_x,
                pad_y,
            } => {
                out.push(T_INPUT_AREA);
                write_rect(&mut out, *rect)?;
                out.extend_from_slice(&state.to_be_bytes());
                match on_enter {
                    Some(action) => {
                        out.push(1);
                        write_action(&mut out, action)?;
                    }
                    None => out.push(0),
                }
                out.push(*multiline as u8);
                write_f32(&mut out, *font_size, "font size")?;
                out.extend_from_slice(&font_weight.to_be_bytes());
                write_str16(&mut out, font_family, "font family")?;
                write_f32(&mut out, *pad_x, "pad x")?;
                write_f32(&mut out, *pad_y, "pad y")?;
            }
        }
    }
    if clip_depth != 0 {
        return Err(err(format!("{clip_depth} unclosed PushClip(s)")));
    }
    if out.len() > MAX_STREAM_SIZE {
        return Err(err(format!("stream is {} bytes, over the cap", out.len())));
    }
    Ok(out)
}

// ---------------------------------------------------------------- decoding

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], StreamError> {
        let end = self.pos.checked_add(n).ok_or_else(|| err("overflow"))?;
        if end > self.bytes.len() {
            return Err(err("truncated stream"));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, StreamError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, StreamError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, StreamError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn f32_finite(&mut self, what: &str) -> Result<f32, StreamError> {
        let v = f32::from_be_bytes(self.take(4)?.try_into().unwrap());
        if !v.is_finite() {
            return Err(err(format!("non-finite {what}")));
        }
        Ok(v)
    }

    fn f64_finite(&mut self, what: &str) -> Result<f64, StreamError> {
        let v = f64::from_be_bytes(self.take(8)?.try_into().unwrap());
        if !v.is_finite() {
            return Err(err(format!("non-finite {what}")));
        }
        Ok(v)
    }

    fn bool(&mut self, what: &str) -> Result<bool, StreamError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(err(format!("bad {what} bool byte {b}"))),
        }
    }

    fn rect(&mut self) -> Result<Rect, StreamError> {
        Ok(Rect {
            x: self.f32_finite("rect x")?,
            y: self.f32_finite("rect y")?,
            w: self.f32_finite("rect w")?,
            h: self.f32_finite("rect h")?,
        })
    }

    fn color(&mut self) -> Result<Color, StreamError> {
        let b = self.take(4)?;
        Ok(Color { r: b[0], g: b[1], b: b[2], a: b[3] })
    }

    fn str_of(&mut self, len: usize, cap: usize, what: &str) -> Result<String, StreamError> {
        if len > cap {
            return Err(err(format!("{what} length {len} over cap")));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| err(format!("{what} is not UTF-8")))
    }

    fn str16(&mut self, what: &str) -> Result<String, StreamError> {
        let len = self.u16()? as usize;
        self.str_of(len, MAX_SHORT_STRING, what)
    }

    fn str32(&mut self, what: &str) -> Result<String, StreamError> {
        let len = self.u32()? as usize;
        self.str_of(len, MAX_TEXT_STRING, what)
    }
}

fn read_value(r: &mut Reader) -> Result<ActionValue, StreamError> {
    match r.u8()? {
        1 => Ok(ActionValue::Str(r.str16("action string value")?)),
        2 => Ok(ActionValue::Num(r.f64_finite("action number")?)),
        3 => Ok(ActionValue::Bool(r.bool("action value")?)),
        t => Err(err(format!("unknown action value tag {t}"))),
    }
}

fn read_action(r: &mut Reader) -> Result<UiAction, StreamError> {
    match r.u8()? {
        A_NAVIGATE => Ok(UiAction::Navigate(r.str16("navigate target")?)),
        A_TOGGLE => Ok(UiAction::Toggle(r.u16()?)),
        A_SET => Ok(UiAction::Set(r.u16()?, read_value(r)?)),
        A_SUBMIT => {
            let endpoint = r.str16("submit endpoint")?;
            let count = r.u8()? as usize;
            if count > MAX_SUBMIT_FIELDS {
                return Err(err(format!("submit has {count} fields")));
            }
            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                let name = r.str16("submit field name")?;
                let slot = r.u16()?;
                fields.push((name, slot));
            }
            Ok(UiAction::Submit { endpoint, fields })
        }
        A_PICK_FILE => Ok(UiAction::PickFile { into: r.u16()? }),
        A_OPEN_MENU => Ok(UiAction::OpenMenu),
        t => Err(err(format!("unknown action tag {t}"))),
    }
}

/// Decode a command stream. Strict: any non-canonical byte sequence is
/// rejected — see the module docs for the full list.
pub fn decode(bytes: &[u8]) -> Result<Vec<DrawCommand>, StreamError> {
    if bytes.len() > MAX_STREAM_SIZE {
        return Err(err(format!("stream is {} bytes, over the cap", bytes.len())));
    }
    let mut r = Reader { bytes, pos: 0 };
    if r.take(4)? != STREAM_MAGIC {
        return Err(err("bad stream magic"));
    }
    let count = r.u32()? as usize;
    if count > MAX_COMMANDS {
        return Err(err(format!("{count} commands exceeds cap")));
    }

    let mut commands = Vec::with_capacity(count.min(4096));
    let mut clip_depth: u32 = 0;
    let mut backdrops: usize = 0;
    let mut path_points: usize = 0;
    for _ in 0..count {
        let command = match r.u8()? {
            T_PATH => {
                let n = r.u32()? as usize;
                if n > MAX_PATH_POINTS {
                    return Err(err(format!("path of {n} points exceeds cap {MAX_PATH_POINTS}")));
                }
                path_points += n;
                if path_points > MAX_PATH_POINTS_TOTAL {
                    return Err(err(format!(
                        "more than {MAX_PATH_POINTS_TOTAL} path points in one frame"
                    )));
                }
                // Reserve against the declared count only after the cap check —
                // an unchecked `n` here is an allocation the sender controls.
                let mut points = Vec::with_capacity(n);
                for _ in 0..n {
                    points.push(Point {
                        x: r.f32_finite("path x")?,
                        y: r.f32_finite("path y")?,
                    });
                }
                let color = r.color()?;
                let width = r.f32_finite("path width")?;
                if !(0.0..=MAX_PATH_WIDTH).contains(&width) {
                    return Err(err(format!("path width {width} out of range")));
                }
                let closed = match r.u8()? {
                    0 => false,
                    1 => true,
                    b => return Err(err(format!("bad path closed flag {b}"))),
                };
                DrawCommand::Path { points, color, width, closed }
            }
            T_RECT => DrawCommand::Rect {
                rect: r.rect()?,
                color: r.color()?,
                corner_radius: r.f32_finite("corner radius")?,
            },
            T_SHADOW => DrawCommand::Shadow {
                rect: r.rect()?,
                color: r.color()?,
                blur: r.f32_finite("shadow blur")?,
                spread: r.f32_finite("shadow spread")?,
                corner_radius: r.f32_finite("corner radius")?,
            },
            T_TEXT => DrawCommand::Text {
                rect: r.rect()?,
                text: r.str32("text body")?,
                color: r.color()?,
                font_size: r.f32_finite("font size")?,
                font_weight: r.u16()?,
                font_family: r.str16("font family")?,
            },
            T_IMAGE => DrawCommand::Image { rect: r.rect()?, source: r.str16("image source")? },
            T_BACKDROP => {
                backdrops += 1;
                if backdrops > MAX_BACKDROPS {
                    return Err(err(format!("more than {MAX_BACKDROPS} backdrops")));
                }
                let rect = r.rect()?;
                let blur = r.f32_finite("backdrop blur")?;
                if !(0.0..=MAX_BACKDROP_BLUR).contains(&blur) {
                    return Err(err(format!("backdrop blur {blur} out of range")));
                }
                DrawCommand::Backdrop {
                    rect,
                    blur,
                    corner_radius: r.f32_finite("corner radius")?,
                }
            }
            T_FILL_PATH => {
                let n = r.u32()? as usize;
                if n > MAX_PATH_POINTS {
                    return Err(err(format!("fill of {n} points exceeds cap")));
                }
                path_points += n;
                if path_points > MAX_PATH_POINTS_TOTAL {
                    return Err(err(format!(
                        "more than {MAX_PATH_POINTS_TOTAL} path points in one frame"
                    )));
                }
                let mut points = Vec::with_capacity(n);
                for _ in 0..n {
                    points.push(Point {
                        x: r.f32_finite("fill x")?,
                        y: r.f32_finite("fill y")?,
                    });
                }
                let rings = r.u32()? as usize;
                if rings > n {
                    return Err(err("more contours than points".to_string()));
                }
                let mut contours = Vec::with_capacity(rings);
                for _ in 0..rings {
                    contours.push(r.u32()?);
                }
                if contours.iter().map(|c| *c as usize).sum::<usize>() != n {
                    return Err(err("fill contours do not partition its points".to_string()));
                }
                let color = r.color()?;
                DrawCommand::FillPath { points, contours, color }
            }
            T_BORDER => {
                let rect = r.rect()?;
                let color = r.color()?;
                let width = r.f32_finite("border width")?;
                if !(0.0..=MAX_BORDER_WIDTH).contains(&width) {
                    return Err(err(format!("border width {width} out of range")));
                }
                DrawCommand::Border {
                    rect,
                    color,
                    width,
                    corner_radius: r.f32_finite("corner radius")?,
                }
            }
            T_GLOW => DrawCommand::Glow {
                rect: r.rect()?,
                color: r.color()?,
                blur: r.f32_finite("glow blur")?,
                corner_radius: r.f32_finite("corner radius")?,
            },
            T_PUSH_CLIP => {
                clip_depth += 1;
                DrawCommand::PushClip { rect: r.rect()?, radius: 0.0 }
            }
            T_PUSH_CLIP_ROUNDED => {
                clip_depth += 1;
                let rect = r.rect()?;
                let radius = r.f32_finite("clip radius")?;
                if radius < 0.0 {
                    return Err(err("negative clip radius"));
                }
                DrawCommand::PushClip { rect, radius }
            }
            T_POP_CLIP => {
                clip_depth = clip_depth
                    .checked_sub(1)
                    .ok_or_else(|| err("PopClip without matching PushClip"))?;
                DrawCommand::PopClip
            }
            T_KEY_BIND => {
                let key = r.str16("key combo")?;
                let target = match r.u8()? {
                    0 => None,
                    1 => Some(r.str16("key target")?),
                    b => return Err(err(format!("bad key target tag {b}"))),
                };
                let action = match r.u8()? {
                    0 => None,
                    1 => Some(read_action(&mut r)?),
                    b => return Err(err(format!("bad key action tag {b}"))),
                };
                if target.is_some() == action.is_some() {
                    return Err(err("key bind needs exactly one of target/action"));
                }
                DrawCommand::KeyBind { key, target, action }
            }
            T_KEY_CAPTURE => DrawCommand::KeyCapture { target: r.str16("key capture target")? },
            T_LIVE_REFRESH => {
                let target = r.str16("live target")?;
                let interval = r.u16()?;
                if interval < crate::MIN_LIVE_INTERVAL_MS {
                    return Err(err(format!("live interval {interval}ms is below the floor")));
                }
                DrawCommand::LiveRefresh { target, interval }
            }
            T_MENU_AREA => {
                let rect = r.rect()?;
                let count = r.u8()? as usize;
                if count == 0 || count > MAX_MENU_ITEMS {
                    return Err(err(format!("menu area with {count} items")));
                }
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    let label = r.str16("menu label")?;
                    let icon = match r.u8()? {
                        0 => None,
                        1 => Some(r.str16("menu icon")?),
                        b => return Err(err(format!("bad menu icon tag {b}"))),
                    };
                    let target = match r.u8()? {
                        0 => None,
                        1 => Some(r.str16("menu target")?),
                        b => return Err(err(format!("bad menu target tag {b}"))),
                    };
                    let action = match r.u8()? {
                        0 => None,
                        1 => Some(read_action(&mut r)?),
                        b => return Err(err(format!("bad menu action tag {b}"))),
                    };
                    let flags = r.u8()?;
                    if flags > 0b11 {
                        return Err(err(format!("bad menu flags {flags}")));
                    }
                    let (danger, separator) = (flags & 1 != 0, flags & 2 != 0);
                    let wires = target.is_some() as u8 + action.is_some() as u8;
                    if separator {
                        if wires != 0 || danger || !label.is_empty() {
                            return Err(err("menu separator carries data"));
                        }
                    } else if wires != 1 || label.is_empty() {
                        return Err(err("menu item needs a label and exactly one of target/action"));
                    }
                    items.push(MenuItem { label, icon, target, action, danger, separator });
                }
                DrawCommand::MenuArea { rect, items }
            }
            T_LINK_AREA => {
                DrawCommand::LinkArea { rect: r.rect()?, target: r.str16("link target")? }
            }
            T_ACTION_AREA => {
                DrawCommand::ActionArea { rect: r.rect()?, action: read_action(&mut r)? }
            }
            T_INPUT_AREA => {
                let rect = r.rect()?;
                let state = r.u16()?;
                let on_enter = match r.u8()? {
                    0 => None,
                    1 => Some(read_action(&mut r)?),
                    b => return Err(err(format!("bad on_enter tag {b}"))),
                };
                DrawCommand::InputArea {
                    rect,
                    state,
                    on_enter,
                    multiline: r.bool("multiline")?,
                    tab_inserts: false,
                    font_size: r.f32_finite("font size")?,
                    font_weight: r.u16()?,
                    font_family: r.str16("font family")?,
                    pad_x: r.f32_finite("pad x")?,
                    pad_y: r.f32_finite("pad y")?,
                }
            }
            T_SLIDER_AREA => {
                let rect = r.rect()?;
                let state = r.u16()?;
                let min = r.f32_finite("slider min")?;
                let max = r.f32_finite("slider max")?;
                let step = r.f32_finite("slider step")?;
                if min >= max || step < 0.0 || step > max - min {
                    return Err(err(format!("slider range {min}..{max} step {step} malformed")));
                }
                let on_release = match r.u8()? {
                    0 => None,
                    1 => Some(read_action(&mut r)?),
                    b => return Err(err(format!("bad on_release tag {b}"))),
                };
                DrawCommand::SliderArea { rect, state, min, max, step, on_release }
            }
            T_SCROLL_AREA => {
                DrawCommand::ScrollArea { rect: r.rect()?, content: r.f32_finite("scroll content")? }
            }
            t => return Err(err(format!("unknown command tag {t}"))),
        };
        commands.push(command);
    }
    if clip_depth != 0 {
        return Err(err(format!("{clip_depth} unclosed PushClip(s)")));
    }
    if r.pos != bytes.len() {
        return Err(err(format!("{} trailing bytes", bytes.len() - r.pos)));
    }
    Ok(commands)
}

/// Translate a command list by `(dx, dy)` — how the compositor places a
/// window-local frame at its on-screen position. Hit-region rects move too,
/// so a translated frame stays semantically coherent.
pub fn offset_commands(commands: &[DrawCommand], dx: f32, dy: f32) -> Vec<DrawCommand> {
    let shift = |r: &Rect| Rect { x: r.x + dx, y: r.y + dy, w: r.w, h: r.h };
    commands
        .iter()
        .map(|command| match command {
            DrawCommand::KeyBind { .. }
            | DrawCommand::KeyCapture { .. }
            | DrawCommand::LiveRefresh { .. } => command.clone(),
            DrawCommand::Path { points, color, width, closed } => DrawCommand::Path {
                points: points.iter().map(|p| Point { x: p.x + dx, y: p.y + dy }).collect(),
                color: *color,
                width: *width,
                closed: *closed,
            },
            DrawCommand::FillPath { points, contours, color } => DrawCommand::FillPath {
                points: points.iter().map(|p| Point { x: p.x + dx, y: p.y + dy }).collect(),
                contours: contours.clone(),
                color: *color,
            },
            DrawCommand::Rect { rect, color, corner_radius } => DrawCommand::Rect {
                rect: shift(rect),
                color: *color,
                corner_radius: *corner_radius,
            },
            DrawCommand::Shadow { rect, color, blur, spread, corner_radius } => {
                DrawCommand::Shadow {
                    rect: shift(rect),
                    color: *color,
                    blur: *blur,
                    spread: *spread,
                    corner_radius: *corner_radius,
                }
            }
            DrawCommand::Text { rect, text, color, font_size, font_weight, font_family } => {
                DrawCommand::Text {
                    rect: shift(rect),
                    text: text.clone(),
                    color: *color,
                    font_size: *font_size,
                    font_weight: *font_weight,
                    font_family: font_family.clone(),
                }
            }
            DrawCommand::Image { rect, source } => {
                DrawCommand::Image { rect: shift(rect), source: source.clone() }
            }
            DrawCommand::Backdrop { rect, blur, corner_radius } => DrawCommand::Backdrop {
                rect: shift(rect),
                blur: *blur,
                corner_radius: *corner_radius,
            },
            DrawCommand::Border { rect, color, width, corner_radius } => DrawCommand::Border {
                rect: shift(rect),
                color: *color,
                width: *width,
                corner_radius: *corner_radius,
            },
            DrawCommand::Glow { rect, color, blur, corner_radius } => DrawCommand::Glow {
                rect: shift(rect),
                color: *color,
                blur: *blur,
                corner_radius: *corner_radius,
            },
            DrawCommand::PushClip { rect, radius } => {
                DrawCommand::PushClip { rect: shift(rect), radius: *radius }
            }
            DrawCommand::PopClip => DrawCommand::PopClip,
            DrawCommand::ScrollArea { rect, content } => {
                DrawCommand::ScrollArea { rect: shift(rect), content: *content }
            }
            DrawCommand::MenuArea { rect, items } => {
                DrawCommand::MenuArea { rect: shift(rect), items: items.clone() }
            }
            DrawCommand::LinkArea { rect, target } => {
                DrawCommand::LinkArea { rect: shift(rect), target: target.clone() }
            }
            DrawCommand::ActionArea { rect, action } => {
                DrawCommand::ActionArea { rect: shift(rect), action: action.clone() }
            }
            DrawCommand::InputArea {
                rect,
                state,
                on_enter,
                multiline,
                tab_inserts,
                font_size,
                font_weight,
                font_family,
                pad_x,
                pad_y,
            } => DrawCommand::InputArea {
                rect: shift(rect),
                state: *state,
                on_enter: on_enter.clone(),
                multiline: *multiline,
                tab_inserts: *tab_inserts,
                font_size: *font_size,
                font_weight: *font_weight,
                font_family: font_family.clone(),
                pad_x: *pad_x,
                pad_y: *pad_y,
            },
            DrawCommand::SliderArea { rect, state, min, max, step, on_release } => {
                DrawCommand::SliderArea {
                    rect: shift(rect),
                    state: *state,
                    min: *min,
                    max: *max,
                    step: *step,
                    on_release: on_release.clone(),
                }
            }
        })
        .collect()
}

/// Scale a command list by `factor` about the origin — zoom as a pure
/// command-space transform. Everything metric scales: rects, corner radii,
/// blur/spread, font sizes, input padding. Text painted from a scaled list
/// re-rasterizes at the scaled size — crisp at any factor, the
/// resolution-independence the stream exists for.
pub fn scale_commands(commands: &[DrawCommand], factor: f32) -> Vec<DrawCommand> {
    let mut out = commands.to_vec();
    for c in &mut out {
        scale_command(c, factor);
    }
    out
}

/// Scale one command in place — the whole of the transform above, and the
/// only place it is written.
///
/// It lives here rather than beside either caller because there were two: the
/// viewport zoomed its own commands with a private copy of these rules, and
/// the two agreed on every arm and every clamp entirely by hand. A command
/// added to the vocabulary had to be remembered twice, and a clamp corrected
/// in one would have been a rendering difference between a zoomed window and a
/// zoomed stream that nothing would have reported.
pub fn scale_command(command: &mut DrawCommand, factor: f32) {
    let s = |r: &mut Rect| {
        r.x *= factor;
        r.y *= factor;
        r.w *= factor;
        r.h *= factor;
    };
    let scale_points = |points: &mut Vec<Point>| {
        for p in points {
            p.x *= factor;
            p.y *= factor;
        }
    };
    match command {
        DrawCommand::ScrollArea { rect, content } => {
            s(rect);
            *content *= factor;
        }
        // Declarations, not geometry: nothing here is measured in pixels.
        DrawCommand::KeyBind { .. }
        | DrawCommand::KeyCapture { .. }
        | DrawCommand::LiveRefresh { .. }
        | DrawCommand::PopClip => {}
        DrawCommand::Path { points, width, .. } => {
            scale_points(points);
            // Clamp like the backdrop blur does: zoom must not push a legal
            // frame past the cap the decoder enforces.
            *width = (*width * factor).min(MAX_PATH_WIDTH);
        }
        DrawCommand::FillPath { points, .. } => scale_points(points),
        DrawCommand::Rect { rect, corner_radius, .. } => {
            s(rect);
            *corner_radius *= factor;
        }
        DrawCommand::Shadow { rect, blur, spread, corner_radius, .. } => {
            s(rect);
            *blur *= factor;
            *spread *= factor;
            *corner_radius *= factor;
        }
        DrawCommand::Text { rect, font_size, .. } => {
            s(rect);
            *font_size *= factor;
        }
        DrawCommand::Image { rect, .. } => s(rect),
        DrawCommand::Backdrop { rect, blur, corner_radius } => {
            s(rect);
            // Clamp so any encodable blur stays encodable at any zoom.
            *blur = (*blur * factor).min(MAX_BACKDROP_BLUR);
            *corner_radius *= factor;
        }
        DrawCommand::PushClip { rect, radius } => {
            s(rect);
            *radius *= factor;
        }
        DrawCommand::MenuArea { rect, .. }
        | DrawCommand::LinkArea { rect, .. }
        | DrawCommand::ActionArea { rect, .. } => s(rect),
        DrawCommand::InputArea { rect, font_size, pad_x, pad_y, .. } => {
            s(rect);
            *font_size *= factor;
            *pad_x *= factor;
            *pad_y *= factor;
        }
        DrawCommand::Glow { rect, blur, corner_radius, .. } => {
            s(rect);
            *blur *= factor;
            *corner_radius *= factor;
        }
        DrawCommand::Border { rect, width, corner_radius, .. } => {
            s(rect);
            *width = (*width * factor).min(MAX_BORDER_WIDTH);
            *corner_radius *= factor;
        }
        // The value space does not scale — only the geometry does.
        DrawCommand::SliderArea { rect, .. } => s(rect),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32) -> Rect {
        Rect { x, y: 1.0, w: 20.0, h: 10.0 }
    }
    const C: Color = Color { r: 10, g: 20, b: 30, a: 255 };

    /// One of everything (both Option arms, every action, every value kind).
    fn everything() -> Vec<DrawCommand> {
        vec![
            DrawCommand::Rect { rect: rect(0.0), color: C, corner_radius: 4.0 },
            DrawCommand::Shadow {
                rect: rect(1.0),
                color: C,
                blur: 8.0,
                spread: 2.0,
                corner_radius: 4.0,
            },
            DrawCommand::PushClip { rect: rect(2.0), radius: 0.0 },
            DrawCommand::Text {
                rect: rect(3.0),
                text: "héllo wörld\nline two".into(),
                color: C,
                font_size: 16.0,
                font_weight: 700,
                font_family: "monospace".into(),
            },
            DrawCommand::Image { rect: rect(4.0), source: "res/logo.png".into() },
            DrawCommand::Backdrop { rect: rect(4.5), blur: 16.0, corner_radius: 8.0 },
            DrawCommand::Glow { rect: rect(4.75), color: C, blur: 20.0, corner_radius: 12.0 },
            DrawCommand::Path {
                points: vec![Point::new(1.0, 2.0), Point::new(3.5, -4.0), Point::new(9.0, 0.25)],
                color: C,
                width: 2.5,
                closed: true,
            },
            DrawCommand::PopClip,
            DrawCommand::LinkArea { rect: rect(5.0), target: "/~launch/notes".into() },
            DrawCommand::ActionArea {
                rect: rect(6.0),
                action: UiAction::Submit {
                    endpoint: "/save".into(),
                    fields: vec![("title".into(), 0), ("body".into(), 1)],
                },
            },
            DrawCommand::ActionArea { rect: rect(7.0), action: UiAction::Navigate("/next".into()) },
            DrawCommand::ActionArea { rect: rect(8.0), action: UiAction::Toggle(3) },
            DrawCommand::ActionArea {
                rect: rect(9.0),
                action: UiAction::Set(2, ActionValue::Str("x".into())),
            },
            DrawCommand::ActionArea {
                rect: rect(10.0),
                action: UiAction::Set(2, ActionValue::Num(2.5)),
            },
            DrawCommand::ActionArea {
                rect: rect(11.0),
                action: UiAction::Set(2, ActionValue::Bool(true)),
            },
            DrawCommand::ActionArea { rect: rect(12.0), action: UiAction::PickFile { into: 4 } },
            DrawCommand::InputArea {
                rect: rect(13.0),
                state: 1,
                on_enter: Some(UiAction::Toggle(0)),
                multiline: false,
                tab_inserts: false,
                font_size: 14.0,
                font_weight: 400,
                font_family: "sans-serif".into(),
                pad_x: 8.0,
                pad_y: 6.0,
            },
            DrawCommand::InputArea {
                rect: rect(14.0),
                state: 2,
                on_enter: None,
                multiline: true,
                tab_inserts: false,
                font_size: 14.0,
                font_weight: 400,
                font_family: String::new(),
                pad_x: 8.0,
                pad_y: 6.0,
            },
        ]
    }

    #[test]
    fn round_trips_every_command_kind() {
        let commands = everything();
        let bytes = encode(&commands).unwrap();
        assert_eq!(decode(&bytes).unwrap(), commands);
    }

    #[test]
    fn empty_stream_round_trips_and_is_tiny() {
        let bytes = encode(&[]).unwrap();
        assert_eq!(bytes.len(), 8); // magic + count
        assert_eq!(decode(&bytes).unwrap(), vec![]);
    }

    #[test]
    fn one_rect_is_compact() {
        let bytes = encode(&[DrawCommand::Rect {
            rect: rect(0.0),
            color: C,
            corner_radius: 0.0,
        }])
        .unwrap();
        // 8 header + 1 tag + 16 rect + 4 color + 4 radius.
        assert_eq!(bytes.len(), 33);
    }

    #[test]
    fn strict_decode_rejects_hostile_bytes() {
        let good = encode(&everything()).unwrap();

        // Bad magic.
        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        assert!(decode(&bad).is_err(), "bad magic accepted");

        // Truncation at every prefix must error, never panic.
        for cut in 0..good.len() {
            assert!(decode(&good[..cut]).is_err(), "truncation at {cut} accepted");
        }

        // Trailing garbage.
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err(), "trailing byte accepted");

        // Unknown command tag (first tag byte is right after the header).
        let mut bad_tag = good.clone();
        bad_tag[8] = 0xEE;
        assert!(decode(&bad_tag).is_err(), "unknown tag accepted");

        // Lying count.
        let mut short_count = good.clone();
        short_count[4..8].copy_from_slice(&1u32.to_be_bytes());
        assert!(decode(&short_count).is_err(), "short count accepted (trailing bytes)");
    }

    #[test]
    fn strict_decode_rejects_bad_values() {
        // Non-finite float in a rect.
        let mut nan_rect = encode(&[DrawCommand::PopClip; 0]).unwrap();
        nan_rect[4..8].copy_from_slice(&1u32.to_be_bytes());
        nan_rect.push(T_RECT);
        nan_rect.extend_from_slice(&f32::NAN.to_be_bytes());
        nan_rect.extend_from_slice(&[0; 12]); // rest of rect
        nan_rect.extend_from_slice(&[0; 4]); // color
        nan_rect.extend_from_slice(&0f32.to_be_bytes());
        assert!(decode(&nan_rect).is_err(), "NaN accepted");

        // Invalid UTF-8 in an image source.
        let mut bad_utf8 = Vec::from(STREAM_MAGIC);
        bad_utf8.extend_from_slice(&1u32.to_be_bytes());
        bad_utf8.push(T_IMAGE);
        bad_utf8.extend_from_slice(&[0; 16]); // rect zeros are finite
        bad_utf8.extend_from_slice(&2u16.to_be_bytes());
        bad_utf8.extend_from_slice(&[0xFF, 0xFE]);
        assert!(decode(&bad_utf8).is_err(), "bad UTF-8 accepted");

        // Unbalanced clips, both directions.
        assert!(encode(&[DrawCommand::PopClip]).is_err(), "encode: bare PopClip");
        assert!(
            encode(&[DrawCommand::PushClip { rect: rect(0.0), radius: 0.0 }]).is_err(),
            "encode: unclosed PushClip"
        );
        let mut bare_pop = Vec::from(STREAM_MAGIC);
        bare_pop.extend_from_slice(&1u32.to_be_bytes());
        bare_pop.push(T_POP_CLIP);
        assert!(decode(&bare_pop).is_err(), "decode: bare PopClip");

        // Encode refuses what decode would refuse.
        assert!(
            encode(&[DrawCommand::Rect {
                rect: Rect { x: f32::INFINITY, y: 0.0, w: 1.0, h: 1.0 },
                color: C,
                corner_radius: 0.0
            }])
            .is_err(),
            "encode: non-finite"
        );
        let long = "x".repeat(MAX_SHORT_STRING + 1);
        assert!(
            encode(&[DrawCommand::Image { rect: rect(0.0), source: long }]).is_err(),
            "encode: oversized source"
        );
    }

    #[test]
    fn backdrop_caps_enforced_both_ways() {
        let frost =
            |i: f32| DrawCommand::Backdrop { rect: rect(i), blur: 12.0, corner_radius: 4.0 };
        // At the cap: fine. One over: refused by encode and by decode.
        let at_cap: Vec<_> = (0..MAX_BACKDROPS).map(|i| frost(i as f32)).collect();
        let bytes = encode(&at_cap).unwrap();
        assert_eq!(decode(&bytes).unwrap(), at_cap);
        let over: Vec<_> = (0..=MAX_BACKDROPS).map(|i| frost(i as f32)).collect();
        assert!(encode(&over).is_err(), "encode: too many backdrops");
        let mut forged = bytes.clone();
        // Splice one more backdrop command in and fix the count.
        let one = encode(&[frost(0.0)]).unwrap();
        forged.extend_from_slice(&one[8..]);
        forged[4..8].copy_from_slice(&((MAX_BACKDROPS as u32) + 1).to_be_bytes());
        assert!(decode(&forged).is_err(), "decode: too many backdrops");

        // Blur out of range refused on both sides.
        let hot = DrawCommand::Backdrop {
            rect: rect(0.0),
            blur: MAX_BACKDROP_BLUR + 1.0,
            corner_radius: 0.0,
        };
        assert!(encode(&[hot]).is_err(), "encode: blur over cap");
        let neg =
            DrawCommand::Backdrop { rect: rect(0.0), blur: -1.0, corner_radius: 0.0 };
        assert!(encode(&[neg]).is_err(), "encode: negative blur");

        // Zoom clamps blur at the cap instead of making the frame unencodable.
        let big = DrawCommand::Backdrop {
            rect: rect(0.0),
            blur: MAX_BACKDROP_BLUR,
            corner_radius: 0.0,
        };
        let zoomed = scale_commands(&[big], 3.0);
        assert!(encode(&zoomed).is_ok(), "scaled blur must stay encodable");
    }

    #[test]
    fn path_caps_enforced_both_ways() {
        let path = |n: usize| DrawCommand::Path {
            points: (0..n).map(|i| Point::new(i as f32, 0.0)).collect(),
            color: C,
            width: 1.0,
            closed: false,
        };
        // Per-path point cap: at it, fine; one over, refused by both sides.
        let at_cap = path(MAX_PATH_POINTS);
        let bytes = encode(std::slice::from_ref(&at_cap)).unwrap();
        assert_eq!(decode(&bytes).unwrap(), vec![at_cap]);
        assert!(encode(&[path(MAX_PATH_POINTS + 1)]).is_err(), "encode: path too long");
        // Forge a decoder-side over-cap path by overwriting the point count.
        let mut forged = bytes.clone();
        forged[9..13].copy_from_slice(&((MAX_PATH_POINTS as u32) + 1).to_be_bytes());
        assert!(decode(&forged).is_err(), "decode: path too long");

        // Frame-wide point budget: many legal paths that together exceed it.
        let per = MAX_PATH_POINTS;
        let count = MAX_PATH_POINTS_TOTAL / per + 1;
        let flood: Vec<_> = (0..count).map(|_| path(per)).collect();
        assert!(encode(&flood).is_err(), "encode: too many path points in a frame");

        // Width bounds on both sides.
        let wide = DrawCommand::Path {
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
            color: C,
            width: MAX_PATH_WIDTH + 1.0,
            closed: false,
        };
        assert!(encode(&[wide]).is_err(), "encode: width over cap");
        let thin = DrawCommand::Path {
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
            color: C,
            width: -1.0,
            closed: false,
        };
        assert!(encode(&[thin]).is_err(), "encode: negative width");

        // Zoom clamps width at the cap rather than making the frame
        // unencodable — same contract as backdrop blur.
        let max = DrawCommand::Path {
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
            color: C,
            width: MAX_PATH_WIDTH,
            closed: false,
        };
        assert!(encode(&scale_commands(&[max], 3.0)).is_ok(), "scaled width must stay encodable");

        // The closed flag is a strict bool.
        let two = encode(&[DrawCommand::Path {
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
            color: C,
            width: 1.0,
            closed: false,
        }])
        .unwrap();
        let mut bad = two.clone();
        let last = bad.len() - 1;
        bad[last] = 2;
        assert!(decode(&bad).is_err(), "decode: bad closed flag");
    }

    #[test]
    fn offset_moves_every_rect_and_round_trips() {
        let commands = everything();
        let moved = offset_commands(&commands, 100.0, 50.0);
        assert_eq!(moved.len(), commands.len());
        // Every rect-bearing command shifted by exactly (100, 50); moving back
        // restores the original list bit-for-bit.
        let back = offset_commands(&moved, -100.0, -50.0);
        assert_eq!(back, commands);
        if let (DrawCommand::Rect { rect: a, .. }, DrawCommand::Rect { rect: b, .. }) =
            (&commands[0], &moved[0])
        {
            assert_eq!((b.x, b.y), (a.x + 100.0, a.y + 50.0));
            assert_eq!((b.w, b.h), (a.w, a.h));
        } else {
            panic!("first command should be a rect");
        }
    }

    /// Writes seed inputs for `cargo fuzz run stream_decode`. Ignored: run
    /// explicitly with `cargo test -p rill-ui -- --ignored write_fuzz` when
    /// the corpus needs refreshing (the corpus is committed).
    #[test]
    #[ignore]
    fn write_fuzz_corpus() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz/corpus/stream_decode");
        std::fs::create_dir_all(dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            std::fs::write(format!("{dir}/{name}"), bytes).unwrap();
        };
        write("empty", &encode(&[]).unwrap());
        write("everything", &encode(&everything()).unwrap());
        let nested = vec![
            DrawCommand::PushClip { rect: rect(0.0), radius: 0.0 },
            DrawCommand::PushClip { rect: rect(1.0), radius: 0.0 },
            DrawCommand::Text {
                rect: rect(2.0),
                text: "héllo\nwörld".into(),
                color: C,
                font_size: 16.0,
                font_weight: 400,
                font_family: "mono".into(),
            },
            DrawCommand::PopClip,
            DrawCommand::PopClip,
        ];
        write("nested-clips", &encode(&nested).unwrap());
        let frosted = vec![
            DrawCommand::Backdrop { rect: rect(0.0), blur: 18.0, corner_radius: 10.0 },
            DrawCommand::Rect {
                rect: rect(0.0),
                color: Color { r: 255, g: 255, b: 255, a: 40 },
                corner_radius: 10.0,
            },
        ];
        write("frosted-panel", &encode(&frosted).unwrap());
    }

    #[test]
    fn scale_scales_metrics_and_inverts() {
        let commands = everything();
        let doubled = scale_commands(&commands, 2.0);
        if let (
            DrawCommand::Text { rect: a, font_size: fa, .. },
            DrawCommand::Text { rect: b, font_size: fb, .. },
        ) = (&commands[3], &doubled[3])
        {
            assert_eq!((b.x, b.y, b.w, b.h), (a.x * 2.0, a.y * 2.0, a.w * 2.0, a.h * 2.0));
            assert_eq!(*fb, fa * 2.0);
        } else {
            panic!("command 3 should be text");
        }
        // Scaling back down restores the original (powers of two are exact).
        assert_eq!(scale_commands(&doubled, 0.5), commands);
    }

    /// Zoom must never produce a frame the wire would refuse. Every metric
    /// with a cap has to clamp, and this checks the whole vocabulary at once
    /// rather than the three commands someone remembered — a command added
    /// later with a capped metric and no clamp fails here.
    ///
    /// It matters more since the viewport stopped carrying its own copy of
    /// these rules: one implementation is one place to get this right, and one
    /// place to prove it.
    #[test]
    fn no_zoom_can_scale_a_frame_out_of_the_encodable_range() {
        let commands = everything();
        assert!(encode(&commands).is_ok(), "the fixture itself must be encodable");
        for factor in [0.001, 0.5, 1.0, 2.0, 17.0, 1000.0, 100_000.0] {
            let scaled = scale_commands(&commands, factor);
            assert_eq!(scaled.len(), commands.len(), "×{factor} changed the command count");
            assert!(
                encode(&scaled).is_ok(),
                "×{factor} produced a frame the encoder rejects"
            );
        }
    }

    #[test]
    fn typical_frame_is_kilobytes() {
        // A plausible app frame: 200 mixed commands.
        let mut commands = Vec::new();
        for i in 0..50 {
            commands.push(DrawCommand::Rect {
                rect: rect(i as f32),
                color: C,
                corner_radius: 4.0,
            });
            commands.push(DrawCommand::Text {
                rect: rect(i as f32),
                text: "The quick brown fox jumps over the lazy dog".into(),
                color: C,
                font_size: 14.0,
                font_weight: 400,
                font_family: "sans-serif".into(),
            });
            commands.push(DrawCommand::LinkArea {
                rect: rect(i as f32),
                target: format!("/page/{i}"),
            });
            commands.push(DrawCommand::Shadow {
                rect: rect(i as f32),
                color: C,
                blur: 10.0,
                spread: 0.0,
                corner_radius: 4.0,
            });
        }
        let bytes = encode(&commands).unwrap();
        // The whole 200-command frame fits in single-digit kilobytes — the
        // vector-native premise (a dmabuf of this window would be megabytes).
        assert!(bytes.len() < 16 * 1024, "frame was {} bytes", bytes.len());
        assert_eq!(decode(&bytes).unwrap(), commands);
    }
}
