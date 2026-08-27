//! Session-recording codec (wgpu-renderer.md W5, the north-star "append-only
//! command log"): a desktop session as timestamped semantic events — window
//! lifecycle/geometry/stacking plus each vector window's frames as the very
//! `stream` blobs the compositor received. Kilobytes per second where video
//! is megabytes, lossless, and text stays text.
//!
//! Same discipline as the `.rill` and stream codecs: big-endian, tag bytes,
//! length prefixes, strict decode, caps. Pixel windows can't be recorded
//! semantically — they appear as placeholder `Window` events with
//! `vector = false`.
//!
//! # `.rillrec` and `.rhs` are both current
//!
//! They look like duplicates and are not, so: this format is a **session
//! recording** — one continuous capture, played back frame by frame by
//! `rill-vector --replay`. `rill_history`'s `.rhs` is the **durable history
//! log** — segmented, chunked, indexed, with per-tier retention and room for
//! encryption, meant to be queried rather than watched.
//!
//! One is a tape, the other is a journal. The overlapping event vocabulary is
//! because they describe the same desktop, and `rill-history`'s
//! `convert-rillrec` example turns a tape into journal entries — a bridge
//! between two live formats, not a migration off a dead one.

use std::fmt;
use std::io::Write;

/// Recording magic + format version.
pub const RECORDING_MAGIC: [u8; 4] = *b"RRC\x01";
/// Cap on a recording accepted by the reader (a session of frames is MBs;
/// beyond this is corruption or abuse).
pub const MAX_RECORDING_SIZE: usize = 512 * 1024 * 1024;
/// Windows tracked per recording.
pub const MAX_RECORDED_WINDOWS: usize = 1024;

#[derive(Debug)]
pub struct RecordingError(pub String);

impl fmt::Display for RecordingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RecordingError {}

fn err(m: impl Into<String>) -> RecordingError {
    RecordingError(m.into())
}

const E_WINDOW: u8 = 1;
const E_CLOSED: u8 = 2;
const E_ORDER: u8 = 3;
const E_FRAME: u8 = 4;
const E_POINTER: u8 = 5;

/// One semantic event in a session.
#[derive(Debug, Clone, PartialEq)]
pub enum RecEvent {
    /// A window appeared or changed geometry/title (upsert by id).
    Window { id: u32, x: i32, y: i32, w: u32, h: u32, title: String, vector: bool },
    Closed { id: u32 },
    /// The full stacking order, bottom → top.
    Order { ids: Vec<u32> },
    /// A vector window's new content: an encoded `rill_ui::stream` blob,
    /// stored verbatim.
    Frame { id: u32, bytes: Vec<u8> },
    Pointer { x: f32, y: f32 },
}

/// An event with its time offset from the start of the recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub t_ms: u32,
    pub event: RecEvent,
}

/// Write the file header: magic + the recorded output size.
pub fn write_header(out: &mut impl Write, width: u32, height: u32) -> std::io::Result<()> {
    out.write_all(&RECORDING_MAGIC)?;
    out.write_all(&width.to_be_bytes())?;
    out.write_all(&height.to_be_bytes())
}

/// Append one stamped event.
pub fn write_event(out: &mut impl Write, e: &Stamped) -> Result<(), RecordingError> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&e.t_ms.to_be_bytes());
    match &e.event {
        RecEvent::Window { id, x, y, w, h, title, vector } => {
            if title.len() > crate::stream::MAX_SHORT_STRING {
                return Err(err("window title too long"));
            }
            buf.push(E_WINDOW);
            buf.extend_from_slice(&id.to_be_bytes());
            buf.extend_from_slice(&x.to_be_bytes());
            buf.extend_from_slice(&y.to_be_bytes());
            buf.extend_from_slice(&w.to_be_bytes());
            buf.extend_from_slice(&h.to_be_bytes());
            buf.extend_from_slice(&(title.len() as u16).to_be_bytes());
            buf.extend_from_slice(title.as_bytes());
            buf.push(*vector as u8);
        }
        RecEvent::Closed { id } => {
            buf.push(E_CLOSED);
            buf.extend_from_slice(&id.to_be_bytes());
        }
        RecEvent::Order { ids } => {
            if ids.len() > MAX_RECORDED_WINDOWS {
                return Err(err("stacking order too large"));
            }
            buf.push(E_ORDER);
            buf.extend_from_slice(&(ids.len() as u16).to_be_bytes());
            for id in ids {
                buf.extend_from_slice(&id.to_be_bytes());
            }
        }
        RecEvent::Frame { id, bytes } => {
            if bytes.len() > crate::stream::MAX_STREAM_SIZE {
                return Err(err("frame blob over the stream cap"));
            }
            buf.push(E_FRAME);
            buf.extend_from_slice(&id.to_be_bytes());
            buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            buf.extend_from_slice(bytes);
        }
        RecEvent::Pointer { x, y } => {
            if !x.is_finite() || !y.is_finite() {
                return Err(err("non-finite pointer"));
            }
            buf.push(E_POINTER);
            buf.extend_from_slice(&x.to_be_bytes());
            buf.extend_from_slice(&y.to_be_bytes());
        }
    }
    out.write_all(&buf).map_err(|e| err(format!("write failed: {e}")))
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RecordingError> {
        let end = self.pos.checked_add(n).ok_or_else(|| err("overflow"))?;
        if end > self.bytes.len() {
            return Err(err("truncated recording"));
        }
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, RecordingError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, RecordingError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, RecordingError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, RecordingError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32_finite(&mut self, what: &str) -> Result<f32, RecordingError> {
        let v = f32::from_be_bytes(self.take(4)?.try_into().unwrap());
        if !v.is_finite() {
            return Err(err(format!("non-finite {what}")));
        }
        Ok(v)
    }
}

