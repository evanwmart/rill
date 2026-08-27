//! Seal-time index: the part that makes history *searchable* rather than
//! merely stored (specs/history.md).
//!
//! The whole economy of this crate rests on one measured fact: frames are
//! nearly all the bytes, and the text inside them is 3–7% of that. So at
//! seal we decode each frame **once**, pull out what it put on screen, and
//! keep that beside the segment:
//!
//! ```text
//! transcript   (t, window, text)  — kept forever; frames age out at 90d
//! postings     token → [t, …]     — what a grep actually reads
//! bloom        one filter/segment — what a grep reads *first*
//! ```
//!
//! Two properties worth stating because they drive the shape:
//!
//! * **Agents and search never decode DrawCommands.** They read the
//!   transcript. Raw frames exist for pixel-perfect replay and audit; both
//!   come from one log and neither blocks the other.
//! * **The index is derived, disposable and rebuildable.** The log stays
//!   the source of truth. A corrupt or missing index is a rebuild, never a
//!   loss — which is why nothing here is load-bearing for correctness.
//!
//! Tiering: an index is built **per tier**, so text classified T2 never
//! enters the T0 index and a search made while sealed content is locked
//! cannot leak it. Callers pass the tier they are indexing.

use std::collections::BTreeMap;

use crate::event::{Event, Stamped, Tier};
use crate::segment::absolute_times;

/// A text change on screen: when, which window, and what it said.
///
/// One record per *change* — a frame that redraws identical text adds
/// nothing, which matters because most frames in a typing session repeat
/// most of their text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// Milliseconds since the segment's start.
    pub t_ms: u64,
    pub window: u32,
    pub text: String,
}

/// A sealed segment's derived index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    pub tier: Tier,
    pub transcript: Vec<TranscriptEntry>,
    /// token → the transcript entry offsets containing it.
    pub postings: BTreeMap<String, Vec<u32>>,
    /// Window titles seen, by id — enough to name a hit without opening
    /// the segment's events.
    pub titles: BTreeMap<u32, String>,
    pub bloom: Bloom,
    /// Absolute time span covered, in ms since the segment start.
    pub span: (u64, u64),
}

