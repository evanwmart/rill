//! `.rhs` segment files: the durable half of semantic history.
//!
//! ```text
//! header (plaintext)  magic RHS\x01 | format ver | device fp | time base
//!                     | keyslot table | flags
//! chunk*              [len u32 | codec u8 | payload]   payload = zstd(events)
//!                     (AEAD wraps the payload once key plumbing lands; the
//!                      keyslot table and per-chunk framing are already
//!                      shaped for it — see specs/history.md)
//! seal (iff sealed)   per-tier indexes | footer: event count | time range
//!                     | sealed-at | tiers | chunk count | blake3 of
//!                     plaintext | merkle root
//! tail (iff sealed)   [seal_len u32 | SEAL_MAGIC]  — the last eight bytes,
//!                     which is how a reader knows without scanning
//! ```
//!
//! Three properties the shape exists to guarantee:
//!
//! * **Crash-honesty.** Each chunk is length-prefixed and hashed, so a torn
//!   write costs at most the trailing chunk; everything before it decodes.
//!   A segment killed mid-write reads back to its last whole chunk, which
//!   is the same promise `.rillrec` made at event granularity.
//! * **Bounded loss.** Chunks flush on size *or* on an elapsed deadline
//!   measured **from the first unflushed event** — never on a periodic
//!   timer. No events means no deadline, so an idle desktop writes nothing.
//! * **Frames separable.** Frame payloads live in their own chunks, so
//!   retention can drop frame chunks at 90 days and keep the transcript and
//!   index intact by rewriting only the footer (specs/history.md, decision
//!   3).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::crypt::{DataKey, KEYSLOT_DEVICE, Kek};
use crate::event::{Event, EventError, Stamped, Tier, decode_chunk, encode};
use crate::index::Index;

pub const MAGIC: [u8; 4] = *b"RHS\x01";
/// Trailing tail of a sealed segment: `[blake3(seal region) 32B][seal_len
/// u32][SEAL_MAGIC]` are the file's last forty bytes iff it has been sealed.
///
/// The region hash is not redundant with the footer's: the footer hashes the
/// *chunks*, and without a hash over the seal region itself a flipped byte
/// inside a stored index would decode as a different-but-valid index — search
/// results silently rewritten under an intact seal, found by the tamper test
/// on the first run.
pub const SEAL_MAGIC: [u8; 4] = *b"RHSs";
/// Bytes of the sealed tail: region hash + length + magic.
pub const SEAL_TAIL: usize = 40;
/// Cap on the seal region (indexes + footer). The transcript is measured at
/// 3–7% of the frames it came from, so a full 64 MiB segment's seal sits an
/// order of magnitude under this — the cap exists for hostile files, not
/// honest ones.
pub const MAX_SEAL: usize = 32 * 1024 * 1024;
/// Chunk payload cap — a hostile or corrupt length is refused, not allocated.
pub const MAX_CHUNK: usize = 8 * 1024 * 1024;
/// Decoded-size cap for a compressed chunk — the decompression-bomb guard.
///
/// [`MAX_CHUNK`] bounds only the bytes on disk, and zstd expands far past any
/// ratio worth guessing, so a crafted 8 MiB chunk can ask for gigabytes on a
/// machine with one. The writer flushes at [`CHUNK_TARGET`] (256 KiB), so
/// even a pathological *honest* chunk sits two orders of magnitude under this.
///
/// Segments are meant to be shareable, which makes reading one a hostile-input
/// problem and not merely a corruption problem.
pub const MAX_CHUNK_DECODED: u64 = 32 * 1024 * 1024;
/// Flush when the pending buffer reaches this many raw bytes.
pub const CHUNK_TARGET: usize = 256 * 1024;
/// Flush this long after the *first* unflushed event (not a periodic tick).
pub const FLUSH_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
/// Rotate at roughly this many raw bytes.
pub const SEGMENT_TARGET: u64 = 64 * 1024 * 1024;

/// How a chunk's payload is stored. A `u8` so more can arrive (encrypted
/// variants) without a format break; unknown codecs are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkCodec {
    /// Raw events, no compression. Kept for tests and tiny chunks where
    /// compression is a loss.
    Plain,
    /// zstd. MEASURED 11–15× at level 3, 19–22× at 19 on real command
    /// streams (specs/history.md, spike results).
    Zstd,
    /// zstd, then XChaCha20-Poly1305 under the segment's data key —
    /// compress-then-encrypt, because ciphertext does not compress. The
    /// payload is `nonce || ct`; the chunk header's 4-byte hash stays a
    /// hash of the *plaintext*, which is what lets crash-honesty and the
    /// merkle survive encryption unchanged.
    ZstdSealed,
}

impl ChunkCodec {
    fn tag(self) -> u8 {
        match self {
            ChunkCodec::Plain => 0,
            ChunkCodec::Zstd => 1,
            ChunkCodec::ZstdSealed => 2,
        }
    }
    fn from_tag(t: u8) -> Option<ChunkCodec> {
        match t {
            0 => Some(ChunkCodec::Plain),
            1 => Some(ChunkCodec::Zstd),
            2 => Some(ChunkCodec::ZstdSealed),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum SegmentError {
    Io(std::io::Error),
    BadMagic,
    UnsupportedVersion(u8),
    UnknownCodec(u8),
    ChunkTooLarge(usize),
    Event(EventError),
    Truncated,
    /// The file claims to be sealed and the claim does not hold — a bad seal
    /// parse, a chunk that fails inside the sealed region, a count or hash
    /// that disagrees with the footer. Unlike a torn tail this is never
    /// expected: a sealed segment promised wholeness, so the reader refuses
    /// the file rather than salvaging it and letting tampering pass as
    /// crash damage.
    SealBroken(String),
    /// The segment is encrypted and the key on hand does not open it — no
    /// key at all, or a key no keyslot accepts. AEAD cannot distinguish a
    /// wrong key from a mangled slot, so neither does this; either way the
    /// honest report is "locked", never a silent empty read.
    Locked(String),
}

impl std::fmt::Display for SegmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SegmentError::Io(e) => write!(f, "io: {e}"),
            SegmentError::BadMagic => write!(f, "not a .rhs segment"),
            SegmentError::UnsupportedVersion(v) => write!(f, "unsupported format version {v}"),
            SegmentError::UnknownCodec(c) => write!(f, "unknown chunk codec {c}"),
            SegmentError::ChunkTooLarge(n) => write!(f, "chunk too large ({n})"),
            SegmentError::Event(e) => write!(f, "event: {e}"),
            SegmentError::Truncated => write!(f, "truncated segment"),
            SegmentError::SealBroken(why) => write!(f, "seal broken: {why}"),
            SegmentError::Locked(why) => write!(f, "locked: {why}"),
        }
    }
}

impl std::error::Error for SegmentError {}

impl From<std::io::Error> for SegmentError {
    fn from(e: std::io::Error) -> Self {
        SegmentError::Io(e)
    }
}

