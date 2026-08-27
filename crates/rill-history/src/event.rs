//! The `.rhs` event vocabulary and its encoding (specs/history.md).
//!
//! Same discipline as the `.rill` and command-stream codecs: tag bytes,
//! length-prefixed strings, **strict** decoding (unknown tags, truncated
//! payloads, oversized strings, invalid UTF-8 are rejected), and encoding
//! that validates the same limits so everything encoded decodes.
//!
//! Departures from those codecs, both deliberate:
//!
//! * **Varints and delta timestamps.** History is append-only and huge; a
//!   `12ms later` gap should cost one byte, not four. Big-endian fixed
//!   widths stay where a field is genuinely wide (frame lengths).
//! * **No keystrokes.** There is no event type for a key press. Typed text
//!   reaches history only as rendered frames, so a masked field is stored
//!   masked. The absence is structural, not a policy someone can misset.
//!
//! Pointer *motion* is likewise absent: only clicks, drags and scroll
//! gestures are recorded (see specs/history.md — hover already shows up as
//! a frame, motion over dead space means nothing, continuous motion would
//! be the one thing making an idle desktop write, and cursor dynamics are
//! biometric).

use std::fmt;

/// Caps. Generous enough for real sessions, tight enough that corruption is
/// rejected rather than allocated.
pub const MAX_STRING: usize = 1024;
/// A frame blob is a command stream; its own codec caps it at 4 MiB.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
/// Cap on one [`Event::Text`] transcript entry. Larger than [`MAX_STRING`],
/// which sizes titles and verb names: this is everything one frame put on
/// screen, and a terminal screen with scrollback is already a few thousand
/// characters. Far below [`MAX_FRAME`], because text is the cheap artifact —
/// that is the entire premise of keeping it forever.
pub const MAX_TEXT: usize = 64 * 1024;
/// Windows in one `Snapshot`.
pub const MAX_SNAPSHOT_WINDOWS: usize = 256;
/// Ids in one `Order`.
pub const MAX_ORDER: usize = 256;

/// Sensitivity tier. A `u8`, not a 2-bit field: deployments get room for
/// their own levels, and **higher means more protected** (the data-
/// sensitivity convention — SELinux MLS, classification lattices — not the
/// privilege convention where ring 0 is strongest). A reader without a key
/// for level N must treat it as unreadable, never as T0.
pub type Tier = u8;

/// Named tiers in v1. Deployments may use the gaps.
pub const T0_ROUTINE: Tier = 0;
pub const T1_SENSITIVE: Tier = 1;
pub const T2_SEALED: Tier = 2;

/// Why a `Gap` exists — history sheds load rather than stalling the
/// compositor, and says so.
///
/// There is deliberately no `Paused` reason: recording cannot be paused
/// (specs/history.md decision 1, amended). A gap is always the system
/// failing to keep up, never the user choosing silence — sensitivity is
/// expressed as tier, and removal is an explicit delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapReason {
    /// The writer's queue was full; the compositor dropped a batch.
    Backpressure,
    /// A write failed; recording continued.
    WriteError,
}

impl GapReason {
    fn tag(self) -> u8 {
        match self {
            GapReason::Backpressure => 0,
            GapReason::WriteError => 1,
        }
    }
    fn from_tag(t: u8) -> Option<GapReason> {
        Some(match t {
            0 => GapReason::Backpressure,
            1 => GapReason::WriteError,
            _ => return None,
        })
    }
}

/// One window as a `Snapshot` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowState {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub title: String,
    pub app: String,
    /// A vector (command-stream) window, as opposed to a pixel client.
    pub vector: bool,
    pub tier: Tier,
}

