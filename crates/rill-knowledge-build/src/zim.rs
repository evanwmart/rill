//! A reader for the ZIM archive format (openzim.org), just enough for a
//! Kiwix Wikipedia export: the directory, path lookup, redirects, and
//! zstd clusters. Written against the format spec rather than a binding:
//! the pure-Rust `zim` crate drags in clap, indicatif, rayon and liblzma,
//! and libzim bindings are a C++ build — for ~200 lines of format this
//! archive needs (ZIM 6, single `C` namespace, zstd or stored clusters).
//!
//! Layout (all little-endian):
//!
//! ```text
//! header 80 B: magic 0x044D495A, major u16, minor u16, uuid 16 B,
//!   entryCount u32, clusterCount u32, urlPtrPos u64, titlePtrPos u64,
//!   clusterPtrPos u64, mimeListPos u64, mainPage u32, layoutPage u32,
//!   checksumPos u64
//! mime list: NUL-terminated strings, ended by an empty one
//! url pointers: entryCount × u64, sorted by (namespace, url)
//! directory entry: mime u16, paramLen u8, namespace u8, revision u32,
//!   then  redirect: index u32                       (mime == 0xFFFF)
//!   or    item:     cluster u32, blob u32
//!   then url NUL, title NUL
//! cluster: info u8 (low nibble 1 = stored, 5 = zstd; bit 4 = 64-bit
//!   offsets), then [offsets (u32 each), blobs…] — compressed as one
//!   zstd frame when the nibble says so. offsets[0] / 4 = blob count + 1.
//! ```

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const MAGIC: u32 = 0x044D_495A;
const REDIRECT_MIME: u16 = 0xFFFF;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Redirect(u32),
    Item { cluster: u32, blob: u32 },
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub mime: u16,
    pub namespace: u8,
    pub url: String,
    pub kind: Kind,
}

/// One decompressed cluster: every blob it holds, addressable by index.
pub struct Cluster {
    data: Vec<u8>,
    offsets: Vec<usize>,
}

impl Cluster {
    pub fn blob(&self, i: u32) -> Option<&[u8]> {
        let i = i as usize;
        let (a, b) = (*self.offsets.get(i)?, *self.offsets.get(i + 1)?);
        self.data.get(a..b)
    }

}

