//! Postings (§8): sorted ids → deltas → unsigned LEB128 → Base64URL, and
//! the sorted `<term>\t<df hex>\t<postings>` files looked up by binary
//! search over the raw file.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::base64;
use crate::{Result, format_err};

pub fn encode(sorted_ids: &[u64]) -> String {
    let mut bytes = Vec::with_capacity(sorted_ids.len() * 2);
    let mut prev = 0u64;
    for (i, &id) in sorted_ids.iter().enumerate() {
        debug_assert!(i == 0 || id > prev, "postings must be strictly ascending");
        let mut d = if i == 0 { id } else { id - prev };
        prev = id;
        loop {
            let byte = (d & 0x7f) as u8;
            d >>= 7;
            if d == 0 {
                bytes.push(byte);
                break;
            }
            bytes.push(byte | 0x80);
        }
    }
    base64::encode(&bytes)
}

pub fn decode(text: &str) -> Result<Vec<u64>> {
    let Some(bytes) = base64::decode(text.as_bytes()) else {
        return format_err("postings: not base64url");
    };
    let mut out = Vec::new();
    let (mut acc, mut shift, mut prev) = (0u64, 0u32, 0u64);
    for &b in &bytes {
        if shift > 63 {
            return format_err("postings: varint too long");
        }
        acc |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            let id = if out.is_empty() { acc } else { prev + acc };
            out.push(id);
            prev = id;
            acc = 0;
            shift = 0;
        } else {
            shift += 7;
        }
    }
    if shift != 0 {
        return format_err("postings: truncated varint");
    }
    Ok(out)
}

/// One line of a postings file, without the newline.
pub fn format_line(term: &str, ids: &[u64]) -> String {
    format!("{term}\t{:x}\t{}", ids.len(), encode(ids))
}

/// Which file a term lives in: its first two chars, `_` for shorter terms,
/// and a two-hex-digit byte name for non-ASCII first characters.
pub fn prefix_of(term: &str) -> String {
    let mut chars = term.chars();
    match (chars.next(), chars.next()) {
        (Some(a), Some(b)) if a.is_ascii() && b.is_ascii() => format!("{a}{b}"),
        (Some(a), _) if !a.is_ascii() => format!("{:02x}", term.as_bytes()[0]),
        _ => "_".to_string(),
    }
}

/// Binary search a sorted postings file for `term`; the ids if present.
pub fn lookup(path: &Path, term: &str) -> Result<Option<Vec<u64>>> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let len = f.metadata()?.len();
    let (mut lo, mut hi) = (0u64, len);
    let mut line = String::new();
    // Invariant: the line containing `term`, if any, starts at or after
    // `lo` and before `hi`. Each probe lands mid-file and advances to a
    // line start, then compares that line's term.
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (start, found) = line_at(&mut f, mid, lo, &mut line)?;
        if !found || start >= hi {
            // No line starts in [mid, hi) — landing mid-line skipped past
            // `hi`, or hit EOF — so the answer is in [lo, mid).
            hi = mid;
            continue;
        }
        let key = line.split('\t').next().unwrap_or("");
        match key.cmp(term) {
            std::cmp::Ordering::Equal => return parse_ids(&line).map(Some),
            std::cmp::Ordering::Less => lo = start + line.len() as u64 + 1,
            std::cmp::Ordering::Greater => hi = start,
        }
    }
    Ok(None)
}

/// The first full line starting at or after `pos` (or at `floor` when
/// `pos == floor`). Returns its start and whether one exists before EOF.
fn line_at(f: &mut File, pos: u64, floor: u64, line: &mut String) -> Result<(u64, bool)> {
    let mut start = pos;
    if pos > floor {
        // Skip the partial line we landed in.
        f.seek(SeekFrom::Start(pos - 1))?;
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte)?;
        if byte[0] != b'\n' {
            let mut r = BufReader::new(&mut *f);
            let mut skip = Vec::new();
            let n = r.read_until(b'\n', &mut skip)?;
            start = pos + n as u64;
        }
    }
    f.seek(SeekFrom::Start(start))?;
    line.clear();
    let n = BufReader::new(f).read_line(line)?;
    if n == 0 {
        return Ok((start, false));
    }
    if line.ends_with('\n') {
        line.pop();
    }
    Ok((start, true))
}

fn parse_ids(line: &str) -> Result<Vec<u64>> {
    let mut f = line.split('\t');
    let (Some(_), Some(df), Some(data)) = (f.next(), f.next(), f.next()) else {
        return format_err("postings line: not 3 fields");
    };
    let ids = decode(data)?;
    let want = usize::from_str_radix(df, 16).map_err(|_| crate::Error::Format(format!("postings df {df:?}")))?;
    if ids.len() != want {
        return format_err(format!("postings line: df {want} but {} ids", ids.len()));
    }
    Ok(ids)
}

/// Every (term, ids) of a file, for verification and merging.
pub fn read_all(path: &Path) -> Result<Vec<(String, Vec<u64>)>> {
    let mut out = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let term = line.split('\t').next().unwrap_or("").to_string();
        out.push((term, parse_ids(&line)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn ids_round_trip_including_large_gaps() {
        for ids in [vec![], vec![0], vec![5], vec![0, 1, 2, 3], vec![3, 130, 16_384, 1 << 40, (1 << 40) + 1]] {
            assert_eq!(decode(&encode(&ids)).unwrap(), ids, "{ids:?}");
        }
        assert!(decode("!").is_err());
        assert!(decode(&base64::encode(&[0x80])).is_err(), "truncated varint");
    }

    #[test]
    fn prefixes() {
        assert_eq!(prefix_of("quantum"), "qu");
        assert_eq!(prefix_of("q"), "_");
        assert_eq!(prefix_of("émile"), "c3");
        assert_eq!(prefix_of("42nd"), "42");
    }

    #[test]
    fn binary_search_finds_every_term_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("rill-knowledge-post-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut terms: Vec<String> = (0..500).map(|i| format!("t{i:04}")).collect();
        terms.push("t".repeat(300)); // a long line
        terms.sort();
        let mut text = String::new();
        for (i, t) in terms.iter().enumerate() {
            let ids: Vec<u64> = (0..(i % 7 + 1) as u64).map(|k| k * 1000 + i as u64).collect();
            text.push_str(&format_line(t, &ids));
            text.push('\n');
        }
        let path = dir.join("tt");
        fs::write(&path, &text).unwrap();
        for (i, t) in terms.iter().enumerate() {
            let ids = lookup(&path, t).unwrap().unwrap_or_else(|| panic!("missing {t}"));
            assert_eq!(ids.len(), i % 7 + 1, "{t}");
        }
        assert_eq!(lookup(&path, "t0000a").unwrap(), None);
        assert_eq!(lookup(&path, "a").unwrap(), None);
        assert_eq!(lookup(&path, "zzz").unwrap(), None);
        assert_eq!(lookup(&dir.join("absent"), "x").unwrap(), None);
        assert_eq!(read_all(&path).unwrap().len(), terms.len());
        fs::remove_dir_all(&dir).unwrap();
    }
}