/// What a segment's header carries. The keyslot table is present from the
/// first version even though slice 1 fills no slots: adding an owner key
/// later must not reformat existing history (specs/history.md, decision 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    /// The recording device's fingerprint, or empty when unenrolled.
    pub device: String,
    /// Wall clock at segment start, for correlating the monotonic deltas.
    pub wall_start_ms: u64,
    /// Filled keyslots. Empty means unencrypted — which the reader reports
    /// honestly rather than silently treating as fine.
    pub keyslots: Vec<Keyslot>,
}

/// One way to unwrap the segment key. Slice 1 writes none; the shape is
/// fixed now so passphrase and recovery-code slots are additive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyslot {
    pub kind: u8,
    pub blob: Vec<u8>,
}

const VERSION: u8 = 1;

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_short(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

impl Header {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.push(self.version);
        put_short(&mut out, self.device.as_bytes());
        put_u64(&mut out, self.wall_start_ms);
        put_u32(&mut out, self.keyslots.len() as u32);
        for slot in &self.keyslots {
            out.push(slot.kind);
            put_short(&mut out, &slot.blob);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<(Header, usize), SegmentError> {
        let mut r = Cursor { b: bytes, p: 0 };
        if r.take(4)? != MAGIC {
            return Err(SegmentError::BadMagic);
        }
        let version = r.u8()?;
        if version != VERSION {
            return Err(SegmentError::UnsupportedVersion(version));
        }
        let device = String::from_utf8(r.short()?.to_vec()).map_err(|_| SegmentError::Truncated)?;
        let wall_start_ms = r.u64()?;
        let n = r.u32()? as usize;
        if n > 16 {
            return Err(SegmentError::Truncated);
        }
        let mut keyslots = Vec::with_capacity(n);
        for _ in 0..n {
            let kind = r.u8()?;
            keyslots.push(Keyslot { kind, blob: r.short()?.to_vec() });
        }
        Ok((Header { version, device, wall_start_ms, keyslots }, r.p))
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SegmentError> {
        let end = self.p.checked_add(n).ok_or(SegmentError::Truncated)?;
        let s = self.b.get(self.p..end).ok_or(SegmentError::Truncated)?;
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, SegmentError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, SegmentError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, SegmentError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn short(&mut self) -> Result<&'a [u8], SegmentError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// Appends events to one segment file.
///
/// The writer owns *only* buffering and framing: callers hand it stamped
/// events and it decides when bytes reach the disk. Nothing here blocks a
/// render loop — the compositor's hot path is a channel send, and this runs
/// on the recorder thread (specs/history.md, runtime mechanics).
pub struct SegmentWriter {
    path: PathBuf,
    file: File,
    /// Events encoded since the last flush.
    pending: Vec<u8>,
    /// When the first unflushed event arrived. `None` when the buffer is
    /// empty — which is what makes an idle desktop write nothing at all.
    pending_since: Option<std::time::Instant>,
    /// Frame chunks are kept separate from event chunks so retention can
    /// drop them without touching the transcript.
    pending_is_frames: bool,
    raw_written: u64,
    events_written: u64,
    codec: ChunkCodec,
    level: i32,
    /// The segment's data key when encrypting, and the KEK that wrapped it
    /// (kept so `finish` can hand sealing the same unlock).
    key: Option<(DataKey, Kek)>,
}

impl SegmentWriter {
    /// Create a plaintext segment and write its header.
    pub fn create(
        path: &Path,
        header: &Header,
        codec: ChunkCodec,
        level: i32,
    ) -> Result<SegmentWriter, SegmentError> {
        SegmentWriter::create_with_key(path, header, codec, level, None)
    }

    /// Create a segment, encrypted when a KEK is given: a fresh data key is
    /// generated, wrapped into the header's device keyslot, and every chunk
    /// and index blob goes to disk under it (decision 2). The caller's
    /// header must not carry keyslots of its own — the wrap is this
    /// function's job, which is what keeps a data key from ever existing
    /// unwrapped outside this process.
    pub fn create_with_key(
        path: &Path,
        header: &Header,
        codec: ChunkCodec,
        level: i32,
        kek: Option<&Kek>,
    ) -> Result<SegmentWriter, SegmentError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut header = header.clone();
        let key = kek.map(|kek| {
            let key = DataKey::generate();
            header.keyslots.push(Keyslot { kind: KEYSLOT_DEVICE, blob: kek.wrap(&key) });
            (key, kek.clone())
        });
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
        file.write_all(&header.encode())?;
        Ok(SegmentWriter {
            path: path.to_path_buf(),
            file,
            pending: Vec::with_capacity(CHUNK_TARGET),
            pending_since: None,
            pending_is_frames: false,
            raw_written: 0,
            events_written: 0,
            codec,
            level,
            key,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn events_written(&self) -> u64 {
        self.events_written
    }

    /// Append one event. Returns whether a flush happened (useful in tests;
    /// callers can ignore it).
    pub fn append(&mut self, s: &Stamped) -> Result<bool, SegmentError> {
        // Frames and everything else never share a chunk: retention drops
        // frame chunks wholesale, and mixing would take the transcript with
        // them.
        let is_frame = matches!(s.event, Event::Frame { .. });
        let mut flushed = false;
        if !self.pending.is_empty() && is_frame != self.pending_is_frames {
            self.flush()?;
            flushed = true;
        }
        self.pending_is_frames = is_frame;
        if self.pending_since.is_none() {
            self.pending_since = Some(std::time::Instant::now());
        }
        encode(&mut self.pending, s).map_err(SegmentError::Event)?;
        self.events_written += 1;
        if self.pending.len() >= CHUNK_TARGET {
            self.flush()?;
            flushed = true;
        }
        Ok(flushed)
    }

    /// Whether the elapsed-time trigger has fired. Callers poll this; it is
    /// deliberately not a timer, so a quiet desktop is never woken.
    pub fn flush_due(&self) -> bool {
        self.pending_since.is_some_and(|t| t.elapsed() >= FLUSH_AFTER)
    }

    /// Whether this segment has reached its rotation size.
    pub fn should_rotate(&self) -> bool {
        self.raw_written >= SEGMENT_TARGET
    }

    /// Write the buffered events as one chunk and fsync. A no-op when empty,
    /// so calling it on a timer costs nothing.
    pub fn flush(&mut self) -> Result<(), SegmentError> {
        if self.pending.is_empty() {
            self.pending_since = None;
            return Ok(());
        }
        let raw = std::mem::take(&mut self.pending);
        let payload = match self.codec {
            ChunkCodec::Plain => raw.clone(),
            ChunkCodec::Zstd | ChunkCodec::ZstdSealed => {
                zstd::stream::encode_all(&raw[..], self.level).map_err(SegmentError::Io)?
            }
        };
        // Compress, then encrypt: ciphertext does not compress.
        let (payload, tag) = match &self.key {
            Some((key, _)) => (key.seal(&payload), ChunkCodec::ZstdSealed.tag()),
            None => (payload, self.codec.tag()),
        };
        let mut framed = Vec::with_capacity(payload.len() + 9);
        put_u32(&mut framed, payload.len() as u32);
        framed.push(tag);
        // Hash of the *plaintext*: it validates the chunk end-to-end and
        // survives a later switch to an encrypted payload.
        framed.extend_from_slice(&blake3::hash(&raw).as_bytes()[..4]);
        framed.extend_from_slice(&payload);
        self.file.write_all(&framed)?;
        self.file.sync_data()?;
        self.raw_written += raw.len() as u64;
        self.pending = raw;
        self.pending.clear();
        self.pending_since = None;
        Ok(())
    }

    /// Flush, seal, and close. The segment stays readable without this —
    /// sealing is the clean path, not a requirement for the file to decode —
    /// but only a sealed segment carries its footer, its stored indexes, and
    /// the wholeness promise that retention and sharing build on.
    pub fn finish(mut self) -> Result<PathBuf, SegmentError> {
        self.flush()?;
        self.file.sync_all()?;
        let path = self.path;
        let kek = self.key.map(|(_, kek)| kek);
        drop(self.file);
        seal_path_with(&path, kek.as_ref())?;
        Ok(path)
    }
}

/// Seal a segment in place: append the per-tier indexes, the footer, and the
/// tail mark. Idempotent — sealing a sealed segment is a no-op — and the
/// recovery path for a crashed writer: the next session seals what the last
/// one left open. A torn tail is truncated to the last whole chunk first,
/// which is the same bytes the crash-honest reader would have surrendered;
/// the seal then covers exactly what was ever durable.
///
/// This is the one heavy operation in the writer's life — it re-reads the
/// segment it just wrote — and it runs on the recorder thread between
/// segments, never against the compositor (specs/history.md).
pub fn seal_path(path: &Path) -> Result<(), SegmentError> {
    seal_path_with(path, None)
}

pub fn seal_path_with(path: &Path, kek: Option<&Kek>) -> Result<(), SegmentError> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if seal_region(&bytes).is_some() {
        return Ok(());
    }
    let (header, header_end) = Header::decode(&bytes)?;
    let key = unlock(&header, kek)?;

    // One streaming pass: every accumulator below is O(index), never
    // O(events). The decoded events of each chunk live exactly as long as
    // this closure runs — the soak-measured alternative was the whole
    // segment's events resident at once, retained by the allocator after.
    let mut t_abs: u64 = 0;
    let mut event_count: u64 = 0;
    let mut span: Option<(u64, u64)> = None;
    let mut builders: std::collections::BTreeMap<Tier, crate::index::Builder> =
        std::collections::BTreeMap::new();
    let scan = walk_chunks(&bytes, header_end, bytes.len(), key.as_ref(), &mut |evs| {
        for s in &evs {
            t_abs += s.dt_ms as u64;
            event_count += 1;
            span = Some(match span {
                None => (t_abs, t_abs),
                Some((lo, _)) => (lo, t_abs),
            });
            // A tier's builder is created on that tier's first event, which
            // loses nothing: every earlier event was filtered by its tier
            // anyway. But every event is pushed to EVERY builder, because
            // the frame-fallback switch is segment-wide — a T0 Text must
            // disable frame extraction in the T1 index too, exactly as the
            // batch builder's whole-segment scan did.
            builders
                .entry(s.tier)
                .or_insert_with(|| crate::index::Builder::new(s.tier));
            for b in builders.values_mut() {
                b.push(t_abs, s);
            }
        }
    })?;
    if scan.stopped.is_some() {
        // The torn trailing chunk was never durably written — cutting it is
        // the crash-honesty contract applied with a knife instead of a
        // shrug. Everything the seal will then assert really is on disk.
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(scan.end as u64)?;
        file.sync_all()?;
        bytes.truncate(scan.end);
    }

    let tiers: Vec<Tier> = builders.keys().copied().collect();
    let indexes = builders.into_values().map(crate::index::Builder::finish).collect();
    let span = span.unwrap_or((0, 0));
    let seal = Seal {
        events: event_count,
        span,
        sealed_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(header.wall_start_ms + span.1),
        tiers,
        chunks: scan.chunk_hashes.len() as u32,
        plaintext: *scan.whole.finalize().as_bytes(),
        merkle: merkle_root(&scan.chunk_hashes),
        indexes,
    };
    let region = encode_seal(&seal, key.as_ref());
    let mut tail = Vec::with_capacity(region.len() + SEAL_TAIL);
    tail.extend_from_slice(&region);
    tail.extend_from_slice(blake3::hash(&region).as_bytes());
    put_u32(&mut tail, region.len() as u32);
    tail.extend_from_slice(&SEAL_MAGIC);

    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(&tail)?;
    file.sync_all()?;
    // Hand the seal's transient back to the OS. The streaming pass above
    // already shrank the peak from O(segment-decoded) to O(chunk) + O(file),
    // but glibc retains freed heap as arena high-water — the 2026-08-25
    // soak watched exactly that staircase 28 → 130 MiB in two seals on a
    // 1 GB board. malloc_trim releases what the allocator is merely
    // hoarding; a no-op cost on the paths that don't hoard.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
    Ok(())
}

/// Just the header and the seal, without decoding a single chunk — the fast
/// path a corpus scan takes over a sealed segment. `Ok(None)` is an unsealed
/// segment; the caller falls back to the full read.
pub fn read_seal(path: &Path) -> Result<Option<(Header, Seal)>, SegmentError> {
    read_seal_with(path, None)
}

pub fn read_seal_with(
    path: &Path,
    kek: Option<&Kek>,
) -> Result<Option<(Header, Seal)>, SegmentError> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    // Header first (small, at the front).
    let mut head = vec![0u8; 65536.min(len as usize)];
    file.read_exact(&mut head)?;
    let (header, _) = Header::decode(&head)?;
    // Tail next (small, at the back).
    if len < SEAL_TAIL as u64 {
        return Ok(None);
    }
    let mut tail = [0u8; SEAL_TAIL];
    file.seek(SeekFrom::End(-(SEAL_TAIL as i64)))?;
    file.read_exact(&mut tail)?;
    if tail[36..] != SEAL_MAGIC {
        return Ok(None);
    }
    let hash: [u8; 32] = tail[..32].try_into().unwrap();
    let seal_len = u32::from_be_bytes(tail[32..36].try_into().unwrap()) as u64;
    if seal_len > MAX_SEAL as u64 || seal_len + SEAL_TAIL as u64 > len {
        return Err(SegmentError::SealBroken("implausible seal length".into()));
    }
    let mut region = vec![0u8; seal_len as usize];
    file.seek(SeekFrom::End(-(SEAL_TAIL as i64) - seal_len as i64))?;
    file.read_exact(&mut region)?;
    if *blake3::hash(&region).as_bytes() != hash {
        return Err(SegmentError::SealBroken("seal region hash mismatch".into()));
    }
    let key = unlock(&header, kek)?;
    Ok(Some((header, decode_seal(&region, key.as_ref())?)))
}

/// What a seal asserts about its segment — the footer, decoded, plus the
/// per-tier indexes stored beside it.
///
/// The indexes are one blob per tier deliberately, the same shaping argument
/// as the header's keyslot table: when encryption lands, each tier's index
/// wraps under that tier's key without a format change, and a search made
/// while sealed content is locked cannot read what it cannot decrypt.
#[derive(Debug, Clone, PartialEq)]
pub struct Seal {
    /// Total events across every chunk.
    pub events: u64,
    /// Absolute span covered, in ms since the segment's start.
    pub span: (u64, u64),
    /// Wall clock when the seal was written.
    pub sealed_at_ms: u64,
    /// Every tier present in the events, whether or not it produced text.
    pub tiers: Vec<Tier>,
    /// Chunk count, and the two integrity roots over their plaintext: the
    /// flat hash proves the whole, the merkle root lets a future share or an
    /// aged segment prove membership of the chunks it kept (decision 7).
    pub chunks: u32,
    pub plaintext: [u8; 32],
    pub merkle: [u8; 32],
    /// The stored per-tier indexes. Derived and rebuildable — a segment
    /// whose stored index is unusable is a rebuild, never a loss.
    pub indexes: Vec<Index>,
}

/// Merkle root over the chunks' plaintext hashes: parents are
/// `blake3(left || right)`, an odd node is paired with itself, no chunks is
/// the hash of nothing.
fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return *blake3::hash(&[]).as_bytes();
    }
    let mut level: Vec<[u8; 32]> = leaves.to_vec();
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| {
                let mut h = blake3::Hasher::new();
                h.update(&pair[0]);
                h.update(pair.get(1).unwrap_or(&pair[0]));
                *h.finalize().as_bytes()
            })
            .collect();
    }
    level[0]
}

/// Encode the seal region: `[n_indexes u8] ([tier u8][len u32][blob])*` then
/// the footer fields. The tail `[seal_len u32][SEAL_MAGIC]` goes after it.
fn encode_seal(seal: &Seal, key: Option<&DataKey>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(seal.indexes.len() as u8);
    for index in &seal.indexes {
        out.push(index.tier);
        // The indexes hold the transcript — all the text — so they encrypt
        // whenever the chunks do. The footer stays plaintext: spans, counts
        // and hashes are the metadata a manifest may read without a key.
        let blob = match key {
            Some(k) => k.seal(&index.to_bytes()),
            None => index.to_bytes(),
        };
        put_u32(&mut out, blob.len() as u32);
        out.extend_from_slice(&blob);
    }
    put_u64(&mut out, seal.events);
    put_u64(&mut out, seal.span.0);
    put_u64(&mut out, seal.span.1);
    put_u64(&mut out, seal.sealed_at_ms);
    out.push(seal.tiers.len() as u8);
    out.extend_from_slice(&seal.tiers);
    put_u32(&mut out, seal.chunks);
    out.extend_from_slice(&seal.plaintext);
    out.extend_from_slice(&seal.merkle);
    out
}

fn decode_seal(bytes: &[u8], key: Option<&DataKey>) -> Result<Seal, SegmentError> {
    let broke = |why: &str| SegmentError::SealBroken(why.into());
    let mut r = Cursor { b: bytes, p: 0 };
    let n = r.u8().map_err(|_| broke("seal region truncated"))?;
    let mut indexes = Vec::new();
    for _ in 0..n {
        let tier = r.u8().map_err(|_| broke("index tier truncated"))?;
        let blob = r.short().map_err(|_| broke("index blob truncated"))?;
        let index = match key {
            Some(k) => {
                let pt = k
                    .open(blob)
                    .ok_or_else(|| broke("stored index failed to open"))?;
                Index::from_bytes(tier, &pt)
            }
            None => Index::from_bytes(tier, blob),
        };
        indexes.push(index.ok_or_else(|| broke("stored index malformed"))?);
    }
    let events = r.u64().map_err(|_| broke("footer truncated"))?;
    let span = (
        r.u64().map_err(|_| broke("footer truncated"))?,
        r.u64().map_err(|_| broke("footer truncated"))?,
    );
    let sealed_at_ms = r.u64().map_err(|_| broke("footer truncated"))?;
    let nt = r.u8().map_err(|_| broke("footer truncated"))? as usize;
    if nt > 8 {
        return Err(broke("implausible tier count"));
    }
    let tiers = r.take(nt).map_err(|_| broke("footer truncated"))?.to_vec();
    let chunks = r.u32().map_err(|_| broke("footer truncated"))?;
    let plaintext: [u8; 32] =
        r.take(32).map_err(|_| broke("footer truncated"))?.try_into().unwrap();
    let merkle: [u8; 32] =
        r.take(32).map_err(|_| broke("footer truncated"))?.try_into().unwrap();
    if r.p != bytes.len() {
        return Err(broke("trailing bytes after footer"));
    }
    Ok(Seal { events, span, sealed_at_ms, tiers, chunks, plaintext, merkle, indexes })
}

/// A segment read back: its header, and every event from every chunk that
/// survived intact.
pub struct SegmentRead {
    pub header: Header,
    pub events: Vec<Stamped>,
    /// Why reading stopped early, if it did. `Some` means the tail was torn
    /// — expected for a segment killed mid-write, and not an error.
    pub stopped: Option<String>,
    /// The verified seal, when the segment has one. `None` is an open (or
    /// crashed) segment — still readable, just not yet promising wholeness.
    pub seal: Option<Seal>,
}

/// Read a segment, tolerating a torn tail.
///
/// This is the crash-honesty contract: every whole chunk before the damage
/// decodes, the incomplete one is dropped, and the caller is told. Strict
/// validation still applies *within* a chunk — a chunk that fails its hash
/// or its event decode is corruption, and reading stops there rather than
/// guessing.
pub fn read(path: &Path) -> Result<SegmentRead, SegmentError> {
    read_with(path, None)
}

pub fn read_with(path: &Path, kek: Option<&Kek>) -> Result<SegmentRead, SegmentError> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    read_bytes_with(&bytes, kek)
}

