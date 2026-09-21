//! `knowledge/manifest` (§9): `key=value` lines, `#` comments, unknown
//! keys ignored. Written last by a build; its absence means an incomplete
//! tree.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::{Result, format_err};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifest {
    entries: BTreeMap<String, String>,
}

impl Manifest {
    pub fn new() -> Manifest {
        Manifest::default()
    }

    pub fn parse(text: &str) -> Result<Manifest> {
        let mut entries = BTreeMap::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                return format_err(format!("manifest line {}: no `=`", n + 1));
            };
            let k = k.trim();
            if k.is_empty() || k.contains(char::is_whitespace) {
                return format_err(format!("manifest line {}: bad key", n + 1));
            }
            entries.insert(k.to_string(), v.trim().to_string());
        }
        let m = Manifest { entries };
        match m.get("format") {
            Some("rill-knowledge") => {}
            other => return format_err(format!("manifest format={other:?}, want rill-knowledge")),
        }
        match m.get("version") {
            Some("1") => {}
            other => return format_err(format!("manifest version={other:?}, want 1")),
        }
        for key in ["shard", "chunks", "documents"] {
            m.u64(key)?;
        }
        Ok(m)
    }

    pub fn read(path: &Path) -> Result<Manifest> {
        let text = fs::read_to_string(path)?;
        Manifest::parse(&text)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let (key, value) = (key.into(), value.into());
        debug_assert!(!key.contains(char::is_whitespace) && !key.contains('='));
        debug_assert!(!value.contains('\n'));
        self.entries.insert(key, value);
        self
    }

    pub fn u64(&self, key: &str) -> Result<u64> {
        match self.get(key) {
            Some(v) => v.parse().map_err(|_| crate::Error::Format(format!("manifest {key}={v}: not an integer"))),
            None => format_err(format!("manifest: missing {key}")),
        }
    }

    pub fn shard(&self) -> u64 {
        self.u64("shard").unwrap_or(0)
    }

    pub fn chunks(&self) -> u64 {
        self.u64("chunks").unwrap_or(0)
    }

    pub fn documents(&self) -> u64 {
        self.u64("documents").unwrap_or(0)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Serialised in key order, one blank line between key prefixes so it
    /// reads as sections. Deterministic.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let mut last_prefix: Option<&str> = None;
        for (k, v) in &self.entries {
            let prefix = k.split('.').next().unwrap_or(k);
            if let Some(p) = last_prefix
                && p != prefix
            {
                out.push('\n');
            }
            last_prefix = Some(prefix);
            out.push_str(k);
            out.push('=');
            out.push_str(v);
            out.push('\n');
        }
        out
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        fs::write(path, self.to_text())?;
        Ok(())
    }
}

/// The keys a version-1 build always writes, with the fixed values.
pub fn base(chunks: u64, documents: u64) -> Manifest {
    let mut m = Manifest::new();
    m.set("format", "rill-knowledge")
        .set("version", "1")
        .set("shard", crate::SHARD.to_string())
        .set("chunks", chunks.to_string())
        .set("documents", documents.to_string())
        .set("embedding.dim", crate::DIM.to_string())
        .set("embedding.quant", "i8")
        .set("embedding.normalized", "true")
        .set("projection", "countsketch-v1")
        .set("projection.dim", crate::COARSE_DIM.to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ignores_unknown_and_round_trips() {
        let mut m = base(10, 2);
        m.set("projection.seed", "0123456789abcdef").set("embedding.model", "bge-small-en-v1.5");
        let text = format!("# built\n{}\nfuture.key=whatever\n", m.to_text());
        let back = Manifest::parse(&text).unwrap();
        assert_eq!(back.chunks(), 10);
        assert_eq!(back.documents(), 2);
        assert_eq!(back.shard(), crate::SHARD);
        assert_eq!(back.get("future.key"), Some("whatever"));
        assert_eq!(back.get("embedding.model"), Some("bge-small-en-v1.5"));
        assert_eq!(Manifest::parse(&back.to_text()).unwrap(), back);
    }

    #[test]
    fn refuses_wrong_format_or_missing_counts() {
        assert!(Manifest::parse("format=other\nversion=1\n").is_err());
        assert!(Manifest::parse("format=rill-knowledge\nversion=2\n").is_err());
        assert!(Manifest::parse("format=rill-knowledge\nversion=1\nshard=8192\n").is_err());
        assert!(Manifest::parse("format=rill-knowledge\nversion=1\nshard=x\nchunks=1\ndocuments=1\n").is_err());
    }
}