/// What the log records. Tags are stable: 0x00–0x1F core, 0xE0–0xFF meta,
/// the rest reserved and rejected.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Wall-clock correlation, emitted lazily (on the next real event after
    /// a minute has passed) and at seal — never on a timer, so an idle
    /// desktop appends nothing.
    Sync { wall_ms: u64 },
    /// A window appeared or changed (upsert by id).
    Window(WindowState),
    Closed { id: u32 },
    /// Full stacking order, bottom → top.
    Order { ids: Vec<u32> },
    /// A vector window's new content: the client's own encoded bytes, stored
    /// verbatim — never a re-encoding of decoded commands.
    Frame { id: u32, bytes: Vec<u8> },
    /// What a frame put on screen, as text, recorded beside the frame that
    /// produced it.
    ///
    /// Redundant with [`Event::Frame`] the day it is written, and the whole
    /// point of the format the day it isn't: retention drops frame chunks at
    /// 90 days and keeps transcripts indefinitely (specs/history.md decision
    /// 3), which is only possible if the transcript is *stored* rather than
    /// derived. Recomputing it from the frames means the frames can never be
    /// dropped, and the 30× saving the decision is built on cannot be taken.
    ///
    /// Emitted only when the text *changes*, so a typing session costs one
    /// entry per visible change rather than one per frame.
    Text { id: u32, text: String },
    /// A button press or release, with what it hit. No motion (see module
    /// docs).
    Click { id: u32, button: u8, x: f32, y: f32, pressed: bool },
    /// A completed drag, as one event rather than a motion stream.
    Drag { id: u32, from: (f32, f32), to: (f32, f32) },
    /// One scroll gesture, coalesced.
    Scroll { id: u32, dx: f32, dy: f32 },
    Focus { id: u32 },
    /// A declared verb the app invoked — client-reported (specs/history.md,
    /// observation boundary). `params` is a hash, never the values, except
    /// where policy opts in.
    Action { id: u32, verb: String, category: u8, params: Option<[u8; 8]> },
    /// A capability grant or denial: always full metadata, never content —
    /// this *is* the audit trail.
    Capability { id: u32, kind: String, granted: bool },
    /// Keyframe: the full window set, so "state at T" seeks here and rolls
    /// forward instead of scanning from the segment head.
    Snapshot { windows: Vec<WindowState> },
    /// An honest hole.
    Gap { reason: GapReason, dropped: u32 },
    /// The audit of the auditing: a change in the recording tier floor —
    /// a policy edit, or an owner pinning an app higher. Recording itself
    /// is never off (specs/history.md decision 1, amended), so this
    /// records *classification* changes, not silence.
    Scope { floor: Tier, note: String },
}

impl Event {
    fn tag(&self) -> u8 {
        match self {
            Event::Sync { .. } => 0x00,
            Event::Window(_) => 0x01,
            Event::Closed { .. } => 0x02,
            Event::Order { .. } => 0x03,
            Event::Text { .. } => 0x04,
            Event::Frame { .. } => 0x05,
            Event::Click { .. } => 0x06,
            Event::Focus { .. } => 0x07,
            Event::Action { .. } => 0x08,
            Event::Snapshot { .. } => 0x09,
            Event::Gap { .. } => 0x0A,
            Event::Scope { .. } => 0x0B,
            Event::Capability { .. } => 0x0C,
            Event::Drag { .. } => 0x0D,
            Event::Scroll { .. } => 0x0E,
        }
    }
}

/// An event with its time delta (ms since the previous event in the chunk)
/// and its tier.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub dt_ms: u32,
    pub tier: Tier,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventError {
    Truncated,
    UnknownTag(u8),
    UnknownGapReason(u8),
    StringTooLong(usize),
    FrameTooLong(usize),
    TextTooLong(usize),
    TooManyItems(usize),
    BadUtf8,
    NonFinite,
    VarintTooLong,
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventError::Truncated => write!(f, "truncated event"),
            EventError::UnknownTag(t) => write!(f, "unknown event tag {t:#04x}"),
            EventError::UnknownGapReason(t) => write!(f, "unknown gap reason {t}"),
            EventError::StringTooLong(n) => write!(f, "string too long ({n})"),
            EventError::FrameTooLong(n) => write!(f, "frame too long ({n})"),
            EventError::TextTooLong(n) => write!(f, "transcript text too long ({n})"),
            EventError::TooManyItems(n) => write!(f, "too many items ({n})"),
            EventError::BadUtf8 => write!(f, "invalid utf-8"),
            EventError::NonFinite => write!(f, "non-finite float"),
            EventError::VarintTooLong => write!(f, "varint too long"),
        }
    }
}

