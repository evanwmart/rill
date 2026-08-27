//! Rill content addressing (`specs/resource-format.md`):
//!
//! * [`Hash`] — BLAKE3-256, the identity of a resource's raw bytes;
//! * [`ObjectStore`] — `objects/xx/…` content-addressed storage, verified on
//!   every read (corrupt entries are deleted);
//! * [`RefIndex`] — `(authority, path) → hash` bindings;
//! * [`Cache`] — the two composed, as used by rill-client.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// A BLAKE3-256 content hash. Displays as `blake3:<64 hex>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Hash {
        Hash(*blake3::hash(bytes).as_bytes())
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    pub fn from_hex(hex: &str) -> Option<Hash> {
        let hex = hex.strip_prefix("blake3:").unwrap_or(hex);
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let s = std::str::from_utf8(chunk).ok()?;
            bytes[i] = u8::from_str_radix(s, 16).ok()?;
        }
        Some(Hash(bytes))
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blake3:{}", self.to_hex())
    }
}

/// Transport encodings (resource-format.md §8). Compression is transport-only:
/// hashes, the cache, and metadata always describe decoded bytes.
pub mod encoding {
    use std::io::{self, Read, Write};
    use std::path::Path;

    /// Extensions whose content is already compressed (plan § Resource
    /// Phase 2 table): compressing again wastes CPU for ~0% gain.
    pub const SKIP_COMPRESS_EXT: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "mp3", "m4a", "aac", "ogg",
        "opus", "flac", "mp4", "mkv", "webm", "mov", "zip", "gz", "zst", "xz", "bz2", "7z",
        "rar", "br",
    ];

    /// Is this path's content worth attempting to compress?
    pub fn compressible_path(path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => !SKIP_COMPRESS_EXT.contains(&ext.to_ascii_lowercase().as_str()),
            None => true,
        }
    }

    /// Streaming zstd compressor with bounded memory: feed input with
    /// [`write`](Compressor::write), drain full output chunks as they
    /// accumulate, then [`finish`](Compressor::finish) for the tail.
    pub struct Compressor {
        encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
    }

    impl Compressor {
        pub fn new(level: i32) -> io::Result<Compressor> {
            Ok(Compressor { encoder: zstd::stream::write::Encoder::new(Vec::new(), level)? })
        }

        pub fn write(&mut self, input: &[u8]) -> io::Result<()> {
            self.encoder.write_all(input)
        }

        /// Take the buffered compressed output if at least `min` bytes have
        /// accumulated (pass 0 to take whatever is there).
        pub fn drain(&mut self, min: usize) -> Vec<u8> {
            let buffer = self.encoder.get_mut();
            if buffer.len() >= min && !buffer.is_empty() {
                std::mem::take(buffer)
            } else {
                Vec::new()
            }
        }

        /// Flush the stream's end marker and return all remaining output.
        pub fn finish(self) -> io::Result<Vec<u8>> {
            self.encoder.finish()
        }
    }

    /// One-shot compression (small inputs, tests).
    pub fn compress(bytes: &[u8], level: i32) -> io::Result<Vec<u8>> {
        zstd::stream::encode_all(bytes, level)
    }

    /// Decompress with a hard cap on decoded size — the decompression-bomb
    /// guard (resource-format.md §8). Exceeding `max` is an error, not a
    /// truncation.
    pub fn decompress(bytes: &[u8], max: u64) -> io::Result<Vec<u8>> {
        let mut decoder = zstd::stream::read::Decoder::new(bytes)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = decoder.read(&mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() as u64 + n as u64 > max {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "decoded size exceeds configured cap",
                ));
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{Compressor, compress, decompress};

        #[test]
        fn roundtrip_and_bomb_guard() {
            let input: Vec<u8> = b"compressible ".repeat(10_000);
            let packed = compress(&input, 3).unwrap();
            assert!(packed.len() < input.len() / 10);
            assert_eq!(decompress(&packed, u64::MAX).unwrap(), input);
            // Decoded size over the cap → hard error.
            assert!(decompress(&packed, 1024).is_err());
        }

        #[test]
        fn streaming_matches_one_shot() {
            let input: Vec<u8> = (0..100_000u32).flat_map(|i| i.to_le_bytes()).collect();
            let mut streamed = Vec::new();
            let mut compressor = Compressor::new(3).unwrap();
            for chunk in input.chunks(7_919) {
                compressor.write(chunk).unwrap();
                streamed.extend(compressor.drain(4096));
            }
            streamed.extend(compressor.finish().unwrap());
            assert_eq!(decompress(&streamed, u64::MAX).unwrap(), input);
        }
    }
}

