//! Retention: fidelity decay and forgetting (specs/history.md decision 3).
//!
//! ```text
//! transcripts   kept indefinitely     (search + agent memory never expire)
//! frames        rolling window        (replayable recent past, default 90d)
//! pinned        kept whole, forever   (explicit user act, sidecar marker)
//! hard delete   first-class operation (policy/legal, not disk pressure)
//! ```
//!
//! Aging and deletion are one mechanism worn two ways: rewrite a sealed
//! segment keeping only some of its events. It has to be an *event-level*
//! rewrite — the tempting chunk-verbatim copy is wrong, because every event
//! carries a delta from its predecessor, and dropping a chunk would silently
//! shift every later timestamp earlier by the span it covered. The rewrite
//! re-anchors each kept event at its original absolute time, so what
//! survives says exactly when it always said.
//!
//! Rewrites go through a temp file and a rename: a crash mid-age leaves the
//! original untouched and a `.tmp` the next attempt overwrites. The result
//! is sealed like anything else (`finish` seals), so an aged segment keeps
//! the wholeness promise — with a new footer, which is the "rewrites only
//! the footer" the spec asked for, plus the re-chunking honesty demands.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crypt::Kek;
use crate::event::{Event, Stamped};
use crate::segment::{
    ChunkCodec, SegmentError, SegmentWriter, absolute_times, read_seal, read_with,
};

/// What a rewrite did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    pub events_before: usize,
    pub events_after: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// The default frame window: how long pixel-perfect replay is kept.
pub const DEFAULT_FRAME_DAYS: u64 = 90;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The pin marker beside a segment. A sidecar file rather than a bit inside
/// the segment, so pinning never rewrites what it exists to protect, and so
/// `ls` shows the policy as plainly as the CLI does.
pub fn pin_path(segment: &Path) -> PathBuf {
    let mut s = segment.as_os_str().to_os_string();
    s.push(".pin");
    PathBuf::from(s)
}

pub fn is_pinned(segment: &Path) -> bool {
    pin_path(segment).exists()
}

/// Rewrite a sealed segment, keeping the events `keep` accepts.
///
/// `keep` sees each event with its wall-clock time (header base + absolute
/// offset). Kept events are re-anchored so their absolute times are exactly
/// what they were. Refuses unsealed segments: an open segment belongs to a
/// live writer, and a crashed one gets sealed by recovery before anything
/// here should touch it.
pub fn rewrite(
    path: &Path,
    kek: Option<&Kek>,
    mut keep: impl FnMut(u64, &Stamped) -> bool,
) -> Result<Rewrite, SegmentError> {
    let bytes_before = std::fs::metadata(path)?.len();
    let seg = read_with(path, kek)?;
    if seg.seal.is_none() {
        return Err(SegmentError::SealBroken(
            "refusing to rewrite an unsealed segment (seal it first)".into(),
        ));
    }

    let events_before = seg.events.len();
    let base = seg.header.wall_start_ms;
    let mut kept: Vec<Stamped> = Vec::new();
    let mut prev_abs = 0u64;
    for (abs, s) in absolute_times(&seg.events) {
        if !keep(base.saturating_add(abs), s) {
            continue;
        }
        // Re-anchor: this event's delta becomes the distance from the last
        // *kept* event, so its absolute time is unmoved. Saturating, because
        // a delete can open a gap wider than u32 ms (49 days) — the minute
        // Sync events re-anchor wall time on the far side regardless.
        let dt_ms = abs.saturating_sub(prev_abs).min(u32::MAX as u64) as u32;
        prev_abs = abs;
        kept.push(Stamped { dt_ms, tier: s.tier, event: s.event.clone() });
    }
    let events_after = kept.len();

    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // An encrypted segment stays encrypted through a rewrite — under a
    // *fresh* data key, wrapped by the same KEK; the old key dies with the
    // old bytes. The header handed over is stripped of its old keyslots,
    // because create_with_key wraps the new key itself.
    let was_encrypted = !seg.header.keyslots.is_empty();
    let mut header = seg.header.clone();
    header.keyslots.clear();
    let rewrap = if was_encrypted {
        let Some(kek) = kek else {
            return Err(SegmentError::Locked(
                "cannot rewrite an encrypted segment without its key".into(),
            ));
        };
        Some(kek)
    } else {
        None
    };
    let mut w = SegmentWriter::create_with_key(&tmp, &header, ChunkCodec::Zstd, 3, rewrap)?;
    for s in &kept {
        w.append(s)?;
    }
    w.finish()?;
    std::fs::rename(&tmp, path)?;
    let bytes_after = std::fs::metadata(path)?.len();
    Ok(Rewrite { events_before, events_after, bytes_before, bytes_after })
}