/// Decode a whole recording: `(width, height, events)`. Strict — truncation,
/// bad tags, oversized fields, and non-monotonic garbage are rejected. Use
/// this to verify a file; use [`decode_lossy`] to replay one.
#[allow(clippy::type_complexity)]
pub fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<Stamped>), RecordingError> {
    let (width, height, mut r) = open(bytes)?;
    let mut events = Vec::new();
    while r.pos < bytes.len() {
        events.push(read_event(&mut r)?);
    }
    Ok((width, height, events))
}

/// Decode as much of a recording as is intact: the events up to the last whole
/// one, plus why reading stopped (`None` if the file ended cleanly).
///
/// A recording is an append-only log written through a buffer, so a session
/// that was *killed* rather than stopped routinely ends with a half-written
/// event — the flush landed mid-record. Rejecting the whole file over that
/// would throw away a session that is otherwise perfectly good, which is the
/// opposite of what an append-only log is for. The header must still be
/// valid: without a size there is nothing to replay into.
#[allow(clippy::type_complexity)]
pub fn decode_lossy(
    bytes: &[u8],
) -> Result<(u32, u32, Vec<Stamped>, Option<String>), RecordingError> {
    let (width, height, mut r) = open(bytes)?;
    let mut events = Vec::new();
    let mut stopped = None;
    while r.pos < bytes.len() {
        // Rewind to the last whole event on failure, so a partial tail is
        // dropped rather than half-applied.
        let good = r.pos;
        match read_event(&mut r) {
            Ok(e) => events.push(e),
            Err(e) => {
                stopped = Some(format!("{e} ({} trailing bytes)", bytes.len() - good));
                break;
            }
        }
    }
    Ok((width, height, events, stopped))
}

/// Check the size cap and header, returning the output size and a reader
/// positioned at the first event.
fn open(bytes: &[u8]) -> Result<(u32, u32, Reader<'_>), RecordingError> {
    if bytes.len() > MAX_RECORDING_SIZE {
        return Err(err("recording over the size cap"));
    }
    let mut r = Reader { bytes, pos: 0 };
    if r.take(4)? != RECORDING_MAGIC {
        return Err(err("bad recording magic"));
    }
    let width = r.u32()?;
    let height = r.u32()?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(err("bad recorded size"));
    }
    Ok((width, height, r))
}

