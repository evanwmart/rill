//! Chunk text shards (§4.2): `text/NNNN` holds one escaped record per
//! line, `text/NNNN.off` a fixed 17-byte offset per row. Lookup of a chunk
//! is a seek into the offset table, a seek into the shard, one line.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{ChunkId, DocId, MAX_CHUNK_BYTES, Result, SHARD, format_err, shard_name};

/// Bytes per offset-table row: 16 hex digits and a newline.
pub const OFF_ROW: u64 = 17;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: ChunkId,
    pub doc: DocId,
    /// Heading path, empty for the lead.
    pub section: String,
    pub text: String,
}

/// `\` → `\\`, TAB → `\t`, LF → `\n`, CR → `\r`. Nothing else.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_into(s, &mut out);
    out
}

pub fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

pub fn unescape(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            other => return format_err(format!("bad escape \\{}", other.map_or("<end>".to_string(), |c| c.to_string()))),
        }
    }
    Ok(out)
}

/// One record line, without the trailing newline.
pub fn format_record(c: &Chunk) -> String {
    let mut line = format!("{:016x}\t{:08x}\t", c.id, c.doc);
    escape_into(&c.section, &mut line);
    line.push('\t');
    escape_into(&c.text, &mut line);
    line
}

pub fn parse_record(line: &str) -> Result<Chunk> {
    let mut f = line.splitn(4, '\t');
    let (Some(id), Some(doc), Some(section), Some(text)) = (f.next(), f.next(), f.next(), f.next()) else {
        return format_err("chunk record: fewer than 4 fields");
    };
    let id = u64::from_str_radix(id, 16).map_err(|_| crate::Error::Format(format!("chunk id {id:?}")))?;
    let doc = u32::from_str_radix(doc, 16).map_err(|_| crate::Error::Format(format!("doc id {doc:?}")))?;
    Ok(Chunk { id, doc, section: unescape(section)?, text: unescape(text)? })
}

/// Writes `text/NNNN` and `text/NNNN.off` for consecutive chunk ids,
/// rolling to the next shard every `SHARD` records. Ids must arrive dense
/// and ascending from `first_id`; the writer checks.
pub struct Writer {
    dir: PathBuf,
    next_id: ChunkId,
    shard: Option<(u32, BufWriter<File>, BufWriter<File>, u64)>,
}

