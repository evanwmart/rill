//! The corpus: a directory of `.rhs` segments, and search across all of
//! them (specs/history.md).
//!
//! The scaling trick, in one sentence: **a search reads the manifest
//! before it opens a single segment.** Each segment contributes one row —
//! its time span, event count, tiers present, and a Bloom filter over its
//! vocabulary. A year is a few thousand rows; testing them all costs
//! microseconds, and only the segments that *could* match are opened.
//!
//! ```text
//! grep "tls handshake"
//!   1. bloom-test every manifest row          ~µs each, all in memory
//!   2. open only the candidates               ~ms each
//!   3. build/consult their indexes, intersect
//! ```
//!
//! The manifest is a **cache, not a source of truth**. It is rebuilt from
//! the segments whenever it is missing, stale, or unreadable — losing it
//! costs time, never history. That is the same posture as the index: the
//! log is the only thing that must survive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::crypt::Kek;
use crate::event::Tier;
use crate::index::{self, Bloom, Index};
use crate::segment::{self, SegmentError};

/// One segment as the manifest knows it — enough to decide whether opening
/// it is worthwhile, and nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentInfo {
    pub path: PathBuf,
    /// File size, used with `mtime` to notice a segment that changed under
    /// a stale manifest row.
    pub size: u64,
    pub mtime_ms: u64,
    /// Wall-clock start, from the segment header.
    pub wall_start_ms: u64,
    /// Span within the segment, in ms from its start.
    pub span: (u64, u64),
    pub events: u64,
    pub tiers: Vec<Tier>,
    /// Vocabulary filter — the thing a search actually consults first.
    pub bloom: Bloom,
    /// Whether the segment is encrypted. Never persisted: an encrypted
    /// segment's row is rebuilt from the segment on every open, because a
    /// plaintext manifest caching its bloom would put the vocabulary of
    /// encrypted content on disk unencrypted — a membership oracle ("did
    /// this machine ever show word X") that undoes what the encryption
    /// bought. Found live, not in review: the first keyless search opened
    /// 0/1 segments instead of saying "locked", and the cached row was why.
    pub encrypted: bool,
}

impl SegmentInfo {
    /// Wall-clock range this segment covers.
    pub fn wall_range(&self) -> (u64, u64) {
        (self.wall_start_ms + self.span.0, self.wall_start_ms + self.span.1)
    }
}

/// A hit, located in the corpus rather than in one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusHit {
    pub segment: PathBuf,
    /// Milliseconds since the unix epoch — the timestamp a user sees.
    pub wall_ms: u64,
    /// Offset within the segment, which is what a replay seeks to.
    pub t_ms: u64,
    pub window: u32,
    pub title: String,
    pub text: String,
}

/// A directory of segments.
pub struct Corpus {
    root: PathBuf,
    segments: Vec<SegmentInfo>,
    /// The unlock for encrypted segments. `None` reads plaintext history
    /// and reports encrypted segments locked rather than empty.
    kek: Option<Kek>,
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Corpus {
    /// Open a corpus, loading the manifest and reconciling it against what
    /// is actually on disk. Segments that are new, resized or rewritten are
    /// re-scanned; the rest are taken from the manifest.
    pub fn open(root: &Path) -> Result<Corpus, SegmentError> {
        Corpus::open_with(root, None)
    }

    pub fn open_with(root: &Path, kek: Option<Kek>) -> Result<Corpus, SegmentError> {
        let mut cached: BTreeMap<PathBuf, SegmentInfo> = manifest_load(&manifest_path(root))
            .unwrap_or_default()
            .into_iter()
            .map(|i| (i.path.clone(), i))
            .collect();

        let mut segments = Vec::new();
        let mut changed = false;
        if root.is_dir() {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(root)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "rhs"))
                .collect();
            paths.sort();
            for path in paths {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let mtime = mtime_ms(&path);
                match cached.remove(&path) {
                    // A row is only trusted while the file it describes is
                    // untouched — an active segment grows, and its row must
                    // grow with it.
                    Some(info) if info.size == size && info.mtime_ms == mtime => {
                        segments.push(info)
                    }
                    _ => {
                        changed = true;
                        match scan(&path, kek.as_ref()) {
                            Ok(info) => segments.push(info),
                            // A segment we cannot read at all is skipped, not
                            // fatal: one bad file must not hide the corpus.
                            Err(_) => continue,
                        }
                    }
                }
            }
        }
        // Anything left in the cache no longer exists on disk.
        changed |= !cached.is_empty();
        if changed {
            let _ = manifest_save(&manifest_path(root), &segments);
        }
        Ok(Corpus { root: root.to_path_buf(), segments, kek })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn segments(&self) -> &[SegmentInfo] {
        &self.segments
    }