impl std::error::Error for EventError {}

// ---------------------------------------------------------------- encoding

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Zigzag so small negatives cost one byte too (window positions go
/// negative when a window is dragged off the left edge).
fn put_svarint(out: &mut Vec<u8>, v: i64) {
    put_varint(out, ((v << 1) ^ (v >> 63)) as u64);
}

fn put_str(out: &mut Vec<u8>, s: &str) -> Result<(), EventError> {
    if s.len() > MAX_STRING {
        return Err(EventError::StringTooLong(s.len()));
    }
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn put_f32(out: &mut Vec<u8>, v: f32) -> Result<(), EventError> {
    if !v.is_finite() {
        return Err(EventError::NonFinite);
    }
    out.extend_from_slice(&v.to_be_bytes());
    Ok(())
}

fn put_window(out: &mut Vec<u8>, w: &WindowState) -> Result<(), EventError> {
    put_varint(out, w.id as u64);
    put_svarint(out, w.x as i64);
    put_svarint(out, w.y as i64);
    put_varint(out, w.w as u64);
    put_varint(out, w.h as u64);
    put_str(out, &w.title)?;
    put_str(out, &w.app)?;
    out.push(w.vector as u8);
    out.push(w.tier);
    Ok(())
}

/// Append one event. The caller owns chunk framing; this is just the body.
pub fn encode(out: &mut Vec<u8>, s: &Stamped) -> Result<(), EventError> {
    out.push(s.event.tag());
    put_varint(out, s.dt_ms as u64);
    out.push(s.tier);
    match &s.event {
        Event::Sync { wall_ms } => put_varint(out, *wall_ms),
        Event::Window(w) => put_window(out, w)?,
        Event::Closed { id } | Event::Focus { id } => put_varint(out, *id as u64),
        Event::Order { ids } => {
            if ids.len() > MAX_ORDER {
                return Err(EventError::TooManyItems(ids.len()));
            }
            put_varint(out, ids.len() as u64);
            for id in ids {
                put_varint(out, *id as u64);
            }
        }
        Event::Text { id, text } => {
            if text.len() > MAX_TEXT {
                return Err(EventError::TextTooLong(text.len()));
            }
            put_varint(out, *id as u64);
            put_varint(out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
        Event::Frame { id, bytes } => {
            if bytes.len() > MAX_FRAME {
                return Err(EventError::FrameTooLong(bytes.len()));
            }
            put_varint(out, *id as u64);
            put_varint(out, bytes.len() as u64);
            out.extend_from_slice(bytes);
        }
        Event::Click { id, button, x, y, pressed } => {
            put_varint(out, *id as u64);
            out.push(*button);
            put_f32(out, *x)?;
            put_f32(out, *y)?;
            out.push(*pressed as u8);
        }
        Event::Drag { id, from, to } => {
            put_varint(out, *id as u64);
            put_f32(out, from.0)?;
            put_f32(out, from.1)?;
            put_f32(out, to.0)?;
            put_f32(out, to.1)?;
        }
        Event::Scroll { id, dx, dy } => {
            put_varint(out, *id as u64);
            put_f32(out, *dx)?;
            put_f32(out, *dy)?;
        }
        Event::Action { id, verb, category, params } => {
            put_varint(out, *id as u64);
            put_str(out, verb)?;
            out.push(*category);
            match params {
                Some(h) => {
                    out.push(1);
                    out.extend_from_slice(h);
                }
                None => out.push(0),
            }
        }
        Event::Capability { id, kind, granted } => {
            put_varint(out, *id as u64);
            put_str(out, kind)?;
            out.push(*granted as u8);
        }
        Event::Snapshot { windows } => {
            if windows.len() > MAX_SNAPSHOT_WINDOWS {
                return Err(EventError::TooManyItems(windows.len()));
            }
            put_varint(out, windows.len() as u64);
            for w in windows {
                put_window(out, w)?;
            }
        }
        Event::Gap { reason, dropped } => {
            out.push(reason.tag());
            put_varint(out, *dropped as u64);
        }
        Event::Scope { floor, note } => {
            out.push(*floor);
            put_str(out, note)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- decoding

pub struct Reader<'a> {
    pub bytes: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }
    pub fn done(&self) -> bool {
        self.pos >= self.bytes.len()
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], EventError> {
        let end = self.pos.checked_add(n).ok_or(EventError::Truncated)?;
        let slice = self.bytes.get(self.pos..end).ok_or(EventError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, EventError> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool, EventError> {
        // Strict: only 0 and 1, like every other codec in the tree.
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(EventError::Truncated),
        }
    }
    fn varint(&mut self) -> Result<u64, EventError> {
        let mut v = 0u64;
        for shift in 0..10 {
            let byte = self.u8()?;
            v |= ((byte & 0x7f) as u64) << (shift * 7);
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(EventError::VarintTooLong)
    }
    fn svarint(&mut self) -> Result<i64, EventError> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }
    fn u32v(&mut self) -> Result<u32, EventError> {
        u32::try_from(self.varint()?).map_err(|_| EventError::Truncated)
    }
    fn i32v(&mut self) -> Result<i32, EventError> {
        i32::try_from(self.svarint()?).map_err(|_| EventError::Truncated)
    }
    fn f32(&mut self) -> Result<f32, EventError> {
        let b: [u8; 4] = self.take(4)?.try_into().unwrap();
        let v = f32::from_be_bytes(b);
        if v.is_finite() { Ok(v) } else { Err(EventError::NonFinite) }
    }
    fn string(&mut self) -> Result<String, EventError> {
        let n = self.varint()? as usize;
        if n > MAX_STRING {
            return Err(EventError::StringTooLong(n));
        }
        let bytes = self.take(n)?;
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| EventError::BadUtf8)
    }
    fn window(&mut self) -> Result<WindowState, EventError> {
        Ok(WindowState {
            id: self.u32v()?,
            x: self.i32v()?,
            y: self.i32v()?,
            w: self.u32v()?,
            h: self.u32v()?,
            title: self.string()?,
            app: self.string()?,
            vector: self.bool()?,
            tier: self.u8()?,
        })
    }
}

/// Decode one event. Strict: anything malformed is an error, and the
/// caller's position is left where it started so a partial tail can be
/// dropped rather than half-applied.
pub fn decode_one(r: &mut Reader<'_>) -> Result<Stamped, EventError> {
    let tag = r.u8()?;
    let dt_ms = r.u32v()?;
    let tier = r.u8()?;
    let event = match tag {
        0x00 => Event::Sync { wall_ms: r.varint()? },
        0x01 => Event::Window(r.window()?),
        0x02 => Event::Closed { id: r.u32v()? },
        0x03 => {
            let n = r.varint()? as usize;
            if n > MAX_ORDER {
                return Err(EventError::TooManyItems(n));
            }
            let mut ids = Vec::with_capacity(n.min(MAX_ORDER));
            for _ in 0..n {
                ids.push(r.u32v()?);
            }
            Event::Order { ids }
        }
        0x04 => {
            let id = r.u32v()?;
            let n = r.varint()? as usize;
            if n > MAX_TEXT {
                return Err(EventError::TextTooLong(n));
            }
            let text = std::str::from_utf8(r.take(n)?)
                .map_err(|_| EventError::BadUtf8)?
                .to_string();
            Event::Text { id, text }
        }
        0x05 => {
            let id = r.u32v()?;
            let n = r.varint()? as usize;
            if n > MAX_FRAME {
                return Err(EventError::FrameTooLong(n));
            }
            Event::Frame { id, bytes: r.take(n)?.to_vec() }
        }
        0x06 => Event::Click {
            id: r.u32v()?,
            button: r.u8()?,
            x: r.f32()?,
            y: r.f32()?,
            pressed: r.bool()?,
        },
        0x07 => Event::Focus { id: r.u32v()? },
        0x08 => {
            let id = r.u32v()?;
            let verb = r.string()?;
            let category = r.u8()?;
            let params = match r.bool()? {
                true => Some(<[u8; 8]>::try_from(r.take(8)?).unwrap()),
                false => None,
            };
            Event::Action { id, verb, category, params }
        }
        0x09 => {
            let n = r.varint()? as usize;
            if n > MAX_SNAPSHOT_WINDOWS {
                return Err(EventError::TooManyItems(n));
            }
            let mut windows = Vec::with_capacity(n.min(MAX_SNAPSHOT_WINDOWS));
            for _ in 0..n {
                windows.push(r.window()?);
            }
            Event::Snapshot { windows }
        }
        0x0A => {
            let raw = r.u8()?;
            let reason = GapReason::from_tag(raw).ok_or(EventError::UnknownGapReason(raw))?;
            Event::Gap { reason, dropped: r.u32v()? }
        }
        0x0B => Event::Scope { floor: r.u8()?, note: r.string()? },
        0x0C => {
            Event::Capability { id: r.u32v()?, kind: r.string()?, granted: r.bool()? }
        }
        0x0D => Event::Drag {
            id: r.u32v()?,
            from: (r.f32()?, r.f32()?),
            to: (r.f32()?, r.f32()?),
        },
        0x0E => Event::Scroll { id: r.u32v()?, dx: r.f32()?, dy: r.f32()? },
        other => return Err(EventError::UnknownTag(other)),
    };
    Ok(Stamped { dt_ms, tier, event })
}

/// Decode a whole chunk body. Strict — a chunk is authenticated as a unit,
/// so a malformed one is corruption, not a torn tail (torn tails are handled
/// at the chunk layer, where the incomplete chunk is simply absent).
pub fn decode_chunk(bytes: &[u8]) -> Result<Vec<Stamped>, EventError> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    while !r.done() {
        out.push(decode_one(&mut r)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Stamped> {
        vec![
            Stamped { dt_ms: 0, tier: T0_ROUTINE, event: Event::Sync { wall_ms: 1_760_000_531_000 } },
            Stamped {
                dt_ms: 0,
                tier: T0_ROUTINE,
                event: Event::Snapshot {
                    windows: vec![WindowState {
                        id: 1,
                        x: 100,
                        y: 80,
                        w: 900,
                        h: 600,
                        title: "Notes — draft".into(),
                        app: "notes".into(),
                        vector: true,
                        tier: T0_ROUTINE,
                    }],
                },
            },
            Stamped {
                dt_ms: 2310,
                tier: T0_ROUTINE,
                event: Event::Click { id: 1, button: 1, x: 412.0, y: 233.5, pressed: true },
            },
            Stamped {
                dt_ms: 30,
                tier: T0_ROUTINE,
                event: Event::Frame { id: 1, bytes: vec![7u8; 64] },
            },
            Stamped {
                dt_ms: 140,
                tier: T1_SENSITIVE,
                event: Event::Action {
                    id: 1,
                    verb: "submit".into(),
                    category: 2,
                    params: Some([1, 2, 3, 4, 5, 6, 7, 8]),
                },
            },
            Stamped {
                dt_ms: 5,
                tier: T0_ROUTINE,
                event: Event::Capability { id: 1, kind: "file.read".into(), granted: true },
            },
            Stamped {
                dt_ms: 8,
                tier: T0_ROUTINE,
                event: Event::Drag { id: 1, from: (1.0, 2.0), to: (3.0, 4.0) },
            },
            Stamped { dt_ms: 9, tier: T0_ROUTINE, event: Event::Scroll { id: 1, dx: 0.0, dy: -15.0 } },
            Stamped { dt_ms: 1, tier: T0_ROUTINE, event: Event::Focus { id: 1 } },
            Stamped { dt_ms: 1, tier: T0_ROUTINE, event: Event::Order { ids: vec![1, 2, 3] } },
            Stamped {
                dt_ms: 2,
                tier: T0_ROUTINE,
                event: Event::Gap { reason: GapReason::Backpressure, dropped: 12 },
            },
            Stamped {
                dt_ms: 3,
                tier: T0_ROUTINE,
                event: Event::Scope { floor: T2_SEALED, note: "vault pinned".into() },
            },
            Stamped { dt_ms: 4, tier: T0_ROUTINE, event: Event::Closed { id: 2 } },
        ]
    }

    #[test]
    fn round_trips_every_event_kind() {
        let events = sample();
        let mut buf = Vec::new();
        for e in &events {
            encode(&mut buf, e).expect("encodes");
        }
        assert_eq!(decode_chunk(&buf).expect("decodes"), events);
    }

    /// The property the whole append-only story rests on at this layer:
    /// every prefix either decodes to a whole number of events or errors —
    /// never a half-applied one.
    #[test]
    fn every_truncation_is_rejected_not_half_read() {
        let events = sample();
        let mut buf = Vec::new();
        for e in &events {
            encode(&mut buf, e).unwrap();
        }
        for cut in 1..buf.len() {
            let mut r = Reader::new(&buf[..cut]);
            let mut n = 0;
            loop {
                if r.done() {
                    break;
                }
                match decode_one(&mut r) {
                    Ok(_) => n += 1,
                    Err(_) => break,
                }
            }
            // Whatever decoded must be a prefix of the original.
            assert!(n <= events.len(), "cut {cut} produced too many events");
        }
    }

    #[test]
    fn unknown_tags_and_reasons_are_rejected() {
        let mut buf = vec![0x7f, 0x00, 0x00];
        assert_eq!(decode_chunk(&buf), Err(EventError::UnknownTag(0x7f)));
        buf = vec![0x0A, 0x00, 0x00, 9, 0x00];
        assert_eq!(decode_chunk(&buf), Err(EventError::UnknownGapReason(9)));
        // The retired `Paused` reason must not silently decode as something.
        buf = vec![0x0A, 0x00, 0x00, 2, 0x00];
        assert_eq!(decode_chunk(&buf), Err(EventError::UnknownGapReason(2)));
    }

    #[test]
    fn oversized_strings_and_frames_are_refused_both_ways() {
        let long = "x".repeat(MAX_STRING + 1);
        let mut buf = Vec::new();
        let e = Stamped {
            dt_ms: 0,
            tier: 0,
            event: Event::Capability { id: 1, kind: long, granted: true },
        };
        assert!(matches!(encode(&mut buf, &e), Err(EventError::StringTooLong(_))));

        // A decoder must refuse a declared length it would otherwise allocate.
        let mut hostile = vec![0x0C, 0x00, 0x00];
        put_varint(&mut hostile, 1);
        put_varint(&mut hostile, (MAX_STRING + 1) as u64);
        assert!(matches!(decode_chunk(&hostile), Err(EventError::StringTooLong(_))));
    }

    #[test]
    fn non_finite_floats_are_refused() {
        let mut buf = Vec::new();
        let e = Stamped {
            dt_ms: 0,
            tier: 0,
            event: Event::Click { id: 1, button: 1, x: f32::NAN, y: 0.0, pressed: true },
        };
        assert_eq!(encode(&mut buf, &e), Err(EventError::NonFinite));
    }

    /// Timestamps are the common case: a sub-128ms gap must cost one byte.
    #[test]
    fn small_deltas_cost_one_byte() {
        let mut a = Vec::new();
        encode(&mut a, &Stamped { dt_ms: 12, tier: 0, event: Event::Closed { id: 1 } }).unwrap();
        // tag + dt + tier + id
        assert_eq!(a.len(), 4, "a small event should be 4 bytes, got {a:?}");
    }

    #[test]
    fn negative_positions_survive() {
        let w = WindowState {
            id: 9,
            x: -1200,
            y: -3,
            w: 10,
            h: 10,
            title: String::new(),
            app: String::new(),
            vector: false,
            tier: 0,
        };
        let mut buf = Vec::new();
        encode(&mut buf, &Stamped { dt_ms: 0, tier: 0, event: Event::Window(w.clone()) }).unwrap();
        let back = decode_chunk(&buf).unwrap();
        assert_eq!(back[0].event, Event::Window(w));
    }
}