/// The text a frame puts on screen, in paint order.
///
/// Frames whose bytes fail to decode are skipped rather than fatal: a
/// segment with one bad frame still indexes the rest, and the log itself
/// remains the source of truth.
///
/// **Public because a writer should call it, not a reader.** A producer that
/// already holds the decoded commands — the compositor does, it decoded them
/// to display them — emits [`Event::Text`] beside the frame and the index
/// never has to decode anything. Reading uses this only as the fallback for
/// segments written before that, and once frame chunks age out it cannot be
/// used at all, which is the point.
pub fn frame_text(bytes: &[u8]) -> Option<String> {
    let cmds = rill_draw::stream::decode(bytes).ok()?;
    let mut out = String::new();
    for c in &cmds {
        if let rill_draw::DrawCommand::Text { text, .. } = c {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Split text into searchable tokens: lowercased, alphanumeric runs.
///
/// Deliberately dumb. Substring search ("handshak", "rill-auth") wants a
/// trigram index, which is a later slice; whole-token matching is what the
/// first `grep` needs and is exhaustively testable.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A tiny Bloom filter over a segment's tokens.
///
/// This is the corpus-scale trick: a year is a few thousand segments, and
/// a search tests every filter (microseconds each) before opening any
/// index. False positives cost one wasted segment read; false negatives
/// are impossible, which is the only property correctness needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom {
    bits: Vec<u64>,
    hashes: u32,
}

impl Default for Bloom {
    fn default() -> Bloom {
        Bloom::new(512)
    }
}

impl Bloom {
    /// `words` × 64 bits. 512 words = 4 KiB, comfortable for a segment's
    /// vocabulary; sizing is a tuning decision to revisit with real corpora.
    pub fn new(words: usize) -> Bloom {
        Bloom { bits: vec![0; words.max(1)], hashes: 4 }
    }

    fn probes(&self, token: &str) -> impl Iterator<Item = usize> + '_ {
        // Two independent hashes, combined — the standard Kirsch-Mitzenmacher
        // trick, so k probes cost two hashes rather than k.
        let h = blake3::hash(token.as_bytes());
        let b = h.as_bytes();
        let h1 = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let h2 = u64::from_le_bytes(b[8..16].try_into().unwrap()) | 1;
        let n = (self.bits.len() * 64) as u64;
        (0..self.hashes as u64).map(move |i| (h1.wrapping_add(h2.wrapping_mul(i)) % n) as usize)
    }

    pub fn insert(&mut self, token: &str) {
        for bit in self.probes(token).collect::<Vec<_>>() {
            self.bits[bit / 64] |= 1 << (bit % 64);
        }
    }

    /// False positives are possible; false negatives are not.
    pub fn maybe_contains(&self, token: &str) -> bool {
        self.probes(token).all(|bit| self.bits[bit / 64] & (1 << (bit % 64)) != 0)
    }

    /// Fold another filter in. Same geometry ORs bit-for-bit; a mismatch
    /// saturates instead, because a filter that can only ever say "maybe"
    /// errs in the safe direction — a wrong "no" would hide history.
    pub fn union(&mut self, other: &Bloom) {
        if self.hashes == other.hashes && self.bits.len() == other.bits.len() {
            for (a, b) in self.bits.iter_mut().zip(&other.bits) {
                *a |= b;
            }
        } else {
            self.bits.iter_mut().for_each(|w| *w = u64::MAX);
        }
    }

    pub fn bytes(&self) -> usize {
        self.bits.len() * 8
    }

    /// Serialize for the corpus manifest: hash count, then the bit words.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.bits.len() * 8);
        out.extend_from_slice(&self.hashes.to_be_bytes());
        for w in &self.bits {
            out.extend_from_slice(&w.to_be_bytes());
        }
        out
    }

    /// Rebuild from [`Bloom::to_bytes`]. `None` on anything malformed — a
    /// corrupt filter means "rescan", never "trust it".
    pub fn from_bytes(bytes: &[u8]) -> Option<Bloom> {
        if bytes.len() < 4 || !(bytes.len() - 4).is_multiple_of(8) {
            return None;
        }
        let hashes = u32::from_be_bytes(bytes[..4].try_into().ok()?);
        if hashes == 0 || hashes > 32 {
            return None;
        }
        let bits = bytes[4..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect::<Vec<u64>>();
        if bits.is_empty() {
            return None;
        }
        Some(Bloom { bits, hashes })
    }
}

/// Build the index for one tier of a decoded segment.
///
/// Called at seal, on the recorder thread between segments — or deferred to
/// idle, since a damage-gated desktop has abundant idle and sealing is the
/// one heavy operation in the writer's life.
pub fn build(events: &[Stamped], tier: Tier) -> Index {
    let mut b = Builder::new(tier);
    for (t_ms, s) in absolute_times(events) {
        b.push(t_ms, s);
    }
    b.finish()
}

/// The same index, built one event at a time — what the streaming seal
/// uses so a segment's whole decoded event list never has to exist in
/// memory at once (the 2026-08-25 soak finding: batch sealing's transient
/// became ~50 MiB of retained heap per seal on a 1 GB board).
///
/// `build` above is this Builder driven in a loop, so the two cannot
/// drift; the seal test asserts stored == rebuilt, which keeps it honest.
///
/// The one non-obvious part is the frame fallback. Batch `build` decides
/// "does this segment carry its own transcript?" by looking at *all*
/// events before indexing any — a stream cannot look ahead. So
/// frame-derived text is buffered (deduplicated, so it costs what the
/// index would have cost, not what the frames cost) and discarded the
/// moment a `Text` event proves the segment carries transcripts. If none
/// ever does, the buffer replays at `finish` — all entries are then
/// frame-derived, so replay order equals stream order and the result is
/// identical to the batch decision.
pub struct Builder {
    index: Index,
    last_text: BTreeMap<u32, String>,
    lo: u64,
    hi: u64,
    /// A `Text` event has been seen (segment-wide semantics: any tier's
    /// Text disables the frame fallback for every tier's index).
    text_present: bool,
    /// Frame-derived candidates awaiting the verdict, already deduped.
    frames_pending: Vec<(u64, u32, String)>,
    /// Dedup map for the frame path ONLY. Batch `build` never lets frames
    /// touch its `last_text` when a transcript exists — so neither may the
    /// stream, or a frame seen before an identical `Text` would suppress an
    /// entry the batch builder keeps.
    frames_last: BTreeMap<u32, String>,
}

impl Builder {
    pub fn new(tier: Tier) -> Builder {
        Builder {
            index: Index { tier, bloom: Bloom::default(), ..Index::default() },
            last_text: BTreeMap::new(),
            lo: u64::MAX,
            hi: 0,
            text_present: false,
            frames_pending: Vec::new(),
            frames_last: BTreeMap::new(),
        }
    }

    /// Feed one event at its absolute time. Events of other tiers are
    /// skipped here (same rule as batch), EXCEPT that a `Text` event of any
    /// tier still flips the segment-wide fallback switch — which is why the
    /// tier filter sits below that check, not above it.
    pub fn push(&mut self, t_ms: u64, s: &Stamped) {
        if !self.text_present && matches!(s.event, Event::Text { .. }) {
            self.text_present = true;
            self.frames_pending = Vec::new();
            self.frames_last = BTreeMap::new();
        }
        if s.tier != self.index.tier {
            return;
        }
        self.lo = self.lo.min(t_ms);
        self.hi = self.hi.max(t_ms);
        match &s.event {
            Event::Window(w) => {
                self.index.titles.insert(w.id, w.title.clone());
            }
            Event::Snapshot { windows } => {
                for w in windows {
                    self.index.titles.insert(w.id, w.title.clone());
                }
            }
            Event::Text { id, text } => {
                enter(&mut self.index, &mut self.last_text, t_ms, *id, text.clone());
            }
            Event::Frame { id, bytes } => {
                if self.text_present {
                    return;
                }
                let Some(text) = frame_text(bytes) else { return };
                // Dedup while buffering (same per-window rule as `enter`),
                // so the pending list costs the index's size, not the
                // session's.
                if self.frames_last.get(id) == Some(&text) {
                    return;
                }
                self.frames_last.insert(*id, text.clone());
                self.frames_pending.push((t_ms, *id, text));
            }
            _ => {}
        }
    }

    pub fn finish(mut self) -> Index {
        if !self.text_present {
            // No transcript anywhere in the segment: the frame-derived
            // entries are the index. They were deduped against a last_text
            // that only ever saw frames, so replaying through a fresh one
            // reproduces `enter`'s postings exactly.
            let mut last = BTreeMap::new();
            for (t_ms, window, text) in std::mem::take(&mut self.frames_pending) {
                enter(&mut self.index, &mut last, t_ms, window, text);
            }
        }
        self.index.span = if self.lo == u64::MAX { (0, 0) } else { (self.lo, self.hi) };
        self.index
    }
}

/// Add one transcript entry and its postings, if the text actually changed.
///
/// Only a *change* earns an entry: most frames in a typing session repeat
/// most of their text, and a transcript that recorded every one would be the
/// frames again in a more expensive spelling.
fn enter(
    index: &mut Index,
    last_text: &mut BTreeMap<u32, String>,
    t_ms: u64,
    window: u32,
    text: String,
) {
    if last_text.get(&window) == Some(&text) {
        return;
    }
    last_text.insert(window, text.clone());
    let offset = index.transcript.len() as u32;
    for token in tokenize(&text) {
        index.bloom.insert(&token);
        let slots = index.postings.entry(token).or_default();
        if slots.last() != Some(&offset) {
            slots.push(offset);
        }
    }
    index.transcript.push(TranscriptEntry { t_ms, window, text });
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub t_ms: u64,
    pub window: u32,
    pub title: String,
    pub text: String,
}

impl Index {
    /// Entries containing **every** token in the query (AND, not OR — a
    /// two-word search should narrow, not widen).
    pub fn search(&self, query: &str) -> Vec<Hit> {
        let tokens = tokenize(query);
        if tokens.is_empty() {
            return Vec::new();
        }
        // Cheap rejection first: if the bloom says no, nothing else runs.
        if !tokens.iter().all(|t| self.bloom.maybe_contains(t)) {
            return Vec::new();
        }
        let mut candidates: Option<Vec<u32>> = None;
        for token in &tokens {
            let Some(slots) = self.postings.get(token) else {
                return Vec::new();
            };
            candidates = Some(match candidates {
                None => slots.clone(),
                Some(prev) => prev.iter().copied().filter(|o| slots.contains(o)).collect(),
            });
        }
        candidates
            .unwrap_or_default()
            .into_iter()
            .filter_map(|offset| self.transcript.get(offset as usize))
            .map(|e| Hit {
                t_ms: e.t_ms,
                window: e.window,
                title: self.titles.get(&e.window).cloned().unwrap_or_default(),
                text: e.text.clone(),
            })
            .collect()
    }

    /// The transcript over a time range — the agent's read path. Returns
    /// text in time order, which is the shape an LLM wants: a diary, not a
    /// frame dump.
    pub fn range(&self, from_ms: u64, to_ms: u64) -> Vec<&TranscriptEntry> {
        self.transcript.iter().filter(|e| e.t_ms >= from_ms && e.t_ms <= to_ms).collect()
    }

    /// The last `n` transcript entries — the standing agent tail
    /// (specs/history.md decision 9).
    pub fn tail(&self, n: usize) -> &[TranscriptEntry] {
        let start = self.transcript.len().saturating_sub(n);
        &self.transcript[start..]
    }

    /// Serialize for the segment's seal region.
    ///
    /// The postings are deliberately *not* stored: they derive from the
    /// transcript by the same tokenization that built them, so storing them
    /// would double the blob to say nothing new — and a divergence between
    /// stored postings and stored transcript would be a corruption class
    /// that cannot exist if only one of them is on disk.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.span.0.to_be_bytes());
        out.extend_from_slice(&self.span.1.to_be_bytes());
        out.extend_from_slice(&(self.titles.len() as u32).to_be_bytes());
        for (id, title) in &self.titles {
            out.extend_from_slice(&id.to_be_bytes());
            out.extend_from_slice(&(title.len() as u32).to_be_bytes());
            out.extend_from_slice(title.as_bytes());
        }
        out.extend_from_slice(&(self.transcript.len() as u32).to_be_bytes());
        for e in &self.transcript {
            out.extend_from_slice(&e.t_ms.to_be_bytes());
            out.extend_from_slice(&e.window.to_be_bytes());
            out.extend_from_slice(&(e.text.len() as u32).to_be_bytes());
            out.extend_from_slice(e.text.as_bytes());
        }
        let bloom = self.bloom.to_bytes();
        out.extend_from_slice(&(bloom.len() as u32).to_be_bytes());
        out.extend_from_slice(&bloom);
        out
    }

    /// Rebuild from [`Index::to_bytes`]. `None` on anything malformed — a
    /// corrupt stored index means "rebuild from the events", never "trust
    /// it"; the log stays the source of truth.
    pub fn from_bytes(tier: Tier, bytes: &[u8]) -> Option<Index> {
        struct R<'a>(&'a [u8], usize);
        impl<'a> R<'a> {
            fn take(&mut self, n: usize) -> Option<&'a [u8]> {
                let end = self.1.checked_add(n)?;
                let s = self.0.get(self.1..end)?;
                self.1 = end;
                Some(s)
            }
            fn u32(&mut self) -> Option<u32> {
                Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
            }
            fn u64(&mut self) -> Option<u64> {
                Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
            }
            fn string(&mut self) -> Option<String> {
                let n = self.u32()? as usize;
                String::from_utf8(self.take(n)?.to_vec()).ok()
            }
        }
        let mut r = R(bytes, 0);
        let mut index = Index { tier, ..Index::default() };
        index.span = (r.u64()?, r.u64()?);
        let titles = r.u32()?;
        for _ in 0..titles {
            let id = r.u32()?;
            index.titles.insert(id, r.string()?);
        }
        let entries = r.u32()?;
        for _ in 0..entries {
            let t_ms = r.u64()?;
            let window = r.u32()?;
            let text = r.string()?;
            // Postings rebuilt exactly as `enter` built them: per entry, per
            // unique token. The transcript arrives already change-deduped, so
            // this reproduces the built index token for token.
            let offset = index.transcript.len() as u32;
            for token in tokenize(&text) {
                let slots = index.postings.entry(token).or_default();
                if slots.last() != Some(&offset) {
                    slots.push(offset);
                }
            }
            index.transcript.push(TranscriptEntry { t_ms, window, text });
        }
        let bloom_len = r.u32()? as usize;
        index.bloom = Bloom::from_bytes(r.take(bloom_len)?)?;
        // Trailing garbage is malformation, not padding.
        (r.1 == bytes.len()).then_some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{T0_ROUTINE, T2_SEALED, WindowState};
    use rill_draw::{Color, DrawCommand, Rect, stream};

    fn text_frame(id: u32, dt: u32, tier: Tier, body: &str) -> Stamped {
        let cmds = vec![DrawCommand::Text {
            rect: Rect { x: 0.0, y: 0.0, w: 100.0, h: 20.0 },
            text: body.to_string(),
            color: Color { r: 0, g: 0, b: 0, a: 255 },
            font_size: 14.0,
            font_weight: 400,
            font_family: "sans-serif".into(),
        }];
        Stamped {
            dt_ms: dt,
            tier,
            event: Event::Frame { id, bytes: stream::encode(&cmds).unwrap() },
        }
    }

    /// The same content as `text_frame`, recorded the way a writer that keeps
    /// its own transcript records it.
    fn text_event(id: u32, dt: u32, tier: Tier, body: &str) -> Stamped {
        Stamped { dt_ms: dt, tier, event: Event::Text { id, text: body.to_string() } }
    }

    /// What retention leaves behind: transcript entries, and no frames.
    fn without_frames(events: &[Stamped]) -> Vec<Stamped> {
        events
            .iter()
            .filter(|s| !matches!(s.event, Event::Frame { .. }))
            .cloned()
            .collect()
    }

    /// The property decision 3 rests on: frame chunks age out at 90 days and
    /// transcripts are kept indefinitely. That is only possible if the
    /// transcript is *stored*. While it was recomputed from the frames, this
    /// test would have found an empty index — deleting the frames deleted the
    /// searchable history along with them, and the 30x saving could not be
    /// taken without losing what it was taken to keep.
    #[test]
    fn a_transcript_outlives_the_frames_it_came_from() {
        let events = vec![
            window(1, "notes"),
            text_event(1, 10, T0_ROUTINE, "the tls handshake failed"),
            text_frame(1, 0, T0_ROUTINE, "the tls handshake failed"),
            text_event(1, 20, T0_ROUTINE, "retry succeeded"),
            text_frame(1, 0, T0_ROUTINE, "retry succeeded"),
        ];

        let whole = build(&events, T0_ROUTINE);
        assert_eq!(whole.transcript.len(), 2, "one entry per change, not per frame");
        assert_eq!(whole.search("handshake").len(), 1);

        // Age the frames out. Everything a search needs must survive.
        let aged = build(&without_frames(&events), T0_ROUTINE);
        assert_eq!(aged.transcript.len(), 2, "the transcript went with the frames");
        assert_eq!(aged.search("handshake").len(), 1, "search died with the frames");
        assert_eq!(aged.search("retry").len(), 1);
        assert!(aged.bloom.maybe_contains("handshake"), "the corpus filter lost the word");
        assert_eq!(whole.transcript, aged.transcript, "dropping frames changed the transcript");
    }

    /// The streaming builder's frame buffer must not poison the Text
    /// path's dedup. A frame arrives carrying the same text a Text event
    /// later confirms: the frame entry is discarded when the transcript
    /// proves itself, and the Text entry must still land — a shared dedup
    /// map would swallow it and the transcript would be empty.
    #[test]
    fn a_frame_seen_before_an_identical_text_does_not_swallow_it() {
        let evs = vec![
            window(1, "notes"),
            text_frame(1, 10, T0_ROUTINE, "the same words"),
            text_event(1, 5, T0_ROUTINE, "the same words"),
        ];
        let idx = build(&evs, T0_ROUTINE);
        assert_eq!(idx.transcript.len(), 1, "the Text entry must survive");
        assert_eq!(idx.transcript[0].t_ms, 15, "and it is the Text, not the frame");
        assert_eq!(idx.search("words").len(), 1);
    }

    /// A segment written before transcripts were stored still answers, by
    /// decoding its frames — and a segment carrying both must not enter every
    /// line twice.
    #[test]
    fn frames_are_the_fallback_and_never_a_duplicate() {
        let old = vec![window(1, "notes"), text_frame(1, 10, T0_ROUTINE, "only in a frame")];
        let idx = build(&old, T0_ROUTINE);
        assert_eq!(idx.transcript.len(), 1, "an old segment must still index");
        assert_eq!(idx.search("frame").len(), 1);

        let both = vec![
            window(1, "notes"),
            text_event(1, 10, T0_ROUTINE, "said once"),
            text_frame(1, 0, T0_ROUTINE, "said once"),
        ];
        assert_eq!(build(&both, T0_ROUTINE).transcript.len(), 1, "entered twice");
    }

    fn window(id: u32, title: &str) -> Stamped {
        Stamped {
            dt_ms: 0,
            tier: T0_ROUTINE,
            event: Event::Window(WindowState {
                id,
                x: 0,
                y: 0,
                w: 10,
                h: 10,
                title: title.into(),
                app: "notes".into(),
                vector: true,
                tier: T0_ROUTINE,
            }),
        }
    }

    #[test]
    fn extracts_a_transcript_and_finds_it_again() {
        let events = vec![
            window(1, "Notes — draft"),
            text_frame(1, 100, T0_ROUTINE, "the TLS handshake fails when the cert is stale"),
            text_frame(1, 900, T0_ROUTINE, "cargo test -p rill-auth"),
        ];
        let idx = build(&events, T0_ROUTINE);
        assert_eq!(idx.transcript.len(), 2);

        let hits = idx.search("tls handshake");
        assert_eq!(hits.len(), 1, "both tokens must match the same entry");
        assert_eq!(hits[0].title, "Notes — draft");
        assert_eq!(hits[0].t_ms, 100);
        assert!(hits[0].text.contains("TLS"));

        // AND, not OR: a token that appears in a *different* entry does not
        // widen the result.
        assert!(idx.search("tls cargo").is_empty());
        assert!(idx.search("nonexistent").is_empty());
    }

    /// Repeated text costs nothing — the property that keeps a typing
    /// session's transcript small.
    #[test]
    fn only_changes_earn_a_transcript_entry() {
        let events = vec![
            text_frame(1, 0, T0_ROUTINE, "same"),
            text_frame(1, 10, T0_ROUTINE, "same"),
            text_frame(1, 10, T0_ROUTINE, "same"),
            text_frame(1, 10, T0_ROUTINE, "different"),
        ];
        let idx = build(&events, T0_ROUTINE);
        assert_eq!(idx.transcript.len(), 2);
    }

    /// Two windows showing the same text are separate streams, not a
    /// collision — dedup is per window.
    #[test]
    fn windows_are_deduped_independently() {
        let events = vec![
            text_frame(1, 0, T0_ROUTINE, "hello"),
            text_frame(2, 5, T0_ROUTINE, "hello"),
        ];
        let idx = build(&events, T0_ROUTINE);
        assert_eq!(idx.transcript.len(), 2);
    }

    /// The tier boundary, at the layer that enforces it for search: text
    /// recorded at T2 must never appear in the T0 index, or a search made
    /// while sealed content is locked would leak it.
    #[test]
    fn a_sealed_frame_never_enters_the_routine_index() {
        let events = vec![
            text_frame(1, 0, T0_ROUTINE, "grocery list"),
            text_frame(2, 5, T2_SEALED, "recovery phrase correct horse"),
        ];
        let routine = build(&events, T0_ROUTINE);
        assert_eq!(routine.transcript.len(), 1);
        assert!(routine.search("recovery").is_empty(), "sealed text leaked into T0");
        assert!(!routine.bloom.maybe_contains("horse"), "sealed token leaked into the bloom");

        // And it is present in its own tier's index, for a holder of that key.
        let sealed = build(&events, T2_SEALED);
        assert_eq!(sealed.search("recovery").len(), 1);
    }

    #[test]
    fn bloom_never_reports_a_false_negative() {
        let mut b = Bloom::new(64);
        let present: Vec<String> = (0..500).map(|i| format!("token{i}")).collect();
        for t in &present {
            b.insert(t);
        }
        for t in &present {
            assert!(b.maybe_contains(t), "false negative on {t}");
        }
        // False positives are allowed but should not be everything.
        let absent = (0..500).filter(|i| !b.maybe_contains(&format!("absent{i}"))).count();
        assert!(absent > 100, "bloom is saturated: only {absent}/500 rejected");
    }

    #[test]
    fn tokenizer_splits_on_punctuation_and_lowercases() {
        assert_eq!(tokenize("Rill-Auth: TLS_1.3!"), vec!["rill", "auth", "tls", "1", "3"]);
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn tail_and_range_serve_the_agent_paths() {
        let events: Vec<Stamped> =
            (0..10).map(|i| text_frame(1, 100, T0_ROUTINE, &format!("line {i}"))).collect();
        let idx = build(&events, T0_ROUTINE);
        assert_eq!(idx.tail(3).len(), 3);
        assert_eq!(idx.tail(3)[2].text, "line 9");
        assert_eq!(idx.tail(50).len(), 10, "tail longer than the log is the whole log");
        let mid = idx.range(300, 500);
        assert_eq!(mid.len(), 3, "range is inclusive at both ends");
    }

    #[test]
    fn an_undecodable_frame_is_skipped_not_fatal() {
        let events = vec![
            Stamped {
                dt_ms: 0,
                tier: T0_ROUTINE,
                event: Event::Frame { id: 1, bytes: vec![0xFF; 16] },
            },
            text_frame(1, 5, T0_ROUTINE, "still indexed"),
        ];
        let idx = build(&events, T0_ROUTINE);
        assert_eq!(idx.transcript.len(), 1);
        assert_eq!(idx.search("indexed").len(), 1);
    }

    #[test]
    fn span_covers_the_indexed_tier() {
        let events = vec![
            text_frame(1, 50, T0_ROUTINE, "first"),
            text_frame(1, 250, T0_ROUTINE, "second"),
        ];
        let idx = build(&events, T0_ROUTINE);
        assert_eq!(idx.span, (50, 300));
    }
}
