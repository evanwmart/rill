//! The document table (§4.1): `doc/NNNN`, one escaped line per document,
//! `SHARD` per shard. Not offset-indexed — a reader scans one shard.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::text::{escape_into, unescape};
use crate::{ChunkId, DocId, Result, format_err, shard_name};

#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub id: DocId,
    pub page_id: u64,
    /// `Q937`-style Wikidata item, if any.
    pub qid: Option<String>,
    pub first_chunk: ChunkId,
    pub chunk_count: u16,
    pub popularity: f64,
    pub title: String,
}

pub fn format_record(d: &Doc) -> String {
    let mut line = format!(
        "{:08x}\t{}\t{}\t{:016x}\t{:04x}\t{}\t",
        d.id,
        d.page_id,
        d.qid.as_deref().unwrap_or("-"),
        d.first_chunk,
        d.chunk_count,
        d.popularity
    );
    escape_into(&d.title, &mut line);
    line
}

pub fn parse_record(line: &str) -> Result<Doc> {
    let f: Vec<&str> = line.splitn(7, '\t').collect();
    if f.len() != 7 {
        return format_err("doc record: not 7 fields");
    }
    let bad = |what: &str, v: &str| crate::Error::Format(format!("doc record {what} {v:?}"));
    Ok(Doc {
        id: u32::from_str_radix(f[0], 16).map_err(|_| bad("id", f[0]))?,
        page_id: f[1].parse().map_err(|_| bad("page_id", f[1]))?,
        qid: (f[2] != "-").then(|| f[2].to_string()),
        first_chunk: u64::from_str_radix(f[3], 16).map_err(|_| bad("first_chunk", f[3]))?,
        chunk_count: u16::from_str_radix(f[4], 16).map_err(|_| bad("chunk_count", f[4]))?,
        popularity: f[5].parse().map_err(|_| bad("popularity", f[5]))?,
        title: unescape(f[6])?,
    })
}

pub struct Writer {
    dir: PathBuf,
    next_id: DocId,
    shard: Option<(u32, BufWriter<File>)>,
}

impl Writer {
    pub fn create(dir: impl AsRef<Path>) -> Result<Writer> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Writer { dir, next_id: 0, shard: None })
    }

    pub fn next_id(&self) -> DocId {
        self.next_id
    }

    pub fn push(&mut self, d: &Doc) -> Result<()> {
        if d.id != self.next_id {
            return format_err(format!("doc {:#x} written out of order (expected {:#x})", d.id, self.next_id));
        }
        let (shard, row) = crate::shard_of(u64::from(d.id));
        if row == 0 {
            self.finish_shard()?;
            self.shard = Some((shard, BufWriter::new(File::create(self.dir.join(shard_name(shard)))?)));
        }
        let (_, w) = self.shard.as_mut().expect("shard open");
        w.write_all(format_record(d).as_bytes())?;
        w.write_all(b"\n")?;
        self.next_id += 1;
        Ok(())
    }

    fn finish_shard(&mut self) -> Result<()> {
        if let Some((_, mut w)) = self.shard.take() {
            w.flush()?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<DocId> {
        self.finish_shard()?;
        Ok(self.next_id)
    }
}

pub fn read_shard(dir: &Path, shard: u32) -> Result<Vec<Doc>> {
    let file = File::open(dir.join(shard_name(shard)))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        out.push(parse_record(&line?)?);
    }
    Ok(out)
}

pub fn read_doc(dir: &Path, shard: u32, id: DocId) -> Result<Doc> {
    let file = File::open(dir.join(shard_name(shard)))?;
    let prefix = format!("{id:08x}\t");
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.starts_with(&prefix) {
            return parse_record(&line);
        }
    }
    format_err(format!("doc {id:#x} not in shard {shard}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_finds_by_id() {
        let dir = std::env::temp_dir().join(format!("rill-knowledge-doc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut w = Writer::create(&dir).unwrap();
        let docs: Vec<Doc> = (0..3)
            .map(|i| Doc {
                id: i,
                page_id: 1000 + u64::from(i),
                qid: (i != 1).then(|| format!("Q{i}")),
                first_chunk: u64::from(i) * 10,
                chunk_count: 10,
                popularity: 1.5e-6 * f64::from(i + 1),
                title: format!("Title {i}\twith tab"),
            })
            .collect();
        for d in &docs {
            w.push(d).unwrap();
        }
        w.finish().unwrap();
        assert_eq!(read_shard(&dir, 0).unwrap(), docs);
        assert_eq!(read_doc(&dir, 0, 2).unwrap(), docs[2]);
        assert_eq!(read_doc(&dir, 0, 1).unwrap().qid, None);
        assert!(read_doc(&dir, 0, 9).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