/// One pass over the chunk region: events out, integrity material alongside.
struct ChunkScan {
    events: Vec<Stamped>,
    /// Full plaintext hash per intact chunk — the merkle leaves.
    chunk_hashes: Vec<[u8; 32]>,
    /// Running hash of the concatenated plaintexts — the flat root.
    whole: blake3::Hasher,
    /// Where the last intact chunk ended (torn tails start here).
    end: usize,
    stopped: Option<String>,
}

fn scan_chunks(
    bytes: &[u8],
    from: usize,
    to: usize,
    key: Option<&DataKey>,
) -> Result<ChunkScan, SegmentError> {
    let mut events = Vec::new();
    let walk = walk_chunks(bytes, from, to, key, &mut |mut evs: Vec<Stamped>| {
        events.append(&mut evs);
    })?;
    Ok(ChunkScan {
        events,
        chunk_hashes: walk.chunk_hashes,
        whole: walk.whole,
        end: walk.end,
        stopped: walk.stopped,
    })
}

/// The integrity material a pass over the chunks produces, without the
/// events themselves — those go to the sink, one chunk at a time, and can
/// be dropped as soon as the sink returns. This is what lets the seal run
/// in O(chunk) memory instead of O(segment): the 2026-08-25 soak measured
/// batch sealing retaining ~50 MiB of heap per seal on the 1 GB Pi, a
/// staircase that walks a week-long run into the OOM killer around day 3.
struct ChunkWalk {
    chunk_hashes: Vec<[u8; 32]>,
    whole: blake3::Hasher,
    end: usize,
    stopped: Option<String>,
}