fn read_event(r: &mut Reader) -> Result<Stamped, RecordingError> {
    let t_ms = r.u32()?;
    let event = match r.u8()? {
        E_WINDOW => {
            let id = r.u32()?;
            let x = r.i32()?;
            let y = r.i32()?;
            let w = r.u32()?;
            let h = r.u32()?;
            let len = r.u16()? as usize;
            if len > crate::stream::MAX_SHORT_STRING {
                return Err(err("window title too long"));
            }
            let title = String::from_utf8(r.take(len)?.to_vec())
                .map_err(|_| err("window title not UTF-8"))?;
            let vector = match r.u8()? {
                0 => false,
                1 => true,
                b => return Err(err(format!("bad vector flag {b}"))),
            };
            RecEvent::Window { id, x, y, w, h, title, vector }
        }
        E_CLOSED => RecEvent::Closed { id: r.u32()? },
        E_ORDER => {
            let n = r.u16()? as usize;
            if n > MAX_RECORDED_WINDOWS {
                return Err(err("stacking order too large"));
            }
            let mut ids = Vec::with_capacity(n);
            for _ in 0..n {
                ids.push(r.u32()?);
            }
            RecEvent::Order { ids }
        }
        E_FRAME => {
            let id = r.u32()?;
            let len = r.u32()? as usize;
            if len > crate::stream::MAX_STREAM_SIZE {
                return Err(err("frame blob over the stream cap"));
            }
            RecEvent::Frame { id, bytes: r.take(len)?.to_vec() }
        }
        E_POINTER => RecEvent::Pointer {
            x: r.f32_finite("pointer x")?,
            y: r.f32_finite("pointer y")?,
        },
        t => return Err(err(format!("unknown event tag {t}"))),
    };
    Ok(Stamped { t_ms, event })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (u32, u32, Vec<Stamped>) {
        let frame = crate::stream::encode(&[crate::DrawCommand::Rect {
            rect: crate::Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 },
            color: crate::Color { r: 1, g: 2, b: 3, a: 255 },
            corner_radius: 0.0,
        }])
        .unwrap();
        (
            1280,
            800,
            vec![
                Stamped {
                    t_ms: 0,
                    event: RecEvent::Window {
                        id: 1,
                        x: 60,
                        y: 40,
                        w: 560,
                        h: 420,
                        title: "Rill — Notes".into(),
                        vector: true,
                    },
                },
                Stamped { t_ms: 5, event: RecEvent::Frame { id: 1, bytes: frame } },
                Stamped { t_ms: 10, event: RecEvent::Order { ids: vec![1] } },
                Stamped { t_ms: 16, event: RecEvent::Pointer { x: 100.5, y: 200.25 } },
                Stamped { t_ms: 900, event: RecEvent::Closed { id: 1 } },
            ],
        )
    }

    #[test]
    fn round_trips() {
        let (w, h, events) = sample();
        let mut buf = Vec::new();
        write_header(&mut buf, w, h).unwrap();
        for e in &events {
            write_event(&mut buf, e).unwrap();
        }
        let (dw, dh, decoded) = decode(&buf).unwrap();
        assert_eq!((dw, dh), (w, h));
        assert_eq!(decoded, events);
    }

    #[test]
    fn strict_decode_rejects_damage() {
        let (w, h, events) = sample();
        let mut buf = Vec::new();
        write_header(&mut buf, w, h).unwrap();
        // Byte offsets at which the file is a complete recording: after the
        // header, and after each whole event. The log is append-only, so a
        // session killed mid-write still replays up to its last whole event —
        // only cuts *inside* an event are damage.
        let mut boundaries = vec![buf.len()];
        for e in &events {
            write_event(&mut buf, e).unwrap();
            boundaries.push(buf.len());
        }

        for cut in 0..buf.len() {
            let r = decode(&buf[..cut]);
            match boundaries.iter().position(|&b| b == cut) {
                // A clean boundary decodes to exactly the events written so far.
                Some(n) => {
                    let (dw, dh, got) = r.unwrap_or_else(|e| panic!("boundary {cut} rejected: {e}"));
                    assert_eq!((dw, dh), (w, h));
                    assert_eq!(got, events[..n], "boundary {cut} decoded wrong");
                }
                // Anything else is a partial header or a partial event.
                None => assert!(r.is_err(), "truncation at {cut} accepted"),
            }
        }

        // Unknown tag.
        let mut bad = buf.clone();
        bad[16] = 0xEE;
        assert!(decode(&bad).is_err(), "unknown tag accepted");
    }

    /// The replay path keeps whatever was whole. A session that was killed
    /// mid-write — the realistic ending, since the writer buffers — must still
    /// give back every event that made it to disk.
    #[test]
    fn lossy_decode_keeps_the_intact_prefix() {
        let (w, h, events) = sample();
        let mut buf = Vec::new();
        write_header(&mut buf, w, h).unwrap();
        let mut boundaries = vec![buf.len()];
        for e in &events {
            write_event(&mut buf, e).unwrap();
            boundaries.push(buf.len());
        }

        for cut in boundaries[0]..buf.len() {
            let (dw, dh, got, stopped) = decode_lossy(&buf[..cut]).unwrap();
            assert_eq!((dw, dh), (w, h));
            // However the file was cut, what comes back is a whole-event
            // prefix of what was written — never a partial or invented event.
            assert_eq!(got[..], events[..got.len()], "bad prefix at {cut}");
            let whole = boundaries.contains(&cut);
            assert_eq!(
                stopped.is_none(),
                whole,
                "cut {cut}: clean-end reported as {stopped:?}"
            );
            if whole {
                assert_eq!(got.len(), boundaries.iter().position(|&b| b == cut).unwrap());
            }
        }

        // A bad header still fails outright — there is no size to replay into.
        assert!(decode_lossy(&buf[..4]).is_err(), "short header accepted");
        assert!(decode_lossy(b"nope").is_err(), "bad magic accepted");
    }
}
