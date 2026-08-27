//! Semantic history — the durable substrate behind memory, search, replay,
//! agent context, sharing and audit (specs/history.md).
//!
//! The thesis in one line: because Rill's frames are *meaning* rather than
//! pixels, the display protocol is already a memory. Keeping it costs
//! kilobytes where a screen recorder costs gigabytes, and what comes back
//! out is structured — searchable text, declared verbs, window lifecycle —
//! not a video someone has to watch.
//!
//! What this crate owns:
//!
//! * [`event`] — the event vocabulary and its strict, varint-and-delta
//!   codec. No keystrokes exist as an event type; typed text reaches
//!   history only as rendered frames, so masked input arrives masked.
//! * [`segment`] — `.rhs` files: chunked, compressed, crash-honest, with
//!   frames kept in their own chunks so retention can age them out without
//!   touching the transcript.
//! * [`index`] — the seal-time transcript, postings and bloom filter. What
//!   makes history searchable, and what agents read instead of frames.
//! * [`corpus`] — a directory of segments plus a manifest of Bloom filters,
//!   so a search rejects most of history without opening it.
//!
//! What it deliberately does not own yet: encryption (the keyslot table is
//! in the header, unfilled), retention, and the query surface. Each is
//! additive — the format was shaped for them rather than around them.

pub mod corpus;
pub mod crypt;
pub mod event;
pub mod index;
pub mod retention;
pub mod segment;

pub use event::{Event, EventError, GapReason, Stamped, Tier, WindowState};
pub use corpus::{Corpus, CorpusHit, SegmentInfo};
pub use index::{Bloom, Hit, Index, TranscriptEntry};
pub use segment::{
    ChunkCodec, Header, Keyslot, SegmentError, SegmentRead, SegmentWriter, read, read_bytes,
};

/// Milliseconds since the unix epoch — what `Sync` events carry so a
/// monotonic log can be placed on a wall clock.
pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