fn walk_chunks(
    bytes: &[u8],
    from: usize,
    to: usize,
    key: Option<&DataKey>,
    on_chunk: &mut dyn FnMut(Vec<Stamped>),
) -> Result<ChunkWalk, SegmentError> {
    let mut scan = ChunkWalk {
        chunk_hashes: Vec::new(),
        whole: blake3::Hasher::new(),
        end: from,
        stopped: None,
    };
    let mut pos = from;
    while pos < to {
        let Some(head) = bytes.get(pos..pos + 9) else {
            scan.stopped = Some("torn chunk header".into());
            break;
        };
        let len = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
        if len > MAX_CHUNK {
            return Err(SegmentError::ChunkTooLarge(len));
        }
        let Some(codec) = ChunkCodec::from_tag(head[4]) else {
            return Err(SegmentError::UnknownCodec(head[4]));
        };
        let hash = &head[5..9];
        let start = pos + 9;
        if start + len > to {
            scan.stopped = Some("torn chunk payload".into());
            break;
        }
        let payload = &bytes[start..start + len];
        let raw = match codec {
            ChunkCodec::Plain => payload.to_vec(),
            ChunkCodec::Zstd => match rill_store::encoding::decompress(payload, MAX_CHUNK_DECODED) {
                Ok(v) => v,
                Err(e) => {
                    scan.stopped = Some(format!("chunk decompress failed: {e}"));
                    break;
                }
            },
            ChunkCodec::ZstdSealed => {
                // A sealed chunk in a segment we hold no key for is caught
                // before the scan; reaching here without one is a plaintext
                // header lying about its chunks — corruption, not a lock.
                let Some(key) = key else {
                    scan.stopped = Some("sealed chunk in a keyless segment".into());
                    break;
                };
                let Some(ct) = key.open(payload) else {
                    scan.stopped = Some("sealed chunk failed to open".into());
                    break;
                };
                match rill_store::encoding::decompress(&ct, MAX_CHUNK_DECODED) {
                    Ok(v) => v,
                    Err(e) => {
                        scan.stopped = Some(format!("chunk decompress failed: {e}"));
                        break;
                    }
                }
            }
        };
        let full = blake3::hash(&raw);
        if &full.as_bytes()[..4] != hash {
            scan.stopped = Some("chunk hash mismatch".into());
            break;
        }
        match decode_chunk(&raw) {
            Ok(evs) => on_chunk(evs),
            Err(e) => {
                scan.stopped = Some(format!("chunk decode failed: {e}"));
                break;
            }
        }
        scan.chunk_hashes.push(*full.as_bytes());
        scan.whole.update(&raw);
        pos = start + len;
        scan.end = pos;
    }
    Ok(scan)
}