pub struct Zim {
    file: File,
    entries: Vec<Entry>,
    cluster_ptrs: Vec<u64>,
    cluster_end: u64,
    pub mime_types: Vec<String>,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Parse one directory entry from `b` (which starts at the entry).
/// Returns the entry and the bytes consumed.
fn parse_entry(b: &[u8]) -> io::Result<(Entry, usize)> {
    if b.len() < 12 {
        return Err(bad("directory entry truncated"));
    }
    let mime = u16_at(b, 0);
    let namespace = b[3];
    let (kind, mut at) = if mime == REDIRECT_MIME {
        (Kind::Redirect(u32_at(b, 8)), 12)
    } else {
        if b.len() < 16 {
            return Err(bad("directory entry truncated"));
        }
        (Kind::Item { cluster: u32_at(b, 8), blob: u32_at(b, 12) }, 16)
    };
    let cstr = |at: &mut usize| -> io::Result<String> {
        let start = *at;
        let end = b[start..].iter().position(|&c| c == 0).ok_or_else(|| bad("unterminated string"))? + start;
        *at = end + 1;
        String::from_utf8(b[start..end].to_vec()).map_err(|_| bad("non-UTF-8 entry string"))
    };
    let url = cstr(&mut at)?;
    let _title = cstr(&mut at)?;
    Ok((Entry { mime, namespace, url, kind }, at))
}

impl Zim {
    pub fn open(path: &Path) -> io::Result<Zim> {
        let mut file = File::open(path)?;
        let mut h = [0u8; 80];
        file.read_exact(&mut h)?;
        if u32_at(&h, 0) != MAGIC {
            return Err(bad("not a ZIM file"));
        }
        let entry_count = u32_at(&h, 24) as usize;
        let cluster_count = u32_at(&h, 28) as usize;
        let url_ptr_pos = u64_at(&h, 32);
        let cluster_ptr_pos = u64_at(&h, 48);
        let mime_list_pos = u64_at(&h, 56);
        let checksum_pos = u64_at(&h, 72);

        // Mime list.
        file.seek(SeekFrom::Start(mime_list_pos))?;
        let mut buf = Vec::new();
        {
            let mut chunk = [0u8; 4096];
            loop {
                let n = file.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(2).any(|w| w == [0, 0]) {
                    break;
                }
            }
        }
        let mut mime_types = Vec::new();
        for s in buf.split(|&c| c == 0) {
            if s.is_empty() {
                break;
            }
            mime_types.push(String::from_utf8_lossy(s).into_owned());
        }

        // URL pointers, then every entry, eagerly: the directory is a few
        // tens of MB and we look up ~100k paths — random seeks would cost
        // more than the memory.
        file.seek(SeekFrom::Start(url_ptr_pos))?;
        let mut raw = vec![0u8; entry_count * 8];
        file.read_exact(&mut raw)?;
        let url_ptrs: Vec<u64> = (0..entry_count).map(|i| u64_at(&raw, i * 8)).collect();
        let lo = url_ptrs.iter().copied().min().unwrap_or(0);
        let hi = url_ptrs.iter().copied().max().unwrap_or(0);
        let span_end = hi + 65536; // the last entry is small; generous slack
        file.seek(SeekFrom::Start(lo))?;
        let mut dir = vec![0u8; (span_end - lo) as usize];
        let got = read_up_to(&mut file, &mut dir)?;
        dir.truncate(got);
        let mut entries = Vec::with_capacity(entry_count);
        for p in &url_ptrs {
            let at = (p - lo) as usize;
            let (e, _) = parse_entry(dir.get(at..).ok_or_else(|| bad("entry pointer past directory"))?)?;
            entries.push(e);
        }

        file.seek(SeekFrom::Start(cluster_ptr_pos))?;
        let mut raw = vec![0u8; cluster_count * 8];
        file.read_exact(&mut raw)?;
        let cluster_ptrs: Vec<u64> = (0..cluster_count).map(|i| u64_at(&raw, i * 8)).collect();
        let cluster_end = if checksum_pos != u64::MAX { checksum_pos } else { file.metadata()?.len() };

        Ok(Zim {
            file,
            entries,
            cluster_ptrs,
            cluster_end,
            mime_types,
        })
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn entry(&self, idx: u32) -> Option<&Entry> {
        self.entries.get(idx as usize)
    }

    /// Index of the entry at `namespace`/`url`, by binary search over the
    /// (namespace, url)-sorted directory.
    pub fn lookup(&self, namespace: u8, url: &str) -> Option<u32> {
        self.entries
            .binary_search_by(|e| (e.namespace, e.url.as_str()).cmp(&(namespace, url)))
            .ok()
            .map(|i| i as u32)
    }

    /// Follow redirects (bounded) to an item entry. Returns the final index
    /// and how many hops it took.
    pub fn resolve(&self, mut idx: u32) -> Option<(u32, u32)> {
        let mut hops = 0;
        loop {
            match &self.entry(idx)?.kind {
                Kind::Item { .. } => return Some((idx, hops)),
                Kind::Redirect(to) => {
                    hops += 1;
                    if hops > 8 {
                        return None;
                    }
                    idx = *to;
                }
            }
        }
    }

    /// Read and decompress one cluster.
    pub fn cluster(&mut self, c: u32) -> io::Result<Cluster> {
        let start = *self.cluster_ptrs.get(c as usize).ok_or_else(|| bad("cluster index out of range"))?;
        let end = self.cluster_ptrs.get(c as usize + 1).copied().unwrap_or(self.cluster_end);
        if end <= start {
            return Err(bad("cluster has no extent"));
        }
        self.file.seek(SeekFrom::Start(start))?;
        let mut raw = vec![0u8; (end - start) as usize];
        self.file.read_exact(&mut raw)?;
        let info = raw[0];
        let extended = info & 0x10 != 0;
        let body = match info & 0x0F {
            1 => raw[1..].to_vec(),
            5 => zstd::stream::decode_all(&raw[1..]).map_err(|e| bad(format!("zstd cluster {c}: {e}")))?,
            other => return Err(bad(format!("cluster {c}: unsupported compression {other}"))),
        };
        let word = if extended { 8 } else { 4 };
        if body.len() < word {
            return Err(bad("cluster body truncated"));
        }
        let first = if extended { u64_at(&body, 0) as usize } else { u32_at(&body, 0) as usize };
        let count = first / word;
        if count == 0 || first > body.len() {
            return Err(bad("cluster offset table malformed"));
        }
        let mut offsets = Vec::with_capacity(count);
        for i in 0..count {
            let o = if extended { u64_at(&body, i * 8) as usize } else { u32_at(&body, i * 4) as usize };
            if o > body.len() || offsets.last().is_some_and(|&p| o < p) {
                return Err(bad("cluster offsets not monotonic"));
            }
            offsets.push(o);
        }
        Ok(Cluster { data: body, offsets })
    }
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let n = file.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_item_and_redirect_entries() {
        let mut b = vec![9, 0, 0, b'C', 0, 0, 0, 0, 67, 0, 0, 0, 35, 0, 0, 0];
        b.extend_from_slice(b"April\0\0");
        let (e, used) = parse_entry(&b).unwrap();
        assert_eq!(used, b.len());
        assert_eq!(e.url, "April");
        assert_eq!(e.kind, Kind::Item { cluster: 67, blob: 35 });

        let mut r = vec![0xFF, 0xFF, 0, b'C', 0, 0, 0, 0, 7, 0, 0, 0];
        r.extend_from_slice(b"April_01\0April 01\0");
        let (e, used) = parse_entry(&r).unwrap();
        assert_eq!(used, r.len());
        assert_eq!(e.kind, Kind::Redirect(7));
        assert_eq!(e.url, "April_01");
    }
}