impl Writer {
    pub fn create(dir: impl AsRef<Path>, first_id: ChunkId) -> Result<Writer> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Writer { dir, next_id: first_id, shard: None })
    }

    pub fn next_id(&self) -> ChunkId {
        self.next_id
    }

    pub fn push(&mut self, c: &Chunk) -> Result<()> {
        if c.id != self.next_id {
            return format_err(format!("chunk {:#x} written out of order (expected {:#x})", c.id, self.next_id));
        }
        if c.text.len() > MAX_CHUNK_BYTES {
            return format_err(format!("chunk {:#x}: {} bytes over MAX_CHUNK_BYTES", c.id, c.text.len()));
        }
        let (shard, row) = crate::shard_of(c.id);
        if row == 0 {
            self.roll(shard)?;
        }
        let Some((_, text, off, written)) = self.shard.as_mut() else {
            return format_err("text writer: first id not on a shard boundary");
        };
        writeln!(off, "{written:016x}")?;
        let line = format_record(c);
        text.write_all(line.as_bytes())?;
        text.write_all(b"\n")?;
        *written += line.len() as u64 + 1;
        self.next_id += 1;
        Ok(())
    }

    fn roll(&mut self, shard: u32) -> Result<()> {
        self.finish_shard()?;
        let name = shard_name(shard);
        let text = BufWriter::new(File::create(self.dir.join(&name))?);
        let off = BufWriter::new(File::create(self.dir.join(format!("{name}.off")))?);
        self.shard = Some((shard, text, off, 0));
        Ok(())
    }

    fn finish_shard(&mut self) -> Result<()> {
        if let Some((_, mut text, mut off, _)) = self.shard.take() {
            text.flush()?;
            off.flush()?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<ChunkId> {
        self.finish_shard()?;
        Ok(self.next_id)
    }
}

/// Read row `row` of shard `shard` under `dir` (§4.2 lookup).
pub fn read_chunk(dir: &Path, shard: u32, row: u64) -> Result<Chunk> {
    let name = shard_name(shard);
    let mut off = File::open(dir.join(format!("{name}.off")))?;
    off.seek(SeekFrom::Start(row * OFF_ROW))?;
    let mut hex = [0u8; 16];
    off.read_exact(&mut hex)?;
    let hex = std::str::from_utf8(&hex).map_err(|_| crate::Error::Format("offset row not ascii".into()))?;
    let start = u64::from_str_radix(hex, 16).map_err(|_| crate::Error::Format(format!("offset row {hex:?}")))?;
    let mut text = File::open(dir.join(&name))?;
    text.seek(SeekFrom::Start(start))?;
    let mut line = String::new();
    BufReader::new(text).read_line(&mut line)?;
    let line = line.strip_suffix('\n').unwrap_or(&line);
    let chunk = parse_record(line)?;
    let want = u64::from(shard) * SHARD + row;
    if chunk.id != want {
        return format_err(format!("text/{name} row {row}: record id {:#x}, expected {want:#x}", chunk.id));
    }
    Ok(chunk)
}

/// Every record of a shard in order, for scans and verification.
pub fn read_shard(dir: &Path, shard: u32) -> Result<Vec<Chunk>> {
    let file = File::open(dir.join(shard_name(shard)))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        out.push(parse_record(&line?)?);
    }
    Ok(out)
}

/// Rows in a shard, from the offset table's size.
pub fn shard_rows(dir: &Path, shard: u32) -> io::Result<u64> {
    Ok(fs::metadata(dir.join(format!("{}.off", shard_name(shard))))?.len() / OFF_ROW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_is_exactly_four_sequences() {
        let s = "a\\b\tc\nd\re";
        assert_eq!(escape(s), "a\\\\b\\tc\\nd\\re");
        assert_eq!(unescape(&escape(s)).unwrap(), s);
        assert!(unescape("bad\\x").is_err());
        assert!(unescape("trailing\\").is_err());
    }

    #[test]
    fn writes_shards_and_reads_rows_back() {
        let dir = std::env::temp_dir().join(format!("rill-knowledge-text-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut w = Writer::create(&dir, 0).unwrap();
        let n = SHARD + 5;
        for id in 0..n {
            w.push(&Chunk {
                id,
                doc: (id / 3) as u32,
                section: if id % 2 == 0 { String::new() } else { format!("Sec {id}\twith tab") },
                text: format!("chunk {id}\nline two"),
            })
            .unwrap();
        }
        assert_eq!(w.finish().unwrap(), n);
        assert_eq!(shard_rows(&dir, 0).unwrap(), SHARD);
        assert_eq!(shard_rows(&dir, 1).unwrap(), 5);
        for id in [0, 1, 7, SHARD - 1, SHARD, SHARD + 4] {
            let (shard, row) = crate::shard_of(id);
            let c = read_chunk(&dir, shard, row).unwrap();
            assert_eq!(c.id, id);
            assert_eq!(c.doc, (id / 3) as u32);
            assert_eq!(c.text, format!("chunk {id}\nline two"));
        }
        assert_eq!(read_shard(&dir, 1).unwrap().len(), 5);
        // grep-ability: the raw shard is one physical line per chunk.
        let raw = fs::read_to_string(dir.join("0001")).unwrap();
        assert_eq!(raw.lines().count(), 5);
        assert!(raw.contains("chunk 8192\\nline two"));
        // Out of order is refused.
        let mut w2 = Writer::create(dir.join("x"), 0).unwrap();
        assert!(w2.push(&Chunk { id: 1, doc: 0, section: String::new(), text: "t".into() }).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