/// The segment's data key, from its keyslots and the key on hand. `Ok(None)`
/// is a plaintext segment; `Err(Locked)` is an encrypted one this key does
/// not open — reported honestly rather than read as empty.
fn unlock(header: &Header, kek: Option<&Kek>) -> Result<Option<DataKey>, SegmentError> {
    if header.keyslots.is_empty() {
        return Ok(None);
    }
    let Some(kek) = kek else {
        return Err(SegmentError::Locked(
            "segment is encrypted and no identity key is available".into(),
        ));
    };
    for slot in &header.keyslots {
        if slot.kind == KEYSLOT_DEVICE
            && let Some(key) = kek.unwrap(&slot.blob)
        {
            return Ok(Some(key));
        }
    }
    Err(SegmentError::Locked("no keyslot this key opens".into()))
}

/// Locate the seal region, if the tail claims one: returns the byte range of
/// `[indexes][footer]` (where the chunks end and the seal begins) plus the
/// region hash the tail asserts.
fn seal_region(bytes: &[u8]) -> Option<(usize, usize, [u8; 32])> {
    let tail_at = bytes.len().checked_sub(SEAL_TAIL)?;
    if bytes[tail_at + 36..] != SEAL_MAGIC {
        return None;
    }
    let hash: [u8; 32] = bytes[tail_at..tail_at + 32].try_into().unwrap();
    let seal_len =
        u32::from_be_bytes(bytes[tail_at + 32..tail_at + 36].try_into().unwrap()) as usize;
    if seal_len > MAX_SEAL {
        return None;
    }
    let start = tail_at.checked_sub(seal_len)?;
    Some((start, tail_at, hash))
}

pub fn read_bytes(bytes: &[u8]) -> Result<SegmentRead, SegmentError> {
    read_bytes_with(bytes, None)
}

pub fn read_bytes_with(bytes: &[u8], kek: Option<&Kek>) -> Result<SegmentRead, SegmentError> {
    let (header, header_end) = Header::decode(bytes)?;
    let key = unlock(&header, kek)?;

    if let Some((seal_start, seal_end, region_hash)) = seal_region(bytes) {
        // Sealed: the footer's claims are checked against what the chunks
        // actually contain, and any disagreement — a stop mid-scan included —
        // refuses the file. A sealed segment promised wholeness; salvage
        // semantics would let tampering pass as crash damage.
        let broke = |why: String| SegmentError::SealBroken(why);
        if seal_start < header_end {
            return Err(broke("seal region overlaps the header".into()));
        }
        if *blake3::hash(&bytes[seal_start..seal_end]).as_bytes() != region_hash {
            return Err(broke("seal region hash mismatch".into()));
        }
        let seal = decode_seal(&bytes[seal_start..seal_end], key.as_ref())?;
        let scan = scan_chunks(bytes, header_end, seal_start, key.as_ref())?;
        if let Some(why) = scan.stopped {
            return Err(broke(format!("inside sealed chunks: {why}")));
        }
        if scan.end != seal_start {
            return Err(broke("chunk region does not reach the seal".into()));
        }
        if scan.chunk_hashes.len() as u32 != seal.chunks {
            return Err(broke(format!(
                "footer says {} chunks, file has {}",
                seal.chunks,
                scan.chunk_hashes.len()
            )));
        }
        if scan.events.len() as u64 != seal.events {
            return Err(broke(format!(
                "footer says {} events, chunks decode to {}",
                seal.events,
                scan.events.len()
            )));
        }
        if *scan.whole.finalize().as_bytes() != seal.plaintext {
            return Err(broke("plaintext hash mismatch".into()));
        }
        if merkle_root(&scan.chunk_hashes) != seal.merkle {
            return Err(broke("merkle root mismatch".into()));
        }
        return Ok(SegmentRead { header, events: scan.events, stopped: None, seal: Some(seal) });
    }

    // Open or crashed: the crash-honesty contract — every whole chunk before
    // any damage decodes, the incomplete one is dropped, the caller is told.
    let scan = scan_chunks(bytes, header_end, bytes.len(), key.as_ref())?;
    Ok(SegmentRead { header, events: scan.events, stopped: scan.stopped, seal: None })
}