    pub fn total_events(&self) -> u64 {
        self.segments.iter().map(|s| s.events).sum()
    }

    /// Search the whole corpus, newest first, stopping at `limit` hits.
    ///
    /// Segments whose Bloom filter rejects any query token are never
    /// opened — the property that keeps this cheap as history grows.
    /// Returns the hits and how many segments were actually read, so
    /// callers can show the selectivity honestly.
    pub fn search(&self, query: &str, tier: Tier, limit: usize) -> (Vec<CorpusHit>, usize) {
        let tokens = index::tokenize(query);
        if tokens.is_empty() {
            return (Vec::new(), 0);
        }
        let mut hits = Vec::new();
        let mut opened = 0usize;
        // Newest first: recent history is what people look for.
        for info in self.segments.iter().rev() {
            if hits.len() >= limit {
                break;
            }
            if !info.tiers.contains(&tier) {
                continue;
            }
            if !tokens.iter().all(|t| info.bloom.maybe_contains(t)) {
                continue;
            }
            let Ok(idx) = self.index_of(&info.path, tier) else { continue };
            opened += 1;
            for h in idx.search(query) {
                hits.push(CorpusHit {
                    segment: info.path.clone(),
                    wall_ms: info.wall_start_ms + h.t_ms,
                    t_ms: h.t_ms,
                    window: h.window,
                    title: h.title,
                    text: h.text,
                });
                if hits.len() >= limit {
                    break;
                }
            }
        }
        (hits, opened)
    }

    /// The most recent transcript across the corpus — the agent's standing
    /// tail (specs/history.md decision 9), assembled newest-segment-first
    /// and returned in time order.
    pub fn tail(&self, n: usize, tier: Tier) -> Vec<CorpusHit> {
        let mut out: Vec<CorpusHit> = Vec::new();
        for info in self.segments.iter().rev() {
            // Newest first, so once `n` is satisfied there is nothing older
            // worth opening — `continue` here kept walking (and stat-ing) the
            // whole corpus to decide that repeatedly.
            if out.len() >= n {
                break;
            }
            if !info.tiers.contains(&tier) {
                continue;
            }
            let Ok(idx) = self.index_of(&info.path, tier) else { continue };
            for e in idx.tail(n - out.len()) {
                out.push(CorpusHit {
                    segment: info.path.clone(),
                    wall_ms: info.wall_start_ms + e.t_ms,
                    t_ms: e.t_ms,
                    window: e.window,
                    title: idx.titles.get(&e.window).cloned().unwrap_or_default(),
                    text: e.text.clone(),
                });
            }
        }
        out.sort_by_key(|h| h.wall_ms);
        out
    }

    /// The index for one segment and tier: the stored one when the segment
    /// is sealed, a rebuild from the events when it is not (or when the tier
    /// produced no stored index). The log stays the source of truth either
    /// way — a stored index is a skip of the rebuild, never a different
    /// answer, and the seal's region hash is what makes the skip safe to
    /// trust.
    pub fn index_of(&self, path: &Path, tier: Tier) -> Result<Index, SegmentError> {
        if let Ok(Some((_, seal))) = segment::read_seal_with(path, self.kek.as_ref())
            && let Some(idx) = seal.indexes.into_iter().find(|i| i.tier == tier)
        {
            return Ok(idx);
        }
        let seg = segment::read_with(path, self.kek.as_ref())?;
        Ok(index::build(&seg.events, tier))
    }
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("MANIFEST")
}