/// Age one segment: drop its frames, keep everything else — the transcript,
/// the window story, the clicks. Idempotent (a frameless segment rewrites to
/// itself); the caller decides *whether* (window, pin), this does the how.
pub fn age(path: &Path, kek: Option<&Kek>) -> Result<Rewrite, SegmentError> {
    rewrite(path, kek, |_, s| !matches!(s.event, Event::Frame { .. }))
}

/// One segment as retention sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    /// Wall-clock end of the segment's span.
    pub end_ms: u64,
    pub pinned: bool,
}

/// Sealed segments whose *end* is older than the window — frames past their
/// replay horizon. Pinned segments are listed (so a dry run shows the whole
/// truth) but marked, and [`age_older_than`] skips them.
pub fn age_candidates(dir: &Path, window_days: u64) -> Vec<Candidate> {
    let cutoff = now_ms().saturating_sub(window_days * 24 * 60 * 60 * 1000);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "rhs") {
            continue;
        }
        let Ok(Some((header, seal))) = read_seal(&path) else { continue };
        let end_ms = header.wall_start_ms.saturating_add(seal.span.1);
        if end_ms < cutoff {
            out.push(Candidate { path: path.clone(), end_ms, pinned: is_pinned(&path) });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Age everything past the window, skipping pins. Returns what happened,
/// per segment — the caller prints, this never does.
pub fn age_older_than(
    dir: &Path,
    window_days: u64,
    kek: Option<&Kek>,
) -> Vec<(PathBuf, Result<Rewrite, SegmentError>)> {
    age_candidates(dir, window_days)
        .into_iter()
        .filter(|c| !c.pinned)
        .map(|c| {
            let r = age(&c.path, kek);
            (c.path, r)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{T0_ROUTINE, Tier};
    use crate::segment::{Header, read};

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rhs-retention-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn header(wall_start_ms: u64) -> Header {
        Header { version: 1, device: "test".into(), wall_start_ms, keyslots: Vec::new() }
    }

    fn text(dt: u32, id: u32, body: &str) -> Stamped {
        Stamped { dt_ms: dt, tier: T0_ROUTINE, event: Event::Text { id, text: body.into() } }
    }

    fn frame(dt: u32, n: usize) -> Stamped {
        // Incompressible-ish bytes, or zstd shrinks a constant 200 KB frame
        // to nothing and the size assertions can't see the frames leave.
        let mut x = 0x2545F4914F6CDD1Du64 ^ (n as u64);
        let bytes = (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        Stamped { dt_ms: dt, tier: T0_ROUTINE, event: Event::Frame { id: 1, bytes } }
    }

    fn build(path: &Path, wall: u64, events: &[Stamped]) {
        let mut w = SegmentWriter::create(path, &header(wall), ChunkCodec::Zstd, 3).unwrap();
        for e in events {
            w.append(e).unwrap();
        }
        w.finish().unwrap();
    }

    /// The trade decision 3 is built on: frames go, the transcript stays,
    /// and what survives says exactly when it always said.
    #[test]
    fn aging_drops_the_frames_and_nothing_else_moves() {
        let dir = tmp_dir("age");
        let path = dir.join("a.rhs");
        build(
            &path,
            1_000_000,
            &[
                text(10, 1, "the morning email"),
                frame(5, 200_000),
                text(50, 1, "the afternoon reply"),
                frame(5, 200_000),
                text(30, 2, "a different window"),
            ],
        );
        let before = read(&path).unwrap();
        let before_texts: Vec<(u64, String)> = absolute_times(&before.events)
            .into_iter()
            .filter_map(|(t, s)| match &s.event {
                Event::Text { text, .. } => Some((t, text.clone())),
                _ => None,
            })
            .collect();

        let out = age(&path, None).unwrap();
        assert_eq!(out.events_before, 5);
        assert_eq!(out.events_after, 3);
        assert!(
            out.bytes_after < out.bytes_before / 10,
            "frames were most of the bytes ({} -> {})",
            out.bytes_before,
            out.bytes_after
        );

        let after = read(&path).unwrap();
        assert!(after.seal.is_some(), "an aged segment is still sealed");
        assert!(
            !after.events.iter().any(|s| matches!(s.event, Event::Frame { .. })),
            "a frame survived aging"
        );
        let after_texts: Vec<(u64, String)> = absolute_times(&after.events)
            .into_iter()
            .filter_map(|(t, s)| match &s.event {
                Event::Text { text, .. } => Some((t, text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(before_texts, after_texts, "the transcript moved in time");

        // And the stored index still answers — search does not notice aging.
        let idx = &after.seal.unwrap().indexes[0];
        assert_eq!(idx.search("afternoon reply").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pinned segment is listed so a dry run tells the whole truth, and
    /// skipped so the pin means what it says.
    #[test]
    fn pins_hold_against_the_window() {
        let dir = tmp_dir("pin");
        let old = dir.join("old.rhs");
        let pinned = dir.join("pinned.rhs");
        let fresh = dir.join("fresh.rhs");
        // Long past any window.
        build(&old, 1_000, &[text(1, 1, "old"), frame(1, 50_000)]);
        build(&pinned, 1_000, &[text(1, 1, "pinned"), frame(1, 50_000)]);
        std::fs::write(pin_path(&pinned), b"").unwrap();
        // Now-ish: must not be a candidate.
        build(&fresh, now_ms(), &[text(1, 1, "fresh"), frame(1, 50_000)]);

        let cands = age_candidates(&dir, DEFAULT_FRAME_DAYS);
        let names: Vec<(String, bool)> = cands
            .iter()
            .map(|c| (c.path.file_name().unwrap().to_string_lossy().into_owned(), c.pinned))
            .collect();
        assert_eq!(names, [("old.rhs".to_string(), false), ("pinned.rhs".to_string(), true)]);

        let aged = age_older_than(&dir, DEFAULT_FRAME_DAYS, None);
        assert_eq!(aged.len(), 1, "only the old, unpinned segment ages");
        assert!(aged[0].0.ends_with("old.rhs"));
        assert!(
            read(&pinned)
                .unwrap()
                .events
                .iter()
                .any(|s| matches!(s.event, Event::Frame { .. })),
            "the pinned segment lost its frames"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hard delete by range, mid-segment: what falls inside the range is
    /// gone, what survives keeps its place in time, and the file reseals.
    #[test]
    fn a_range_delete_removes_events_and_preserves_the_timeline() {
        let dir = tmp_dir("del");
        let path = dir.join("d.rhs");
        // Events at absolute ms 10, 20, 30, 40 (wall base 1_000_000).
        build(
            &path,
            1_000_000,
            &[
                text(10, 1, "keep early"),
                text(10, 1, "delete me"),
                text(10, 1, "delete me too"),
                text(10, 1, "keep late"),
            ],
        );
        // Delete wall range covering the middle two (base+15 .. base+35).
        let out = rewrite(&path, None, |wall, _| !(1_000_015..=1_000_035).contains(&wall)).unwrap();
        assert_eq!((out.events_before, out.events_after), (4, 2));

        let after = read(&path).unwrap();
        let texts: Vec<(u64, String)> = absolute_times(&after.events)
            .into_iter()
            .filter_map(|(t, s)| match &s.event {
                Event::Text { text, .. } => Some((t, text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            [(10, "keep early".to_string()), (40, "keep late".to_string())],
            "survivors moved in time or the wrong events died"
        );
        assert!(after.seal.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An open segment belongs to a live writer; retention refuses it.
    #[test]
    fn an_unsealed_segment_is_refused() {
        let dir = tmp_dir("open");
        let path = dir.join("o.rhs");
        let mut w = SegmentWriter::create(&path, &header(1_000), ChunkCodec::Plain, 0).unwrap();
        w.append(&text(1, 1, "live")).unwrap();
        w.flush().unwrap();
        drop(w);
        assert!(matches!(age(&path, None), Err(SegmentError::SealBroken(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Aging twice is aging once.
    #[test]
    fn aging_is_idempotent() {
        let dir = tmp_dir("idem");
        let path = dir.join("i.rhs");
        build(&path, 1_000, &[text(1, 1, "words"), frame(1, 10_000)]);
        age(&path, None).unwrap();
        let once = std::fs::read(&path).unwrap();
        let out = age(&path, None).unwrap();
        assert_eq!(out.events_before, out.events_after);
        // Byte-identical is too strong (sealed_at differs); event-identical
        // and still-sealed is the contract.
        let again = read(&std::path::PathBuf::from(&path)).unwrap();
        assert_eq!(again.events.len(), read_len(&once));
        assert!(again.seal.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_len(bytes: &[u8]) -> usize {
        crate::segment::read_bytes(bytes).unwrap().events.len()
    }

    /// `Tier` is carried through a rewrite untouched — retention must never
    /// be a way to launder a classification.
    #[test]
    fn tiers_survive_a_rewrite() {
        use crate::event::T2_SEALED;
        let dir = tmp_dir("tier");
        let path = dir.join("t.rhs");
        let sealed_text = Stamped {
            dt_ms: 5,
            tier: T2_SEALED as Tier,
            event: Event::Text { id: 9, text: "classified".into() },
        };
        build(&path, 1_000, &[text(1, 1, "plain"), sealed_text, frame(1, 10_000)]);
        age(&path, None).unwrap();
        let after = read(&path).unwrap();
        let tiers: Vec<Tier> = after.events.iter().map(|s| s.tier).collect();
        assert_eq!(tiers, [T0_ROUTINE, T2_SEALED]);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Retention under encryption: an aged segment stays encrypted — under a
    /// fresh data key wrapped by the same KEK — and the plaintext never
    /// touches the disk on the way through.
    #[test]
    fn an_encrypted_segment_ages_and_stays_locked() {
        use crate::crypt::Kek;
        let dir = tmp_dir("enc-age");
        let path = dir.join("e.rhs");
        let kek = Kek::from_bytes([5; 32]);
        let mut w = SegmentWriter::create_with_key(
            &path,
            &header(1_000),
            ChunkCodec::Zstd,
            3,
            Some(&kek),
        )
        .unwrap();
        w.append(&text(1, 1, "the enduring transcript")).unwrap();
        w.append(&frame(1, 50_000)).unwrap();
        w.finish().unwrap();
        let slot_before = crate::segment::read_seal_with(&path, Some(&kek))
            .unwrap()
            .unwrap()
            .0
            .keyslots[0]
            .blob
            .clone();

        // No key, no rewrite.
        assert!(matches!(age(&path, None), Err(SegmentError::Locked(_))));

        let out = age(&path, Some(&kek)).unwrap();
        assert_eq!(out.events_after, 1, "the frame left, the transcript stayed");
        // Still encrypted, and not under the old data key: the wrap changed.
        let after = crate::segment::read_seal_with(&path, Some(&kek)).unwrap().unwrap();
        assert!(!after.0.keyslots.is_empty(), "aging stripped the encryption");
        assert_ne!(after.0.keyslots[0].blob, slot_before, "the data key was reused");
        assert!(matches!(
            crate::segment::read(&path),
            Err(SegmentError::Locked(_))
        ));
        let r = crate::segment::read_with(&path, Some(&kek)).unwrap();
        assert_eq!(r.events.len(), 1);
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(8).any(|w| w == b"enduring"), "plaintext leaked through aging");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