/// Streaming hasher for content too large to hold in memory.
#[derive(Default)]
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Hasher {
        Hasher::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finalize(&self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

/// Content-addressed object storage, sharded by the first hex byte.
/// Every read re-verifies the hash; a mismatch deletes the entry
/// (resource-format.md §3 — "corrupted cache entries are removed").
pub struct ObjectStore {
    dir: PathBuf,
}

impl ObjectStore {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<ObjectStore> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(ObjectStore { dir })
    }

    fn object_path(&self, hash: Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    /// Store bytes; returns their hash. Idempotent — identical content is
    /// stored once. Temp-file + rename, so no torn objects.
    pub fn put(&self, bytes: &[u8]) -> io::Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.object_path(hash);
        if path.exists() {
            return Ok(hash);
        }
        let parent = path.parent().expect("sharded path has parent");
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            &hash.to_hex()[..16]
        ));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    /// Fetch and verify. `Ok(None)` when absent — including when the entry
    /// existed but failed verification and was deleted.
    pub fn get(&self, hash: Hash) -> io::Result<Option<Vec<u8>>> {
        let path = self.object_path(hash);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if Hash::of(&bytes) != hash {
            let _ = std::fs::remove_file(&path); // corrupt: remove
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    pub fn contains(&self, hash: Hash) -> bool {
        self.object_path(hash).exists()
    }

    /// Bytes currently on disk across every shard. Cheap enough to ask
    /// before deciding whether a sweep is worth walking (a stat per object,
    /// no reads), and it is the only honest input to a size budget.
    pub fn total_bytes(&self) -> u64 {
        let Ok(shards) = read_dir_sorted(&self.dir) else { return 0 };
        shards
            .iter()
            .filter(|s| s.is_dir())
            .filter_map(|shard| read_dir_sorted(shard).ok())
            .flatten()
            .filter_map(|object| std::fs::metadata(&object).ok())
            .map(|m| m.len())
            .sum()
    }

    /// Walk every object: (hash claimed by location, verification result).
    pub fn verify_all(&self) -> io::Result<Vec<(String, bool)>> {
        let mut results = Vec::new();
        for shard in read_dir_sorted(&self.dir)? {
            if !shard.is_dir() {
                continue;
            }
            let prefix = file_name(&shard);
            for object in read_dir_sorted(&shard)? {
                let claimed = format!("{prefix}{}", file_name(&object));
                let ok = match Hash::from_hex(&claimed) {
                    Some(hash) => std::fs::read(&object)
                        .map(|bytes| Hash::of(&bytes) == hash)
                        .unwrap_or(false),
                    None => false,
                };
                results.push((claimed, ok));
            }
        }
        Ok(results)
    }
}

fn read_dir_sorted(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    Ok(entries)
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// `(authority, path) → hash` bindings. One file per ref, named by the
/// BLAKE3 of the key, containing `"<authority><path> blake3:<hex>"` for
/// debuggability.
pub struct RefIndex {
    dir: PathBuf,
}

impl RefIndex {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<RefIndex> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(RefIndex { dir })
    }

    fn ref_path(&self, authority: &str, path: &str) -> PathBuf {
        let key = format!("{authority}{path}");
        self.dir.join(Hash::of(key.as_bytes()).to_hex())
    }

    pub fn get(&self, authority: &str, path: &str) -> Option<Hash> {
        let text = std::fs::read_to_string(self.ref_path(authority, path)).ok()?;
        Hash::from_hex(text.rsplit(' ').next()?.trim())
    }

    pub fn set(&self, authority: &str, path: &str, hash: Hash) -> io::Result<()> {
        std::fs::write(self.ref_path(authority, path), format!("{authority}{path} {hash}\n"))
    }

    pub fn count(&self) -> io::Result<usize> {
        Ok(read_dir_sorted(&self.dir)?.len())
    }
}

/// The client cache: refs + objects under one root
/// (`~/.cache/rill` by default; resource-format.md §4).
pub struct Cache {
    pub objects: ObjectStore,
    pub refs: RefIndex,
    root: PathBuf,
}

impl Cache {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Cache> {
        let root = root.into();
        Ok(Cache {
            objects: ObjectStore::open(root.join("objects"))?,
            refs: RefIndex::open(root.join("refs"))?,
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The hash we'd send in GET_IF, if we hold both the ref and a
    /// verifiable object for it.
    pub fn known_hash(&self, authority: &str, path: &str) -> Option<Hash> {
        let hash = self.refs.get(authority, path)?;
        self.objects.contains(hash).then_some(hash)
    }

    /// Serve from cache, re-verifying the object (corrupt → None).
    pub fn lookup(&self, authority: &str, path: &str) -> Option<(Hash, Vec<u8>)> {
        let hash = self.refs.get(authority, path)?;
        let bytes = self.objects.get(hash).ok()??;
        Some((hash, bytes))
    }

    /// Store verified bytes and bind the ref.
    pub fn store(&self, authority: &str, path: &str, bytes: &[u8]) -> io::Result<Hash> {
        let hash = self.objects.put(bytes)?;
        self.refs.set(authority, path, hash)?;
        Ok(hash)
    }

    /// Default ceiling for [`Cache::sweep_if_due`].
    ///
    /// Sized against a measurement, not a feeling: the cache on this machine
    /// held 225 MiB of which 9.9 MiB was reachable. A budget has to sit far
    /// enough above the working set that a normal desktop never sweeps, and
    /// far enough below the leak that it always does. 64 MiB is ~6× the
    /// measured working set and about two days of widget churn.
    pub const DEFAULT_BUDGET: u64 = 64 * 1024 * 1024;

    /// How often a sweep may run, whatever else happens. Walking the store
    /// is cheap but not free, and every client process opens the cache.
    const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

    /// Delete objects no ref points at, if the store has outgrown `budget`
    /// and no sweep has run recently.
    ///
    /// A content-addressed store with mutable refs is a garbage generator: a
    /// page that re-serves on a clock writes a new object per *change*, and
    /// the ref moves on while the old object stays forever. A terminal or a
    /// widget on a one-second clock is therefore a slow disk leak — measured
    /// at 225 MiB on this machine, of which 9.9 MiB was reachable. Nothing
    /// was wrong with the objects; nothing ever removed them.
    ///
    /// Unreachable is the only thing collected, so this can never delete
    /// something a `GET_IF` was about to match: `known_hash` already checks
    /// that the object exists before offering its hash.
    ///
    /// Best-effort by construction — a cache that fails to collect must
    /// still serve, so I/O errors mid-walk end the sweep rather than
    /// propagate.
    pub fn sweep_if_due(&self, budget: u64) -> Option<Swept> {
        let stamp = self.root.join(".last-sweep");
        if let Ok(meta) = std::fs::metadata(&stamp)
            && let Ok(age) = meta.modified().and_then(|m| m.elapsed().map_err(io_err))
            && age < Cache::SWEEP_INTERVAL
        {
            return None;
        }
        // Touch first: two clients starting together should not both walk.
        let _ = std::fs::write(&stamp, b"");
        let live = self.objects.total_bytes();
        if live <= budget {
            return None;
        }
        self.sweep().ok()
    }

    /// Collect every unreferenced object. Returns what went.
    pub fn sweep(&self) -> io::Result<Swept> {
        let mut keep = std::collections::HashSet::new();
        for entry in read_dir_sorted(&self.refs.dir)? {
            if let Ok(text) = std::fs::read_to_string(&entry)
                && let Some(hash) = text.rsplit(' ').next().and_then(|h| Hash::from_hex(h.trim()))
            {
                keep.insert(hash);
            }
        }
        let mut swept = Swept::default();
        for shard in read_dir_sorted(&self.objects.dir)? {
            if !shard.is_dir() {
                continue;
            }
            let prefix = file_name(&shard);
            for object in read_dir_sorted(&shard)? {
                let name = file_name(&object);
                // Leave in-flight temporaries alone: another process may be
                // mid-`put`, and its rename would land on a deleted path.
                if name.starts_with(".tmp-") {
                    continue;
                }
                let Some(hash) = Hash::from_hex(&format!("{prefix}{name}")) else { continue };
                if keep.contains(&hash) {
                    swept.kept += 1;
                    continue;
                }
                let size = std::fs::metadata(&object).map(|m| m.len()).unwrap_or(0);
                if std::fs::remove_file(&object).is_ok() {
                    swept.removed += 1;
                    swept.freed_bytes += size;
                }
            }
        }
        Ok(swept)
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// What one [`Cache::sweep`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub removed: u64,
    pub kept: u64,
    pub freed_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::{Cache, Hash, ObjectStore};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rill-store-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hash_text_forms() {
        let h = Hash::of(b"hello");
        let text = h.to_string();
        assert!(text.starts_with("blake3:"));
        assert_eq!(Hash::from_hex(&text), Some(h));
        assert_eq!(Hash::from_hex(&h.to_hex()), Some(h));
        assert_eq!(Hash::from_hex("nope"), None);
    }

    #[test]
    fn object_store_roundtrip_dedup_and_corruption() {
        let store = ObjectStore::open(dir()).unwrap();
        let h1 = store.put(b"content").unwrap();
        let h2 = store.put(b"content").unwrap();
        assert_eq!(h1, h2); // stored once
        assert_eq!(store.get(h1).unwrap().unwrap(), b"content");

        // Corrupt the object on disk: read detects, deletes, reports absent.
        let path = store.object_path(h1);
        std::fs::write(&path, b"tampered").unwrap();
        assert_eq!(store.get(h1).unwrap(), None);
        assert!(!path.exists(), "corrupt entry removed");

        assert_eq!(store.get(Hash::of(b"never-stored")).unwrap(), None);
    }

    #[test]
    fn cache_refs_and_shared_objects() {
        let cache = Cache::open(dir()).unwrap();
        let h1 = cache.store("host:1", "/a", b"same-bytes").unwrap();
        let h2 = cache.store("host:1", "/b", b"same-bytes").unwrap();
        assert_eq!(h1, h2); // two refs, one object
        assert_eq!(cache.refs.count().unwrap(), 2);
        assert_eq!(cache.lookup("host:1", "/a").unwrap().1, b"same-bytes");
        assert_eq!(cache.lookup("host:1", "/b").unwrap().1, b"same-bytes");
        // Same path, different authority: distinct ref.
        assert!(cache.lookup("other:1", "/a").is_none());
        assert_eq!(cache.known_hash("host:1", "/a"), Some(h1));
        assert_eq!(cache.known_hash("host:1", "/zzz"), None);
    }

    /// A live page rewrites its document on a clock: the ref moves and the
    /// object it used to name is orphaned. This is the leak that put 225 MiB
    /// of unreachable objects in a real cache — sweep must collect exactly
    /// those, and nothing a ref still names.
    #[test]
    fn sweep_collects_superseded_objects_only() {
        let cache = Cache::open(dir()).unwrap();
        // One path, ten revisions — a widget on a one-second clock.
        for i in 0..10 {
            cache.store("host:1", "/live", format!("tick {i}").as_bytes()).unwrap();
        }
        // And one page that never changes.
        let stable = cache.store("host:1", "/stable", b"unchanging").unwrap();
        assert!(cache.objects.total_bytes() > 0);

        let swept = cache.sweep().unwrap();
        assert_eq!(swept.removed, 9, "nine superseded revisions collected");
        assert_eq!(swept.kept, 2, "the current revision and the stable page stay");

        // The surviving refs still resolve — collection must not break a hit.
        assert_eq!(cache.lookup("host:1", "/live").unwrap().1, b"tick 9");
        assert_eq!(cache.lookup("host:1", "/stable").unwrap().1, b"unchanging");
        assert_eq!(cache.known_hash("host:1", "/stable"), Some(stable));
    }

    /// Sweeping an already-tidy cache is a no-op, not a slow way to delete
    /// everything — the failure mode that would turn every fetch into a miss.
    #[test]
    fn sweep_of_a_reachable_cache_removes_nothing() {
        let cache = Cache::open(dir()).unwrap();
        cache.store("host:1", "/a", b"aaa").unwrap();
        cache.store("host:1", "/b", b"bbb").unwrap();
        let swept = cache.sweep().unwrap();
        assert_eq!((swept.removed, swept.kept), (0, 2));
        assert!(cache.lookup("host:1", "/a").is_some());
    }

    /// The budget gate: under it, nothing is walked; over it, collection
    /// happens. And the rate limit holds a second call off regardless.
    #[test]
    fn sweep_if_due_respects_budget_then_rate_limit() {
        let cache = Cache::open(dir()).unwrap();
        for i in 0..5 {
            cache.store("host:1", "/live", format!("revision {i}").as_bytes()).unwrap();
        }
        let size = cache.objects.total_bytes();
        assert!(size > 0);

        // Budget not exceeded: no sweep, and the garbage is still there.
        assert_eq!(cache.sweep_if_due(size + 1024), None, "under budget, no walk");
        assert_eq!(cache.objects.total_bytes(), size, "nothing collected");

        // Over budget: collects. (The stamp written by the call above is
        // what the rate limit then trips on.)
        std::fs::remove_file(cache.root().join(".last-sweep")).unwrap();
        let swept = cache.sweep_if_due(0).expect("over budget, sweeps");
        assert_eq!(swept.removed, 4);

        // Immediately after, the rate limit refuses regardless of budget.
        for i in 5..9 {
            cache.store("host:1", "/live", format!("revision {i}").as_bytes()).unwrap();
        }
        assert_eq!(cache.sweep_if_due(0), None, "rate-limited despite being over budget");
    }

    /// A `.tmp-` file is another process mid-`put`. Collecting it would make
    /// that process's rename land on a path we just deleted.
    #[test]
    fn sweep_leaves_in_flight_temporaries_alone() {
        let cache = Cache::open(dir()).unwrap();
        cache.store("host:1", "/a", b"aaa").unwrap();
        let shard = cache.root().join("objects").join("ab");
        std::fs::create_dir_all(&shard).unwrap();
        let tmp = shard.join(".tmp-1234-deadbeef");
        std::fs::write(&tmp, b"half-written").unwrap();

        cache.sweep().unwrap();
        assert!(tmp.exists(), "in-flight temporary survived the sweep");
    }
}
