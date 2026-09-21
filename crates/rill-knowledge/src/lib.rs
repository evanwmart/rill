//! Knowledge packs (`specs/knowledge.md`): a files-only retrieval substrate.
//!
//! The durable form is a directory of UTF-8 text files — chunk text shards
//! with fixed-width offset tables, Base64URL int8 vectors in fixed-width
//! rows, a semantic tree whose every node is an explicit resource, and
//! sorted postings files — laid out as a Rill resource tree so the same
//! bytes are a local directory, an installed `.rillpack`, and a remotely
//! GET-able tree. This crate is `std` plus BLAKE3 (through `rill-store`),
//! synchronous, and never touches the network.
//!
//! * [`base64`] — the URL-safe alphabet, no padding, fixed width.
//! * [`text`] — chunk records, escaping, shards and offset tables (§4.2).
//! * [`doc`] — the document table (§4.1).
//! * [`vector`] — int8 quantisation, cosine, fixed-width shards (§5).
//! * [`coarse`] — the CountSketch projection (§6).
//! * [`node`] — semantic tree nodes and leaf members (§7).
//! * [`postings`] — delta + LEB128 + Base64URL id lists (§8).
//! * [`manifest`] — `key=value` manifest (§9).
//! * [`Pack`] — a read handle over an extracted pack directory.

pub mod base64;
pub mod coarse;
pub mod doc;
pub mod manifest;
pub mod node;
pub mod postings;
pub mod text;
pub mod vector;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Chunks (and documents) per shard. Fixed for version 1; the manifest
/// repeats it so a reader can refuse a pack built with another value.
pub const SHARD: u64 = 8192;
/// Full embedding width.
pub const DIM: usize = 384;
/// Coarse projection width.
pub const COARSE_DIM: usize = 64;
/// Chunk text cap, in bytes before escaping (§4.2).
pub const MAX_CHUNK_BYTES: usize = 2400;

/// Dense chunk id (§4.2).
pub type ChunkId = u64;
/// Dense document id (§4.1).
pub type DocId = u32;

/// Shard number and row of a chunk or document.
pub fn shard_of(id: u64) -> (u32, u64) {
    ((id / SHARD) as u32, id % SHARD)
}

/// `NNNN`: a shard number as four lowercase hex digits.
pub fn shard_name(shard: u32) -> String {
    format!("{shard:04x}")
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A file's bytes do not follow the format; the message says where.
    Format(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Format(m) => write!(f, "format: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn format_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Format(msg.into()))
}

/// An extracted pack on disk: the directory that holds `manifest`.
///
/// Opening reads and validates the manifest only; everything else is read
/// on demand with seeks, so a handle is cheap and holds no shard in memory.
pub struct Pack {
    root: PathBuf,
    manifest: manifest::Manifest,
}

impl Pack {
    pub fn open(root: impl AsRef<Path>) -> Result<Pack> {
        let root = root.as_ref().to_path_buf();
        let manifest = manifest::Manifest::read(&root.join("manifest"))?;
        if manifest.shard() != SHARD {
            return format_err(format!("manifest shard={} but this reader is built for {SHARD}", manifest.shard()));
        }
        Ok(Pack { root, manifest })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &manifest::Manifest {
        &self.manifest
    }

    pub fn chunk_count(&self) -> u64 {
        self.manifest.chunks()
    }

    pub fn doc_count(&self) -> u64 {
        self.manifest.documents()
    }

    /// One chunk's record: two seeks and a line (§4.2).
    pub fn chunk(&self, id: ChunkId) -> Result<text::Chunk> {
        if id >= self.chunk_count() {
            return format_err(format!("chunk {id:#x} out of range"));
        }
        let (shard, row) = shard_of(id);
        text::read_chunk(&self.root.join("text"), shard, row)
    }

    /// One document's row (§4.1): a bounded scan of its shard.
    pub fn doc(&self, id: DocId) -> Result<doc::Doc> {
        if u64::from(id) >= self.doc_count() {
            return format_err(format!("doc {id:#x} out of range"));
        }
        let (shard, _) = shard_of(u64::from(id));
        doc::read_doc(&self.root.join("doc"), shard, id)
    }

    /// One chunk's full int8 vector (§5).
    pub fn full_vector(&self, id: ChunkId) -> Result<Vec<i8>> {
        let (shard, row) = shard_of(id);
        vector::read_row(&self.root.join("vector/full"), shard, row, DIM)
    }

    /// One chunk's coarse int8 vector (§6).
    pub fn coarse_vector(&self, id: ChunkId) -> Result<Vec<i8>> {
        let (shard, row) = shard_of(id);
        vector::read_row(&self.root.join("vector/coarse"), shard, row, COARSE_DIM)
    }

    /// A whole shard of full vectors, for scans (brute force, reranking).
    pub fn full_shard(&self, shard: u32) -> Result<vector::Shard> {
        vector::Shard::read(&self.root.join("vector/full"), shard, DIM)
    }

    pub fn coarse_shard(&self, shard: u32) -> Result<vector::Shard> {
        vector::Shard::read(&self.root.join("vector/coarse"), shard, COARSE_DIM)
    }

    pub fn semantic_root(&self) -> PathBuf {
        self.root.join("semantic/root")
    }
}
