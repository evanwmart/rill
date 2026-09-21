//! Semantic tree nodes (§7): every directory has an explicit `node`
//! resource; leaves have `members`. Nothing needs `read_dir`.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::base64;
use crate::{COARSE_DIM, ChunkId, Result, format_err};

/// Bytes per `members` row: 16 hex digits and a newline.
pub const MEMBER_ROW: u64 = 17;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Children `0..count`, each with its coarse centroid.
    Inner { children: Vec<(String, Vec<i8>)> },
    /// A leaf with `count` members in the sibling `members` file.
    Leaf { count: u64 },
}

pub fn format_node(node: &Node) -> String {
    let mut out = String::from("v=1\n");
    match node {
        Node::Inner { children } => {
            out.push_str(&format!("kind=inner\ncount={}\n\n", children.len()));
            for (name, centroid) in children {
                out.push_str(name);
                out.push(' ');
                let bytes: Vec<u8> = centroid.iter().map(|&x| x as u8).collect();
                base64::encode_into(&bytes, &mut out);
                out.push('\n');
            }
        }
        Node::Leaf { count } => out.push_str(&format!("kind=leaf\ncount={count}\n")),
    }
    out
}

pub fn parse_node(text: &str) -> Result<Node> {
    let mut lines = text.lines();
    if lines.next() != Some("v=1") {
        return format_err("node: missing v=1");
    }
    let kind = lines.next().and_then(|l| l.strip_prefix("kind=")).ok_or_else(|| crate::Error::Format("node: missing kind".into()))?;
    let count: u64 = lines
        .next()
        .and_then(|l| l.strip_prefix("count="))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| crate::Error::Format("node: missing count".into()))?;
    match kind {
        "leaf" => Ok(Node::Leaf { count }),
        "inner" => {
            if lines.next() != Some("") {
                return format_err("node: inner needs a blank line before children");
            }
            let mut children = Vec::with_capacity(count as usize);
            for line in lines {
                let Some((name, b64)) = line.split_once(' ') else {
                    return format_err(format!("node child line {line:?}"));
                };
                if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                    return format_err(format!("node child name {name:?}"));
                }
                match base64::decode(b64.as_bytes()) {
                    Some(bytes) if bytes.len() == COARSE_DIM => children.push((name.to_string(), bytes.iter().map(|&b| b as i8).collect())),
                    _ => return format_err(format!("node child {name}: bad centroid")),
                }
            }
            if children.len() as u64 != count {
                return format_err(format!("node: count={count} but {} children", children.len()));
            }
            Ok(Node::Inner { children })
        }
        other => format_err(format!("node kind {other:?}")),
    }
}

pub fn read_node(dir: &Path) -> Result<Node> {
    parse_node(&fs::read_to_string(dir.join("node"))?)
}

pub fn write_node(dir: &Path, node: &Node) -> Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("node"), format_node(node))?;
    Ok(())
}

pub fn write_members(dir: &Path, sorted: &[ChunkId]) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut out = String::with_capacity(sorted.len() * MEMBER_ROW as usize);
    for w in sorted.windows(2) {
        if w[0] >= w[1] {
            return format_err("members must be strictly ascending");
        }
    }
    for id in sorted {
        out.push_str(&format!("{id:016x}\n"));
    }
    fs::write(dir.join("members"), out)?;
    Ok(())
}

pub fn read_members(dir: &Path) -> Result<Vec<ChunkId>> {
    let mut out = Vec::new();
    for line in BufReader::new(File::open(dir.join("members"))?).lines() {
        let line = line?;
        out.push(u64::from_str_radix(&line, 16).map_err(|_| crate::Error::Format(format!("members row {line:?}")))?);
    }
    Ok(out)
}

/// One member by row, by seek.
pub fn read_member(dir: &Path, row: u64) -> Result<ChunkId> {
    let mut f = File::open(dir.join("members"))?;
    f.seek(SeekFrom::Start(row * MEMBER_ROW))?;
    let mut hex = [0u8; 16];
    f.read_exact(&mut hex)?;
    let s = std::str::from_utf8(&hex).map_err(|_| crate::Error::Format("members row not ascii".into()))?;
    u64::from_str_radix(s, 16).map_err(|_| crate::Error::Format(format!("members row {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_round_trip() {
        let inner = Node::Inner {
            children: (0..3).map(|i| (i.to_string(), (0..COARSE_DIM).map(|k| ((k as i32 * (i + 1)) % 200 - 100) as i8).collect())).collect(),
        };
        assert_eq!(parse_node(&format_node(&inner)).unwrap(), inner);
        let leaf = Node::Leaf { count: 42 };
        assert_eq!(parse_node(&format_node(&leaf)).unwrap(), leaf);
        assert!(parse_node("v=2\nkind=leaf\ncount=1\n").is_err());
        assert!(parse_node("v=1\nkind=inner\ncount=1\n\n../ AAAA\n").is_err());
        assert!(parse_node("v=1\nkind=inner\ncount=2\n\n0 AAAA\n").is_err());
    }

    #[test]
    fn members_are_seekable() {
        let dir = std::env::temp_dir().join(format!("rill-knowledge-node-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_members(&dir, &[3, 7, 1 << 33]).unwrap();
        assert_eq!(read_members(&dir).unwrap(), vec![3, 7, 1 << 33]);
        assert_eq!(read_member(&dir, 2).unwrap(), 1 << 33);
        assert!(write_members(&dir, &[7, 3]).is_err());
        write_node(&dir, &Node::Leaf { count: 3 }).unwrap();
        assert_eq!(read_node(&dir).unwrap(), Node::Leaf { count: 3 });
        fs::remove_dir_all(&dir).unwrap();
    }
}