/// Scan one segment into a manifest row.
fn scan(path: &Path, kek: Option<&Kek>) -> Result<SegmentInfo, SegmentError> {
    // Sealed fast path: the footer already states everything a manifest row
    // holds, so a sealed segment costs two small reads instead of decoding
    // every chunk — which is most of what makes rescanning a year of
    // history cheap.
    if let Some((header, seal)) = segment::read_seal_with(path, kek)? {
        let encrypted = !header.keyslots.is_empty();
        let mut bloom = Bloom::default();
        for idx in &seal.indexes {
            // The row bloom covers every tier's vocabulary, same as the
            // rebuilt path below: it exists to *reject* segments cheaply,
            // holds no text, and a hit still requires the tier's own index.
            bloom.union(&idx.bloom);
        }
        return Ok(SegmentInfo {
            path: path.to_path_buf(),
            size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            mtime_ms: mtime_ms(path),
            wall_start_ms: header.wall_start_ms,
            span: seal.span,
            events: seal.events,
            tiers: seal.tiers,
            bloom,
            encrypted,
        });
    }
    let seg = segment::read_with(path, kek)?;
    // The bloom covers every tier's vocabulary: it exists to *reject*
    // segments cheaply, and a shared filter cannot leak text (it holds no
    // text, only set membership, and a hit still requires the tier's own
    // index to produce anything).
    let mut bloom = Bloom::default();
    let mut span = (u64::MAX, 0u64);
    for (t_ms, s) in segment::absolute_times(&seg.events) {
        span.0 = span.0.min(t_ms);
        span.1 = span.1.max(t_ms);
        if let crate::event::Event::Frame { bytes, .. } = &s.event
            && let Ok(cmds) = rill_draw::stream::decode(bytes)
        {
            for c in &cmds {
                if let rill_draw::DrawCommand::Text { text, .. } = c {
                    for token in index::tokenize(text) {
                        bloom.insert(&token);
                    }
                }
            }
        }
    }
    if span.0 == u64::MAX {
        span = (0, 0);
    }
    Ok(SegmentInfo {
        path: path.to_path_buf(),
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        mtime_ms: mtime_ms(path),
        wall_start_ms: seg.header.wall_start_ms,
        span,
        events: seg.events.len() as u64,
        tiers: segment::tiers_present(&seg.events),
        bloom,
        encrypted: !seg.header.keyslots.is_empty(),
    })
}

// ------------------------------------------------------- manifest encoding

// v2: encrypted segments' rows are never persisted (their blooms would be a
// plaintext vocabulary oracle over encrypted content). The bump invalidates
// every v1 manifest, which is the point — some carried exactly those rows.
const MANIFEST_MAGIC: [u8; 4] = *b"RHM\x02";

fn put_u32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_be_bytes());
}
fn put_u64(o: &mut Vec<u8>, v: u64) {
    o.extend_from_slice(&v.to_be_bytes());
}
fn put_bytes(o: &mut Vec<u8>, b: &[u8]) {
    put_u32(o, b.len() as u32);
    o.extend_from_slice(b);
}

fn manifest_save(path: &Path, segments: &[SegmentInfo]) -> std::io::Result<()> {
    // Encrypted segments are rescanned per open instead of cached — two
    // small reads apiece against putting their vocabulary on disk in the
    // clear (see `SegmentInfo::encrypted`).
    let segments: Vec<&SegmentInfo> = segments.iter().filter(|s| !s.encrypted).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&MANIFEST_MAGIC);
    put_u32(&mut out, segments.len() as u32);
    for s in segments {
        put_bytes(&mut out, s.path.to_string_lossy().as_bytes());
        put_u64(&mut out, s.size);
        put_u64(&mut out, s.mtime_ms);
        put_u64(&mut out, s.wall_start_ms);
        put_u64(&mut out, s.span.0);
        put_u64(&mut out, s.span.1);
        put_u64(&mut out, s.events);
        put_bytes(&mut out, &s.tiers);
        put_bytes(&mut out, &s.bloom.to_bytes());
    }
    // Written atomically: a half-written manifest would be discarded on the
    // next open anyway, but a torn one must never be *readable*.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path)
}

