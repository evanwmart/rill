//! Terms and the postings directories (§8): the one tokenizer both the
//! build and the query use, and readers over `lexical/` and `entity/`.

use std::path::{Path, PathBuf};

use crate::{ChunkId, Result, postings};

/// Longest term kept; anything longer is noise (URLs, hashes).
pub const MAX_TERM_BYTES: usize = 64;

/// Unicode-lowercase, split on non-alphanumerics, no stemming. Order of
/// first appearance, duplicates removed.
pub fn terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            if cur.len() <= MAX_TERM_BYTES && !out.contains(cur) {
                out.push(cur.clone());
            }
            cur.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// An entity key: the whole string lowercased with whitespace collapsed to
/// single spaces — what a title or an alias is indexed under.
pub fn entity_key(s: &str) -> String {
    s.split_whitespace().flat_map(|w| w.chars().flat_map(char::to_lowercase).chain(std::iter::once(' '))).collect::<String>().trim_end().to_string()
}

/// A postings directory (`lexical/` or `entity/`).
pub struct Index {
    dir: PathBuf,
}

impl Index {
    pub fn open(dir: impl AsRef<Path>) -> Index {
        Index { dir: dir.as_ref().to_path_buf() }
    }

    pub fn exists(&self) -> bool {
        self.dir.is_dir()
    }

    /// The ids for a term, or none.
    pub fn lookup(&self, term: &str) -> Result<Option<Vec<ChunkId>>> {
        if term.is_empty() {
            return Ok(None);
        }
        postings::lookup(&self.dir.join(postings::prefix_of(term)), term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_lowercase_alphanumeric_runs() {
        assert_eq!(terms("Einstein's 1905 papers — Quantum, quantum!"), vec!["einstein", "s", "1905", "papers", "quantum"]);
        assert_eq!(terms("Ærø ÉMILE"), vec!["ærø", "émile"]);
        assert!(terms(&"x".repeat(65)).is_empty());
        assert_eq!(entity_key("  Albert   EINSTEIN "), "albert einstein");
    }
}
