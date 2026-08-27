//! The `.rillpack` format (`specs/resource-format.md` §9): one deterministic,
//! indexed, random-access artifact holding a complete site or application.
//!
//! * [`PackBuilder`] — deterministic builds: sorted index, fixed zstd level,
//!   attempt-and-compare per-resource compression, no timestamps;
//! * [`Pack`] — strict reader: structure validated on open (including index
//!   sort order — determinism is enforced, not just promised), resources
//!   extracted by binary search + one ranged read, always hash-verified;
//! * [`Pack::verify`] — footer hash + every resource.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use rill_protocol::validate_path;
use rill_store::{Hash, encoding};

pub const MAGIC: [u8; 4] = *b"RPCK";
pub const TAIL_MAGIC: [u8; 4] = *b"KCPR";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: u64 = 48;
pub const ENTRY_LEN: u64 = 64;
pub const FOOTER_LEN: u64 = 36;

pub const ENCODING_RAW: u8 = 0;
pub const ENCODING_ZSTD: u8 = 1;

/// Build-time compression threshold, mirroring the server policy.
pub const COMPRESS_MIN_SIZE: usize = 1024;
pub const ZSTD_LEVEL: i32 = 3;

/// Largest decoded size a single entry may declare.
///
/// `decoded_size` comes out of the pack's own index, and it is what the
/// decompressor is given as its bomb cap — so without a ceiling the attacker
/// sets their own limit. Declaring tens of GB against a blob that genuinely
/// expands that far, with a hash computed to match, is enough to OOM whoever
/// opens it; and `verify` extracts *every* entry, which is exactly what
/// installing an app does before it trusts anything. The hash check catches a
/// corrupt pack, never a deliberate one.
///
/// Matched to the client's `DEFAULT_MAX_RESOURCE`: a pack is a bundle of the
/// same resources, so one entry has no business being larger than one fetch.
pub const MAX_DECODED_SIZE: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct PackError(pub String);

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PackError {}

impl From<io::Error> for PackError {
    fn from(e: io::Error) -> PackError {
        PackError(e.to_string())
    }
}

fn err(m: impl Into<String>) -> PackError {
    PackError(m.into())
}

/// One resource's index entry, as read from a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub encoding: u8,
    pub hash: Hash,
    pub blob_offset: u64,
    pub encoded_size: u64,
    pub decoded_size: u64,
}

// ---------------------------------------------------------------- builder

/// Deterministic pack builder: resources are keyed and emitted in sorted
/// path order regardless of insertion order.
#[derive(Default)]
pub struct PackBuilder {
    resources: BTreeMap<String, Vec<u8>>,
}

impl PackBuilder {
    pub fn new() -> PackBuilder {
        PackBuilder::default()
    }

    /// Add a resource by logical path (protocol §7.1 rules apply).
    pub fn add(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), PackError> {
        validate_path(path).map_err(|e| err(format!("{path}: {e}")))?;
        // Check before inserting: a plain `insert` would overwrite the existing
        // entry *before* we report the duplicate, corrupting the builder on a
        // rejected add.
        if self.resources.contains_key(path) {
            return Err(err(format!("{path}: duplicate resource path")));
        }
        self.resources.insert(path.to_string(), bytes);
        Ok(())
    }