fn manifest_load(path: &Path) -> Option<Vec<SegmentInfo>> {
    let bytes = std::fs::read(path).ok()?;
    let mut c = ManCursor { b: &bytes, p: 0 };
    if c.take(4)? != MANIFEST_MAGIC {
        return None;
    }
    let n = c.u32()? as usize;
    if n > 1_000_000 {
        return None;
    }
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let path = PathBuf::from(String::from_utf8(c.bytes()?.to_vec()).ok()?);
        let info = SegmentInfo {
            path,
            size: c.u64()?,
            mtime_ms: c.u64()?,
            wall_start_ms: c.u64()?,
            span: (c.u64()?, c.u64()?),
            events: c.u64()?,
            tiers: c.bytes()?.to_vec(),
            bloom: Bloom::from_bytes(c.bytes()?)?,
            // By construction: encrypted rows are never saved (see
            // manifest_save), so anything loaded is plaintext history.
            encrypted: false,
        };
        out.push(info);
    }
    Some(out)
}

struct ManCursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> ManCursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.b.get(self.p..end)?;
        self.p = end;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Stamped, T0_ROUTINE, T2_SEALED};
    use crate::segment::{ChunkCodec, Header, SegmentWriter};
    use rill_draw::{Color, DrawCommand, Rect, stream};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rhs-corpus-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_segment(dir: &Path, name: &str, wall: u64, texts: &[(&str, Tier)]) -> PathBuf {
        let path = dir.join(name);
        let header = Header {
            version: 1,
            device: "test".into(),
            wall_start_ms: wall,
            keyslots: Vec::new(),
        };
        let mut w = SegmentWriter::create(&path, &header, ChunkCodec::Zstd, 3).unwrap();
        for (i, (text, tier)) in texts.iter().enumerate() {
            let cmds = vec![DrawCommand::Text {
                rect: Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 },
                text: text.to_string(),
                color: Color { r: 0, g: 0, b: 0, a: 255 },
                font_size: 12.0,
                font_weight: 400,
                font_family: "sans-serif".into(),
            }];
            w.append(&Stamped {
                dt_ms: if i == 0 { 0 } else { 100 },
                tier: *tier,
                event: Event::Frame { id: 1, bytes: stream::encode(&cmds).unwrap() },
            })
            .unwrap();
        }
        w.finish().unwrap();
        path
    }

    #[test]
    fn searches_across_segments_newest_first() {
        let d = dir("search");
        write_segment(&d, "0001.rhs", 1_000_000, &[("the tls handshake failed", T0_ROUTINE)]);
        write_segment(&d, "0002.rhs", 2_000_000, &[("cargo test passed", T0_ROUTINE)]);
        write_segment(&d, "0003.rhs", 3_000_000, &[("another tls note", T0_ROUTINE)]);

        let c = Corpus::open(&d).unwrap();
        assert_eq!(c.segments().len(), 3);

        let (hits, opened) = c.search("tls", T0_ROUTINE, 10);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].wall_ms > hits[1].wall_ms, "newest first");
        // The bloom must have spared the segment that cannot match.
        assert_eq!(opened, 2, "only candidate segments should be opened");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The corpus-scale property: a query for a token nobody wrote opens
    /// nothing at all.
    #[test]
    fn a_miss_opens_no_segments() {
        let d = dir("miss");
        for i in 0..5 {
            write_segment(&d, &format!("{i:04}.rhs"), 1_000_000, &[("routine text", T0_ROUTINE)]);
        }
        let c = Corpus::open(&d).unwrap();
        let (hits, opened) = c.search("zzzznotpresent", T0_ROUTINE, 10);
        assert!(hits.is_empty());
        assert_eq!(opened, 0, "bloom filters must reject without opening");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_manifest_is_a_cache_that_survives_a_round_trip() {
        let d = dir("manifest");
        write_segment(&d, "0001.rhs", 5_000, &[("hello world", T0_ROUTINE)]);
        let first = Corpus::open(&d).unwrap();
        assert!(manifest_path(&d).exists(), "opening writes a manifest");

        let second = Corpus::open(&d).unwrap();
        assert_eq!(first.segments(), second.segments(), "cached rows must match a fresh scan");

        // Losing it costs time, never history.
        std::fs::remove_file(manifest_path(&d)).unwrap();
        let third = Corpus::open(&d).unwrap();
        assert_eq!(first.segments(), third.segments());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A segment that grew since the manifest row was written must be
    /// re-scanned, or an active segment's new events would be invisible.
    #[test]
    fn a_changed_segment_is_rescanned() {
        let d = dir("changed");
        let path = write_segment(&d, "0001.rhs", 1_000, &[("first note", T0_ROUTINE)]);
        let before = Corpus::open(&d).unwrap().segments()[0].events;

        // Append a second segment's worth of content by rewriting the file
        // larger (same name), the way an active segment grows.
        write_segment(&d, "0001.rhs", 1_000, &[("first note", T0_ROUTINE), ("second", T0_ROUTINE)]);
        let after = Corpus::open(&d).unwrap().segments()[0].events;
        assert!(after > before, "a grown segment must be re-scanned ({before} → {after})");
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Tier isolation at corpus scale: a T0 search never returns sealed
    /// text, even though the shared bloom knows the token exists.
    #[test]
    fn sealed_text_is_not_searchable_from_the_routine_tier() {
        let d = dir("tiers");
        write_segment(&d, "0001.rhs", 1_000, &[("recovery phrase here", T2_SEALED)]);
        let c = Corpus::open(&d).unwrap();
        let (hits, _) = c.search("recovery", T0_ROUTINE, 10);
        assert!(hits.is_empty(), "sealed text reachable from T0");
        let (sealed, _) = c.search("recovery", T2_SEALED, 10);
        assert_eq!(sealed.len(), 1, "and reachable with the right tier");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_or_missing_directory_is_an_empty_corpus() {
        let missing = std::env::temp_dir().join("rhs-corpus-does-not-exist");
        let _ = std::fs::remove_dir_all(&missing);
        let c = Corpus::open(&missing).unwrap();
        assert!(c.segments().is_empty());
        assert_eq!(c.total_events(), 0);
    }

    #[test]
    fn a_corrupt_manifest_is_ignored_rather_than_fatal() {
        let d = dir("corrupt-manifest");
        write_segment(&d, "0001.rhs", 1_000, &[("text", T0_ROUTINE)]);
        std::fs::write(manifest_path(&d), b"garbage").unwrap();
        let c = Corpus::open(&d).unwrap();
        assert_eq!(c.segments().len(), 1, "a bad manifest must rebuild, not fail");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tail_returns_recent_transcript_in_time_order() {
        let d = dir("tail");
        write_segment(&d, "0001.rhs", 1_000, &[("older line", T0_ROUTINE)]);
        write_segment(&d, "0002.rhs", 9_000, &[("newer line", T0_ROUTINE), ("newest", T0_ROUTINE)]);
        let c = Corpus::open(&d).unwrap();
        let tail = c.tail(2, T0_ROUTINE);
        assert_eq!(tail.len(), 2);
        assert!(tail[0].wall_ms <= tail[1].wall_ms, "time order");
        assert_eq!(tail[1].text, "newest");
        let _ = std::fs::remove_dir_all(&d);
    }
}