/// Absolute-time view of a decoded segment: the deltas summed back into
/// milliseconds since the segment's start.
pub fn absolute_times(events: &[Stamped]) -> Vec<(u64, &Stamped)> {
    let mut t = 0u64;
    events
        .iter()
        .map(|e| {
            t += e.dt_ms as u64;
            (t, e)
        })
        .collect()
}

/// Every tier present in a segment — what a reader needs to know which keys
/// it would require.
pub fn tiers_present(events: &[Stamped]) -> Vec<Tier> {
    let mut seen: Vec<Tier> = events.iter().map(|e| e.tier).collect();
    seen.sort_unstable();
    seen.dedup();
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{T0_ROUTINE, T2_SEALED, WindowState};

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rhs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn header() -> Header {
        Header {
            version: VERSION,
            device: "desktop".into(),
            wall_start_ms: 1_760_000_000_000,
            keyslots: Vec::new(),
        }
    }

    fn ev(dt: u32, id: u32) -> Stamped {
        Stamped { dt_ms: dt, tier: T0_ROUTINE, event: Event::Focus { id } }
    }

    fn frame(dt: u32, n: usize) -> Stamped {
        Stamped {
            dt_ms: dt,
            tier: T0_ROUTINE,
            event: Event::Frame { id: 1, bytes: vec![0xAB; n] },
        }
    }

    /// A chunk's length prefix bounds the compressed bytes, not what they
    /// become. Segments are meant to be shared, so a reader has to survive one
    /// built to expand: a few hundred KiB of zeroes on disk that would
    /// otherwise allocate gigabytes on a machine that has one.
    #[test]
    fn a_compressed_chunk_cannot_expand_without_limit() {
        // Zeroes compress at roughly 1000:1, so this is a small file that
        // decodes to well past the cap.
        let bomb = zstd::stream::encode_all(
            &vec![0u8; (MAX_CHUNK_DECODED as usize) + (8 << 20)][..],
            19,
        )
        .unwrap();
        assert!(bomb.len() < MAX_CHUNK, "the bomb is small on disk: {}", bomb.len());

        let mut bytes = header().encode();
        bytes.extend_from_slice(&(bomb.len() as u32).to_be_bytes());
        bytes.push(ChunkCodec::Zstd.tag());
        bytes.extend_from_slice(&[0, 0, 0, 0]); // hash — never reached
        bytes.extend_from_slice(&bomb);

        // Refused, and refused without having allocated what it asked for.
        let read = read_bytes(&bytes).expect("a bomb is a stopped read, not an error");
        assert!(read.events.is_empty(), "no events came out of the bomb");
        let stopped = read.stopped.expect("the read reports why it stopped");
        assert!(
            stopped.contains("exceeds configured cap"),
            "the cap is what stopped it, not some other decode failure: {stopped}"
        );
    }

    /// Writes seed inputs for `cargo fuzz run segment_read`. Ignored: run
    /// explicitly with `cargo test -p rill-history -- --ignored write_fuzz`
    /// when the corpus needs refreshing (the corpus is committed).
    ///
    /// The seeds are aimed at *chunk framing*, since that is where the format
    /// can lie: both codecs, several chunks, and — the case the reader exists
    /// to survive — a segment cut mid-chunk. A fuzzer will find the byte to
    /// flip far sooner than it will invent a valid header and a chunk table
    /// underneath it.
    #[test]
    #[ignore]
    fn write_fuzz_corpus() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz/corpus/segment_read");
        std::fs::create_dir_all(dir).unwrap();

        let build = |codec: ChunkCodec, events: &[Stamped]| -> Vec<u8> {
            let path = tmp("corpus-seed.rhs");
            let mut w = SegmentWriter::create(&path, &header(), codec, 3).unwrap();
            for e in events {
                w.append(e).unwrap();
            }
            w.finish().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            bytes
        };

        let mixed: Vec<Stamped> = (0..40)
            .map(|i| if i % 8 == 0 { frame(i, 128) } else { ev(i, i) })
            .collect();

        let plain = build(ChunkCodec::Plain, &mixed);
        std::fs::write(format!("{dir}/seed-plain"), &plain).unwrap();
        let zstd = build(ChunkCodec::Zstd, &mixed);
        std::fs::write(format!("{dir}/seed-zstd"), &zstd).unwrap();
        // Header and nothing else — the empty-but-valid case.
        std::fs::write(format!("{dir}/seed-header-only"), header().encode()).unwrap();
        // Torn mid-chunk: the shape every interrupted recording has, and the
        // one the reader promises to survive rather than reject. Cutting a
        // sealed file also destroys its tail magic, so this doubles as the
        // "sealed file truncated" case landing on the tolerant path.
        std::fs::write(format!("{dir}/seed-torn"), &zstd[..zstd.len() * 2 / 3]).unwrap();
        // finish() seals, so the seeds above all carry seal regions. Two
        // aimed at the seal itself: an OPEN segment (no seal to parse), and
        // a sealed one with a flipped byte in its stored index — the tamper
        // class the region hash exists to catch.
        let open_path = tmp("corpus-seed-open.rhs");
        let mut w = SegmentWriter::create(&open_path, &header(), ChunkCodec::Plain, 0).unwrap();
        for e in &mixed[..8] {
            w.append(e).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        std::fs::write(format!("{dir}/seed-open"), std::fs::read(&open_path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&open_path);
        let mut tampered = plain.clone();
        let mid = tampered.len() - 60;
        tampered[mid] ^= 0x01;
        std::fs::write(format!("{dir}/seed-seal-tampered"), &tampered).unwrap();
        // An encrypted segment, which the keyless fuzz target must report
        // Locked — never panic, never read as empty-and-fine.
        let enc_path = tmp("corpus-seed-enc.rhs");
        let kek = crate::crypt::Kek::from_bytes([1; 32]);
        let mut w = SegmentWriter::create_with_key(
            &enc_path,
            &header(),
            ChunkCodec::Zstd,
            3,
            Some(&kek),
        )
        .unwrap();
        for e in &mixed[..12] {
            w.append(e).unwrap();
        }
        w.finish().unwrap();
        std::fs::write(format!("{dir}/seed-encrypted"), std::fs::read(&enc_path).unwrap())
            .unwrap();
        let _ = std::fs::remove_file(&enc_path);
    }

    #[test]
    fn round_trips_through_a_file() {
        let path = tmp("round.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Zstd, 3).unwrap();
        let events: Vec<Stamped> = (0..50).map(|i| ev(i, i)).collect();
        for e in &events {
            w.append(e).unwrap();
        }
        w.finish().unwrap();

        let back = read(&path).unwrap();
        assert_eq!(back.header, header());
        assert_eq!(back.events, events);
        assert!(back.stopped.is_none(), "clean segment should not report damage");
        let _ = std::fs::remove_file(&path);
    }

    /// The crash-honesty property, at the layer that promises it: a segment
    /// cut anywhere reads back to its last whole chunk and says so — it
    /// never errors, and never invents an event.
    #[test]
    fn a_torn_tail_reads_up_to_the_last_whole_chunk() {
        let path = tmp("torn.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Zstd, 3).unwrap();
        // Three chunks: flush explicitly so the boundaries are known.
        for i in 0..10 {
            w.append(&ev(i, i)).unwrap();
        }
        w.flush().unwrap();
        for i in 10..20 {
            w.append(&ev(i, i)).unwrap();
        }
        w.flush().unwrap();
        for i in 20..30 {
            w.append(&ev(i, i)).unwrap();
        }
        // A torn tail happens to a writer that never got to finish; a
        // finished segment is sealed and refuses damage outright.
        w.flush().unwrap();
        drop(w);

        let whole = std::fs::read(&path).unwrap();
        let full = read_bytes(&whole).unwrap();
        assert_eq!(full.events.len(), 30);

        // The byte offsets a chunk ends on — the points where a truncated
        // file is *indistinguishable* from a session that simply stopped
        // there. That indistinguishability is inherent to an append-only
        // log, and is exactly what makes one crash-honest: a clean prefix
        // is a valid, shorter log. Damage is only detectable when the cut
        // lands mid-chunk.
        let mut boundaries = vec![Header::decode(&whole).unwrap().1];
        let mut pos = boundaries[0];
        while pos < whole.len() {
            let len = u32::from_be_bytes(whole[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 9 + len;
            boundaries.push(pos);
        }

        for cut in 1..whole.len() {
            match read_bytes(&whole[..cut]) {
                Ok(r) => {
                    assert!(
                        [0, 10, 20, 30].contains(&r.events.len()),
                        "cut {cut} gave {} events — chunks must be all-or-nothing",
                        r.events.len()
                    );
                    if r.events.len() < 30 && !boundaries.contains(&cut) {
                        assert!(r.stopped.is_some(), "cut {cut} lost events silently");
                    }
                }
                // Only the header itself can be too short to parse.
                Err(e) => assert!(cut < 32, "cut {cut} errored past the header: {e}"),
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Corruption inside a chunk is not a torn tail: it stops the read and
    /// says why, rather than handing back events from a chunk that failed
    /// its hash.
    #[test]
    fn a_corrupt_chunk_stops_the_read() {
        let path = tmp("corrupt.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Plain, 0).unwrap();
        for i in 0..5 {
            w.append(&ev(i, i)).unwrap();
        }
        w.flush().unwrap();
        for i in 5..10 {
            w.append(&ev(i, i)).unwrap();
        }
        // Flush-and-drop, not finish: finish now seals, and a sealed segment
        // *refuses* on corruption rather than salvaging. The tolerant path
        // under test here is the open/crashed segment's.
        w.flush().unwrap();
        drop(w);

        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte inside the *second* chunk's payload.
        let len = bytes.len();
        bytes[len - 2] ^= 0xFF;
        let r = read_bytes(&bytes).unwrap();
        assert_eq!(r.events.len(), 5, "first chunk survives");
        assert!(r.stopped.is_some(), "damage must be reported");
        let _ = std::fs::remove_file(&path);
    }

    /// Frames get their own chunks, so retention can drop them and keep the
    /// rest (specs/history.md decision 3 — "frames-separable").
    #[test]
    fn frames_never_share_a_chunk_with_other_events() {
        let path = tmp("separable.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Plain, 0).unwrap();
        w.append(&ev(0, 1)).unwrap();
        w.append(&frame(1, 32)).unwrap(); // forces a flush of the event chunk
        w.append(&ev(2, 2)).unwrap(); // forces a flush of the frame chunk
        w.finish().unwrap();

        // Walk the chunks and assert each is homogeneous. The walk stops
        // where the seal region begins — finish() seals now, and the seal is
        // not a chunk.
        let bytes = std::fs::read(&path).unwrap();
        let chunks_end = seal_region(&bytes).expect("finish seals").0;
        let (_, mut pos) = Header::decode(&bytes).unwrap();
        let mut chunks = 0;
        while pos < chunks_end {
            let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let raw = &bytes[pos + 9..pos + 9 + len];
            let evs = decode_chunk(raw).unwrap();
            let frames = evs.iter().filter(|e| matches!(e.event, Event::Frame { .. })).count();
            assert!(
                frames == 0 || frames == evs.len(),
                "chunk mixes frames and events: {evs:?}"
            );
            chunks += 1;
            pos += 9 + len;
        }
        assert_eq!(chunks, 3, "expected event | frame | event chunks");
        let _ = std::fs::remove_file(&path);
    }

    /// An idle desktop must not write. With nothing appended there is no
    /// deadline, so flushing is a no-op and the file never grows.
    #[test]
    fn idle_writes_nothing() {
        let path = tmp("idle.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Zstd, 3).unwrap();
        let after_header = std::fs::metadata(&path).unwrap().len();
        assert!(!w.flush_due(), "an empty writer has no deadline");
        w.flush().unwrap();
        w.flush().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), after_header);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tiers_are_visible_to_a_reader() {
        let path = tmp("tiers.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Plain, 0).unwrap();
        w.append(&ev(0, 1)).unwrap();
        w.append(&Stamped {
            dt_ms: 5,
            tier: T2_SEALED,
            event: Event::Window(WindowState {
                id: 2,
                x: 0,
                y: 0,
                w: 10,
                h: 10,
                title: "vault".into(),
                app: "vault".into(),
                vector: true,
                tier: T2_SEALED,
            }),
        })
        .unwrap();
        w.finish().unwrap();
        let r = read(&path).unwrap();
        assert_eq!(tiers_present(&r.events), vec![T0_ROUTINE, T2_SEALED]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn absolute_times_sum_the_deltas() {
        let events = vec![ev(0, 1), ev(30, 2), ev(1000, 3)];
        let abs = absolute_times(&events);
        assert_eq!(abs.iter().map(|(t, _)| *t).collect::<Vec<_>>(), vec![0, 30, 1030]);
    }

    #[test]
    fn a_foreign_file_is_refused() {
        assert!(matches!(read_bytes(b"not a segment at all"), Err(SegmentError::BadMagic)));
    }
    fn text(dt: u32, id: u32, tier: Tier, body: &str) -> Stamped {
        Stamped { dt_ms: dt, tier, event: Event::Text { id, text: body.into() } }
    }

    /// finish() seals: footer facts match the events, the stored per-tier
    /// indexes equal what a rebuild would produce, and search works straight
    /// off the stored copy — the whole point of paying for the seal.
    #[test]
    fn a_finished_segment_carries_a_seal_that_answers_for_it() {
        use crate::event::{T0_ROUTINE, T2_SEALED};
        let path = tmp("sealed.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Zstd, 3).unwrap();
        w.append(&text(10, 1, T0_ROUTINE, "the quick brown fox")).unwrap();
        w.append(&frame(5, 64)).unwrap();
        w.append(&text(5, 2, T2_SEALED, "a private thing")).unwrap();
        w.append(&text(80, 1, T0_ROUTINE, "jumps over the lazy dog")).unwrap();
        w.finish().unwrap();

        let r = read(&path).unwrap();
        let seal = r.seal.as_ref().expect("finish seals");
        assert_eq!(seal.events, r.events.len() as u64);
        assert_eq!(seal.span, (10, 100));
        assert_eq!(seal.tiers, vec![T0_ROUTINE, T2_SEALED]);
        assert!(seal.chunks >= 2, "frames and text cannot share a chunk");

        // The stored indexes are the built indexes, tier for tier.
        for idx in &seal.indexes {
            assert_eq!(idx, &crate::index::build(&r.events, idx.tier), "tier {}", idx.tier);
        }
        // And they answer without touching the events: T0 finds its text,
        // and the sealed tier's text is not in the T0 index.
        let t0 = seal.indexes.iter().find(|i| i.tier == T0_ROUTINE).expect("t0 stored");
        assert_eq!(t0.search("lazy dog").len(), 1);
        assert!(t0.search("private").is_empty(), "T2 text leaked into the T0 index");
        let _ = std::fs::remove_file(&path);
    }

    /// Sealing twice is once: the recovery path may race a clean close, and
    /// the second seal must notice the first rather than stacking another.
    #[test]
    fn sealing_is_idempotent() {
        let path = tmp("reseal.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Plain, 0).unwrap();
        w.append(&ev(1, 1)).unwrap();
        w.finish().unwrap();
        let once = std::fs::read(&path).unwrap();
        seal_path(&path).unwrap();
        assert_eq!(once, std::fs::read(&path).unwrap(), "a second seal changed the file");
        let _ = std::fs::remove_file(&path);
    }

    /// A crashed writer's segment — torn tail included — seals on recovery:
    /// the torn bytes are cut (they were never durable), everything that was
    /// survives, and the file now promises wholeness like any other.
    #[test]
    fn a_crashed_segment_seals_on_recovery() {
        let path = tmp("recover.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Plain, 0).unwrap();
        for i in 0..5 {
            w.append(&ev(i, i)).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        // The crash: half a chunk header of garbage at the tail.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xDE, 0xAD, 0xBE]).unwrap();
        }
        assert!(read(&path).unwrap().stopped.is_some(), "the tear is visible before recovery");

        seal_path(&path).unwrap();
        let r = read(&path).unwrap();
        assert!(r.stopped.is_none());
        assert_eq!(r.events.len(), 5, "everything durable survived recovery");
        assert_eq!(r.seal.expect("sealed on recovery").events, 5);
        let _ = std::fs::remove_file(&path);
    }

    /// A sealed segment refuses tampering instead of salvaging around it —
    /// wherever the byte lands: chunk payload, stored index, or footer.
    #[test]
    fn tampering_anywhere_breaks_the_seal() {
        use crate::event::T0_ROUTINE;
        let path = tmp("tamper.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Plain, 0).unwrap();
        w.append(&text(1, 1, T0_ROUTINE, "evidence of the thing")).unwrap();
        w.append(&ev(1, 2)).unwrap();
        w.finish().unwrap();
        let clean = std::fs::read(&path).unwrap();
        let (seal_start, seal_end, _) = seal_region(&clean).expect("sealed");

        // One flip in the chunk region, one in the seal region, one in the
        // footer's hashes near the end.
        for at in [seal_start - 3, seal_start + 20, seal_end - 3] {
            let mut bytes = clean.clone();
            bytes[at] ^= 0x01;
            match read_bytes(&bytes) {
                Err(SegmentError::SealBroken(_)) => {}
                Ok(_) => panic!("a flipped byte at {at} read back as fine"),
                Err(e) => panic!("flip at {at}: expected SealBroken, got {e}"),
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The corpus fast path: header and seal without decoding a chunk, and
    /// an honest None for a segment that has no seal to read.
    #[test]
    fn read_seal_matches_the_full_read_and_knows_when_there_is_none() {
        use crate::event::T0_ROUTINE;
        let path = tmp("fastseal.rhs");
        let mut w = SegmentWriter::create(&path, &header(), ChunkCodec::Zstd, 3).unwrap();
        w.append(&text(1, 1, T0_ROUTINE, "findable words here")).unwrap();
        w.finish().unwrap();

        let (h, fast) = read_seal(&path).unwrap().expect("sealed");
        let full = read(&path).unwrap();
        assert_eq!(h, full.header);
        assert_eq!(Some(fast), full.seal);

        let open = tmp("fastseal-open.rhs");
        let mut w = SegmentWriter::create(&open, &header(), ChunkCodec::Plain, 0).unwrap();
        w.append(&ev(1, 1)).unwrap();
        w.flush().unwrap();
        drop(w);
        assert!(read_seal(&open).unwrap().is_none(), "an open segment has no seal");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&open);
    }

    /// The whole life of an encrypted segment: written under a wrapped data
    /// key, sealed, read back with the KEK — events, stored index and all —
    /// and honestly *locked* to a reader without it or with the wrong one.
    #[test]
    fn an_encrypted_segment_opens_with_its_key_and_only_its_key() {
        use crate::crypt::Kek;
        use crate::event::T0_ROUTINE;
        let path = tmp("enc.rhs");
        let kek = Kek::from_bytes([42; 32]);
        let mut w =
            SegmentWriter::create_with_key(&path, &header(), ChunkCodec::Zstd, 3, Some(&kek))
                .unwrap();
        w.append(&text(10, 1, T0_ROUTINE, "the secret transcript")).unwrap();
        w.append(&frame(5, 4096)).unwrap();
        w.finish().unwrap();

        // Nothing readable sits in the file: the text must not appear in
        // the raw bytes (the whole point of at-rest encryption).
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(6).any(|w| w == b"secret"),
            "plaintext leaked into an encrypted segment"
        );

        // The right key: everything, including the stored index.
        let r = read_with(&path, Some(&kek)).unwrap();
        assert_eq!(r.events.len(), 2);
        let idx = &r.seal.as_ref().unwrap().indexes[0];
        assert_eq!(idx.search("secret transcript").len(), 1);

        // No key, wrong key: locked, not empty.
        assert!(matches!(read(&path), Err(SegmentError::Locked(_))));
        assert!(matches!(
            read_with(&path, Some(&Kek::from_bytes([43; 32]))),
            Err(SegmentError::Locked(_))
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// Crash recovery under encryption: a killed writer's segment seals on
    /// the next start with the same KEK, torn tail cut, events intact.
    #[test]
    fn an_encrypted_crash_recovers_with_the_key() {
        use crate::crypt::Kek;
        let path = tmp("enc-crash.rhs");
        let kek = Kek::from_bytes([9; 32]);
        let mut w =
            SegmentWriter::create_with_key(&path, &header(), ChunkCodec::Zstd, 3, Some(&kek))
                .unwrap();
        for i in 0..5 {
            w.append(&ev(i, i)).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xBA, 0xD0]).unwrap();
        }
        // Recovery without the key is refused — sealing means reading.
        assert!(matches!(seal_path(&path), Err(SegmentError::Locked(_))));
        seal_path_with(&path, Some(&kek)).unwrap();
        let r = read_with(&path, Some(&kek)).unwrap();
        assert_eq!(r.events.len(), 5);
        assert!(r.seal.is_some());
        let _ = std::fs::remove_file(&path);
    }

}