    /// Add every regular file under `dir` as `/relative/path`.
    pub fn add_dir(&mut self, dir: &Path) -> Result<(), PackError> {
        fn walk(builder: &mut PackBuilder, root: &Path, dir: &Path) -> Result<(), PackError> {
            let mut entries: Vec<_> =
                std::fs::read_dir(dir)?.collect::<Result<_, _>>().map_err(PackError::from)?;
            entries.sort_by_key(|e| e.path());
            for entry in entries {
                let path = entry.path();
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    walk(builder, root, &path)?;
                } else if kind.is_file() {
                    let rel = path.strip_prefix(root).expect("under root");
                    let logical = format!(
                        "/{}",
                        rel.to_str().ok_or_else(|| err(format!("{path:?}: non-UTF8 name")))?
                    );
                    builder.add(&logical, std::fs::read(&path)?)?;
                }
                // Symlinks and specials are skipped: a pack holds content,
                // not filesystem structure.
            }
            Ok(())
        }
        walk(self, dir, dir)
    }

    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Serialize. Identical inputs yield identical bytes.
    pub fn build(&self) -> Result<Vec<u8>, PackError> {
        if self.resources.is_empty() {
            return Err(err("refusing to build an empty pack"));
        }
        let count = u32::try_from(self.resources.len())
            .map_err(|_| err("too many resources"))?;

        // Encode blobs first (order = BTreeMap = sorted path order).
        struct Encoded<'a> {
            path: &'a str,
            encoding: u8,
            hash: Hash,
            blob: Vec<u8>,
            decoded_size: u64,
        }
        let mut encoded = Vec::with_capacity(self.resources.len());
        let mut string_table = Vec::new();
        for (path, bytes) in &self.resources {
            let hash = Hash::of(bytes);
            let try_compress = bytes.len() >= COMPRESS_MIN_SIZE
                && encoding::compressible_path(Path::new(path));
            let (enc, blob) = if try_compress {
                let packed = encoding::compress(bytes, ZSTD_LEVEL)?;
                if packed.len() < bytes.len() {
                    (ENCODING_ZSTD, packed)
                } else {
                    (ENCODING_RAW, bytes.clone())
                }
            } else {
                (ENCODING_RAW, bytes.clone())
            };
            string_table.extend_from_slice(path.as_bytes());
            encoded.push(Encoded {
                path,
                encoding: enc,
                hash,
                blob,
                decoded_size: bytes.len() as u64,
            });
        }

        let string_table_offset = HEADER_LEN;
        let index_offset = string_table_offset + string_table.len() as u64;
        let index_size = count as u64 * ENTRY_LEN;
        let blobs_offset = index_offset + index_size;

        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&string_table_offset.to_be_bytes());
        out.extend_from_slice(&(string_table.len() as u64).to_be_bytes());
        out.extend_from_slice(&index_offset.to_be_bytes());
        out.extend_from_slice(&index_size.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        debug_assert_eq!(out.len() as u64, HEADER_LEN);

        out.extend_from_slice(&string_table);

        let mut path_off: u32 = 0;
        let mut blob_off: u64 = blobs_offset;
        for e in &encoded {
            out.extend_from_slice(&path_off.to_be_bytes());
            out.extend_from_slice(&(e.path.len() as u16).to_be_bytes());
            out.push(e.encoding);
            out.push(0); // reserved (future: resource type)
            out.extend_from_slice(&e.hash.0);
            out.extend_from_slice(&blob_off.to_be_bytes());
            out.extend_from_slice(&(e.blob.len() as u64).to_be_bytes());
            out.extend_from_slice(&e.decoded_size.to_be_bytes());
            path_off += e.path.len() as u32;
            blob_off += e.blob.len() as u64;
        }
        for e in &encoded {
            out.extend_from_slice(&e.blob);
        }

        let digest = Hash::of(&out);
        out.extend_from_slice(&digest.0);
        out.extend_from_slice(&TAIL_MAGIC);
        Ok(out)
    }

    pub fn write_to(&self, path: &Path) -> Result<(), PackError> {
        let bytes = self.build()?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

// ----------------------------------------------------------------- reader

/// Open pack: header + string table + index in memory, blobs read on demand.
pub struct Pack {
    file: File,
    entries: Vec<Entry>,
    file_len: u64,
    blobs_start: u64,
}

impl Pack {
    /// Open and strictly validate structure (not blob contents — see
    /// [`Pack::verify`]). Any malformation is rejected, including an index
    /// that is not strictly sorted.
    pub fn open(path: &Path) -> Result<Pack, PackError> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_LEN + FOOTER_LEN {
            return Err(err("file too small to be a pack"));
        }

        let mut header = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut header)?;
        if header[0..4] != MAGIC {
            return Err(err("bad magic"));
        }
        if header[4] != VERSION {
            return Err(err(format!("unsupported pack version {}", header[4])));
        }
        if header[5..8] != [0, 0, 0] || header[44..48] != [0, 0, 0, 0] {
            return Err(err("reserved header bytes nonzero"));
        }
        let count = u32::from_be_bytes(header[8..12].try_into().unwrap()) as u64;
        let st_off = u64::from_be_bytes(header[12..20].try_into().unwrap());
        let st_size = u64::from_be_bytes(header[20..28].try_into().unwrap());
        let ix_off = u64::from_be_bytes(header[28..36].try_into().unwrap());
        let ix_size = u64::from_be_bytes(header[36..44].try_into().unwrap());

        let footer_start = file_len - FOOTER_LEN;
        if st_off != HEADER_LEN
            || ix_off != st_off.checked_add(st_size).ok_or_else(|| err("overflow"))?
            || ix_size != count.checked_mul(ENTRY_LEN).ok_or_else(|| err("overflow"))?
            || ix_off.checked_add(ix_size).ok_or_else(|| err("overflow"))? > footer_start
            || count == 0
        {
            return Err(err("inconsistent section layout"));
        }
        let blobs_start = ix_off + ix_size;

        let mut tail = [0u8; 4];
        file.seek(SeekFrom::Start(file_len - 4))?;
        file.read_exact(&mut tail)?;
        if tail != TAIL_MAGIC {
            return Err(err("bad tail magic (truncated or not a pack)"));
        }

        let mut string_table = vec![0u8; st_size as usize];
        file.seek(SeekFrom::Start(st_off))?;
        file.read_exact(&mut string_table)?;
        let mut index = vec![0u8; ix_size as usize];
        file.read_exact(&mut index)?;

        let mut entries: Vec<Entry> = Vec::with_capacity(count as usize);
        for chunk in index.as_chunks::<{ ENTRY_LEN as usize }>().0 {
            let path_off = u32::from_be_bytes(chunk[0..4].try_into().unwrap()) as usize;
            let path_len = u16::from_be_bytes(chunk[4..6].try_into().unwrap()) as usize;
            let encoding_byte = chunk[6];
            if chunk[7] != 0 {
                return Err(err("reserved index byte nonzero"));
            }
            if encoding_byte > ENCODING_ZSTD {
                return Err(err(format!("unknown encoding {encoding_byte}")));
            }
            let path_end =
                path_off.checked_add(path_len).ok_or_else(|| err("path range overflow"))?;
            if path_end > string_table.len() {
                return Err(err("path range out of bounds"));
            }
            let path = std::str::from_utf8(&string_table[path_off..path_end])
                .map_err(|_| err("non-UTF8 path"))?
                .to_string();
            validate_path(&path).map_err(|e| err(format!("{path}: {e}")))?;

            let hash = Hash(chunk[8..40].try_into().unwrap());
            let blob_offset = u64::from_be_bytes(chunk[40..48].try_into().unwrap());
            let encoded_size = u64::from_be_bytes(chunk[48..56].try_into().unwrap());
            let decoded_size = u64::from_be_bytes(chunk[56..64].try_into().unwrap());
            if decoded_size > MAX_DECODED_SIZE {
                return Err(err(format!(
                    "{path}: declares a decoded size of {decoded_size} bytes, over the {MAX_DECODED_SIZE} limit"
                )));
            }
            let blob_end = blob_offset
                .checked_add(encoded_size)
                .ok_or_else(|| err("blob range overflow"))?;
            if blob_offset < blobs_start || blob_end > footer_start {
                return Err(err(format!("{path}: blob range out of bounds")));
            }
            if let Some(prev) = entries.last()
                && prev.path.as_bytes() >= path.as_bytes()
            {
                return Err(err("index not strictly sorted by path"));
            }
            entries.push(Entry { path, encoding: encoding_byte, hash, blob_offset, encoded_size, decoded_size });
        }

        Ok(Pack { file, entries, file_len, blobs_start })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| e.path.as_str().cmp(path))
            .ok()
            .map(|i| &self.entries[i])
    }

    /// Extract one resource: ranged read → decode → hash-verify. Never touches
    /// other blobs.
    pub fn get(&mut self, path: &str) -> Result<Option<Vec<u8>>, PackError> {
        let Some(i) = self.entries.binary_search_by(|e| e.path.as_str().cmp(path)).ok() else {
            return Ok(None);
        };
        let entry = self.entries[i].clone();
        let mut blob = vec![0u8; entry.encoded_size as usize];
        self.file.seek(SeekFrom::Start(entry.blob_offset))?;
        self.file.read_exact(&mut blob)?;
        let decoded = match entry.encoding {
            ENCODING_RAW => blob,
            ENCODING_ZSTD => encoding::decompress(&blob, entry.decoded_size)?,
            _ => unreachable!("validated on open"),
        };
        if decoded.len() as u64 != entry.decoded_size {
            return Err(err(format!("{path}: decoded size mismatch")));
        }
        if Hash::of(&decoded) != entry.hash {
            return Err(err(format!("{path}: hash mismatch (corrupt pack)")));
        }
        Ok(Some(decoded))
    }

    /// Full integrity check: footer hash over the whole file, then every
    /// resource extracted and hash-verified.
    pub fn verify(&mut self) -> Result<(), PackError> {
        // Stream the body hash: verify runs on every install/update, and a
        // pack can be as large as an app — the peak allocation here must be
        // the chunk, not the file.
        let mut remaining = self.file_len - FOOTER_LEN;
        self.file.seek(SeekFrom::Start(0))?;
        let mut hasher = rill_store::Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            self.file.read_exact(&mut buf[..want])?;
            hasher.update(&buf[..want]);
            remaining -= want as u64;
        }
        let mut footer_hash = [0u8; 32];
        self.file.read_exact(&mut footer_hash)?;
        if hasher.finalize() != Hash(footer_hash) {
            return Err(err("footer hash mismatch (pack modified or corrupt)"));
        }
        let paths: Vec<String> = self.entries.iter().map(|e| e.path.clone()).collect();
        for path in paths {
            self.get(&path)?
                .ok_or_else(|| err(format!("{path}: vanished during verify")))?;
        }
        let _ = self.blobs_start;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rill-pack-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> PackBuilder {
        let mut b = PackBuilder::new();
        b.add("/index", b"welcome ".repeat(500).to_vec()).unwrap(); // compressible
        b.add("/assets/moon.png", vec![7u8; 4000]).unwrap(); // skip-ext → raw
        b.add("/tiny", b"hi".to_vec()).unwrap(); // below threshold → raw
        b.add("/private/notes", b"secret note ".repeat(300).to_vec()).unwrap();
        b
    }

    /// `decoded_size` is the cap handed to the decompressor, and it is read
    /// out of the pack — so unbounded, it is the attacker's own limit. The
    /// hash check is no defence: a pack built to be hostile hashes correctly,
    /// and `verify` extracts every entry, which is what installing an app
    /// does. The ceiling therefore has to be applied at open, before anything
    /// is decoded.
    #[test]
    fn an_entry_cannot_declare_its_own_decompression_budget() {
        let mut bytes = sample().build().unwrap();
        // Locate the first index entry's decoded_size (entry bytes 56..64).
        let st_size = u64::from_be_bytes(bytes[20..28].try_into().unwrap());
        let ix_off = (HEADER_LEN + st_size) as usize;
        let field = ix_off + 56;
        assert_ne!(
            u64::from_be_bytes(bytes[field..field + 8].try_into().unwrap()),
            0,
            "found the decoded_size field"
        );
        bytes[field..field + 8].copy_from_slice(&(64u64 * 1024 * 1024 * 1024).to_be_bytes());

        let dir = tmp();
        let path = dir.join("bomb.rillpack");
        std::fs::write(&path, &bytes).unwrap();
        let Err(e) = Pack::open(&path) else { panic!("a 64 GiB entry must not open") };
        assert!(e.0.contains("decoded size"), "{}", e.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deterministic_builds() {
        // Same content, different insertion order → identical bytes.
        let a = sample().build().unwrap();
        let mut b2 = PackBuilder::new();
        b2.add("/private/notes", b"secret note ".repeat(300).to_vec()).unwrap();
        b2.add("/tiny", b"hi".to_vec()).unwrap();
        b2.add("/index", b"welcome ".repeat(500).to_vec()).unwrap();
        b2.add("/assets/moon.png", vec![7u8; 4000]).unwrap();
        assert_eq!(a, b2.build().unwrap());
    }

    #[test]
    fn roundtrip_lookup_and_encoding_choices() {
        let dir = tmp();
        let path = dir.join("site.rillpack");
        sample().write_to(&path).unwrap();

        let mut pack = Pack::open(&path).unwrap();
        // Sorted index.
        let paths: Vec<&str> = pack.entries().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/assets/moon.png", "/index", "/private/notes", "/tiny"]);
        // Encoding choices per policy.
        assert_eq!(pack.entry("/index").unwrap().encoding, ENCODING_ZSTD);
        assert_eq!(pack.entry("/assets/moon.png").unwrap().encoding, ENCODING_RAW);
        assert_eq!(pack.entry("/tiny").unwrap().encoding, ENCODING_RAW);
        // Extraction round trips and verifies.
        assert_eq!(pack.get("/index").unwrap().unwrap(), b"welcome ".repeat(500));
        assert_eq!(pack.get("/tiny").unwrap().unwrap(), b"hi");
        assert_eq!(pack.get("/missing").unwrap(), None);
        // Full verify passes.
        pack.verify().unwrap();
    }

    #[test]
    fn build_from_directory_matches_manual() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/x.txt"), b"xx").unwrap();
        std::fs::write(dir.join("top"), b"tt").unwrap();
        let mut b = PackBuilder::new();
        b.add_dir(&dir).unwrap();
        let mut manual = PackBuilder::new();
        manual.add("/a/x.txt", b"xx".to_vec()).unwrap();
        manual.add("/top", b"tt".to_vec()).unwrap();
        assert_eq!(b.build().unwrap(), manual.build().unwrap());
    }

    #[test]
    fn tampering_detected() {
        let dir = tmp();
        let path = dir.join("t.rillpack");
        sample().write_to(&path).unwrap();

        // Flip one byte inside a blob: per-resource get fails, verify fails.
        let mut bytes = std::fs::read(&path).unwrap();
        let raw_entry_off = {
            let mut pack = Pack::open(&path).unwrap();
            pack.verify().unwrap();
            pack.entry("/assets/moon.png").unwrap().blob_offset as usize
        };
        bytes[raw_entry_off] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let mut pack = Pack::open(&path).unwrap();
        assert!(pack.get("/assets/moon.png").unwrap_err().to_string().contains("hash mismatch"));
        assert!(pack.verify().is_err());

        // Truncation is caught at open.
        let short = &bytes[..bytes.len() - 10];
        std::fs::write(&path, short).unwrap();
        assert!(Pack::open(&path).is_err());
    }

    /// verify() streams the body hash in 256 KiB chunks; a pack bigger than
    /// several chunks must still verify clean and still catch a flip in its
    /// final partial chunk.
    #[test]
    fn verify_streams_across_chunk_boundaries() {
        let dir = tmp();
        let path = dir.join("big.rillpack");
        let mut b = PackBuilder::new();
        // .png skips compression, so the blob (and file) really is ~700 KiB.
        let noise: Vec<u8> = (0..700 * 1024u32).map(|i| (i.wrapping_mul(2654435761)) as u8).collect();
        b.add("/big.png", noise).unwrap();
        b.write_to(&path).unwrap();

        let mut pack = Pack::open(&path).unwrap();
        pack.verify().unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        assert!(len > 2 * 256 * 1024, "test premise: pack spans several chunks");
        bytes[len - FOOTER_LEN as usize - 1] ^= 0xFF; // last body byte
        std::fs::write(&path, &bytes).unwrap();
        let mut pack = Pack::open(&path).unwrap();
        assert!(pack.verify().unwrap_err().to_string().contains("footer hash"));
    }

    /// Writes seed inputs for `cargo fuzz run pack_open`. Ignored: run
    /// explicitly with `cargo test -p rill-pack -- --ignored write_fuzz`
    /// when the corpus needs refreshing (the corpus is committed).
    ///
    /// Seeds are *valid* packs plus a few one-byte-off mutations of them.
    /// A structured-format fuzzer that starts from random bytes spends its
    /// whole budget failing the magic check; starting from real artifacts
    /// puts it inside the index and blob validation, which is where the
    /// interesting failures live.
    #[test]
    #[ignore]
    fn write_fuzz_corpus() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz/corpus/pack_open");
        std::fs::create_dir_all(dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            std::fs::write(format!("{dir}/{name}"), bytes).unwrap();
        };

        let sample_bytes = sample().build().unwrap();
        write("sample", &sample_bytes);

        // The smallest legal pack: one tiny raw resource.
        let mut one = PackBuilder::new();
        one.add("/a", b"hi".to_vec()).unwrap();
        write("single-raw", &one.build().unwrap());

        // One that actually compresses, so the zstd branch has a seed.
        let mut zstd = PackBuilder::new();
        zstd.add("/index", b"welcome ".repeat(2000).to_vec()).unwrap();
        write("single-zstd", &zstd.build().unwrap());

        // Several entries, so index ordering and binary search have room.
        let mut many = PackBuilder::new();
        for i in 0..24 {
            many.add(&format!("/r{i:02}"), format!("resource {i} ").repeat(40).into_bytes())
                .unwrap();
        }
        write("many-entries", &many.build().unwrap());

        // Deep and unicode paths: the string table's harder inputs.
        let mut paths = PackBuilder::new();
        paths.add("/a/b/c/d/e/deep.txt", b"deep".to_vec()).unwrap();
        paths.add("/\u{e9}t\u{e9}/caf\u{e9}.txt", "caf\u{e9}".repeat(50).into_bytes()).unwrap();
        write("odd-paths", &paths.build().unwrap());

        // Near-misses: a valid pack with one byte changed. Each should be
        // rejected, and the fuzzer's job is to find the mutation that is
        // not. Offsets chosen to land in the header, the index, and a blob.
        for (name, at) in [
            ("bad-magic", 2usize),
            ("bad-version", 4),
            ("bad-header", 20),
            ("bad-index", HEADER_LEN as usize + 8),
            ("bad-blob", sample_bytes.len() - FOOTER_LEN as usize - 4),
            ("bad-footer", sample_bytes.len() - 8),
        ] {
            let mut broken = sample_bytes.clone();
            if let Some(byte) = broken.get_mut(at) {
                *byte ^= 0xFF;
                write(name, &broken);
            }
        }

        // Truncations: the length-prefix arithmetic's favourite failure.
        for cut in [1usize, HEADER_LEN as usize, sample_bytes.len() / 2] {
            if cut < sample_bytes.len() {
                write(&format!("truncated-{cut}"), &sample_bytes[..cut]);
            }
        }
    }

    #[test]
    fn builder_rejects_bad_input() {
        let mut b = PackBuilder::new();
        assert!(b.add("relative", vec![]).is_err());
        assert!(b.add("/a/../b", vec![]).is_err());
        b.add("/ok", vec![1]).unwrap();
        assert!(b.add("/ok", vec![2]).is_err(), "duplicate path");
        // The rejected duplicate must NOT have overwritten the original bytes.
        assert_eq!(b.resources.get("/ok"), Some(&vec![1]), "duplicate corrupted the original");
        assert!(PackBuilder::new().build().is_err(), "empty pack");
    }
}
