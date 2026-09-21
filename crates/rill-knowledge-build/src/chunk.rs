//! The chunker (`specs/knowledge.md` §4.3): section, then paragraph, then
//! sentence boundaries; paragraphs packed to a word budget; lists folded
//! into paragraphs of a few items; fragments merged; exact duplicates
//! dropped across the whole corpus.

use std::collections::HashSet;

use rill_knowledge::MAX_CHUNK_BYTES;
use rill_store::Hash;

use crate::source::{Block, Document, Section};

pub const TARGET_WORDS: usize = 300;
pub const MAX_WORDS: usize = 400;
pub const MIN_WORDS: usize = 8;
pub const LIST_ITEMS_PER_CHUNK: usize = 12;

/// A chunk before it has an id: where it came from and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Heading path, `Career > 1990s`; empty for the lead.
    pub section: String,
    pub text: String,
}

/// Corpus-wide exact-duplicate filter over normalised text.
#[derive(Default)]
pub struct Dedupe {
    seen: HashSet<[u8; 32]>,
    pub dropped: usize,
}

impl Dedupe {
    /// True if this text is new (and now remembered).
    pub fn admit(&mut self, text: &str) -> bool {
        let norm: String = text.split_whitespace().flat_map(|w| w.chars().flat_map(char::to_lowercase).chain(std::iter::once(' '))).collect();
        let key = Hash::of(norm.as_bytes()).0;
        if self.seen.insert(key) {
            true
        } else {
            self.dropped += 1;
            false
        }
    }
}

fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

/// Abbreviations whose period never ends a sentence.
const ABBREVIATIONS: &[&str] = &["dr", "mr", "mrs", "ms", "st", "jr", "sr", "vs", "etc", "e.g", "i.e", "no", "prof", "gen", "col", "lt", "sgt", "capt", "mt", "ft", "inc", "ltd", "co", "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec"];

/// Split at sentence ends: `.`, `!`, `?` (optionally followed by a closing
/// quote or bracket), then whitespace, then an upper-case letter, digit or
/// opening quote. A single capital or a known abbreviation before the
/// period does not end a sentence. Good enough for Simple English prose;
/// a run-on line just becomes one long sentence.
pub fn sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'.' | b'!' | b'?') {
            let mut end = i + 1;
            while end < bytes.len() && matches!(bytes[end], b'"' | b'\'' | b')') {
                end += 1;
            }
            // Closing curly quotes are multi-byte; take them too.
            for q in ["\u{201d}", "\u{2019}"] {
                if text[end..].starts_with(q) {
                    end += q.len();
                }
            }
            let mut j = end;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let abbreviation = bytes[i] == b'.' && {
                let word = text[start..i].rsplit(|c: char| c.is_whitespace()).next().unwrap_or("");
                let w = word.trim_start_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
                (w.len() == 1 && w.chars().all(|c| c.is_ascii_alphabetic())) || ABBREVIATIONS.contains(&w.as_str())
            };
            if j > end && j < bytes.len() && !abbreviation {
                let next = text[j..].chars().next().unwrap_or(' ');
                if next.is_uppercase() || next.is_ascii_digit() || matches!(next, '"' | '\'' | '(' | '\u{201c}' | '\u{2018}') {
                    out.push(text[start..end].trim());
                    start = j;
                    i = j;
                    continue;
                }
            }
            i = end.max(i + 1);
            continue;
        }
        i += 1;
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Split one over-long paragraph into pieces of at most `TARGET_WORDS`
/// words and `MAX_CHUNK_BYTES` bytes at sentence ends, then at word ends
/// if a sentence alone is too big.
fn split_long(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_words = 0;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.trim().is_empty() {
            out.push(std::mem::take(cur).trim().to_string());
        }
    };
    for s in sentences(text) {
        let w = words(s);
        if w > TARGET_WORDS || s.len() > MAX_CHUNK_BYTES {
            flush(&mut cur, &mut out);
            cur_words = 0;
            // A sentence too big on its own: cut at word ends.
            let mut piece = String::new();
            let mut pw = 0;
            for word in s.split_whitespace() {
                if pw >= TARGET_WORDS || piece.len() + word.len() + 1 > MAX_CHUNK_BYTES {
                    out.push(std::mem::take(&mut piece));
                    pw = 0;
                }
                if !piece.is_empty() {
                    piece.push(' ');
                }
                piece.push_str(word);
                pw += 1;
            }
            if !piece.is_empty() {
                out.push(piece);
            }
            continue;
        }
        if cur_words + w > TARGET_WORDS || cur.len() + s.len() + 1 > MAX_CHUNK_BYTES {
            flush(&mut cur, &mut out);
            cur_words = 0;
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(s);
        cur_words += w;
    }
    flush(&mut cur, &mut out);
    out
}

/// A section's blocks as paragraphs: lists become paragraphs of at most
/// `LIST_ITEMS_PER_CHUNK` items joined by `; `.
fn paragraphs(section: &Section) -> Vec<String> {
    let mut out = Vec::new();
    for block in &section.blocks {
        match block {
            Block::Paragraph(p) => {
                let p = p.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
            Block::List(items) => {
                for group in items.chunks(LIST_ITEMS_PER_CHUNK) {
                    let joined = group.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; ");
                    if !joined.is_empty() {
                        out.push(joined);
                    }
                }
            }
        }
    }
    out
}

/// Heading paths for every section: an `h3` under an `h2` reads
/// `H2 > H3`; the lead is empty.
fn section_paths(doc: &Document) -> Vec<String> {
    let mut stack: Vec<(u8, String)> = Vec::new();
    doc.sections
        .iter()
        .map(|s| match &s.heading {
            None => String::new(),
            Some(h) => {
                while stack.last().is_some_and(|(lvl, _)| *lvl >= s.level) {
                    stack.pop();
                }
                stack.push((s.level, h.trim().to_string()));
                stack.iter().map(|(_, h)| h.as_str()).collect::<Vec<_>>().join(" > ")
            }
        })
        .collect()
}

/// Chunk one document. Duplicates are dropped through `dedupe`.
pub fn chunk(doc: &Document, dedupe: &mut Dedupe) -> Vec<Piece> {
    let paths = section_paths(doc);
    let mut out: Vec<Piece> = Vec::new();
    for (section, path) in doc.sections.iter().zip(paths) {
        let mut cur = String::new();
        let mut cur_words = 0usize;
        let mut section_pieces: Vec<String> = Vec::new();
        for para in paragraphs(section) {
            let w = words(&para);
            if w > MAX_WORDS || para.len() > MAX_CHUNK_BYTES {
                if !cur.is_empty() {
                    section_pieces.push(std::mem::take(&mut cur));
                    cur_words = 0;
                }
                section_pieces.extend(split_long(&para));
                continue;
            }
            if cur_words + w > TARGET_WORDS || cur.len() + para.len() + 1 > MAX_CHUNK_BYTES {
                section_pieces.push(std::mem::take(&mut cur));
                cur_words = 0;
            }
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(&para);
            cur_words += w;
        }
        if !cur.is_empty() {
            section_pieces.push(cur);
        }
        // Fragments: merge a short piece into its predecessor within the
        // section when that stays under the cap; otherwise it stands.
        let mut merged: Vec<String> = Vec::new();
        for piece in section_pieces {
            if words(&piece) < MIN_WORDS
                && let Some(prev) = merged.last_mut()
                && words(prev) + words(&piece) <= MAX_WORDS
                && prev.len() + piece.len() < MAX_CHUNK_BYTES
            {
                prev.push('\n');
                prev.push_str(&piece);
                continue;
            }
            merged.push(piece);
        }
        for text in merged {
            if dedupe.admit(&text) {
                out.push(Piece { section: path.clone(), text });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(sections: Vec<(Option<&str>, u8, Vec<Block>)>) -> Document {
        Document {
            page_id: 1,
            title: "T".into(),
            wikibase_item: None,
            popularity: 0.0,
            sections: sections.into_iter().map(|(h, level, blocks)| Section { heading: h.map(String::from), level, blocks }).collect(),
        }
    }

    #[test]
    fn sentences_split_on_terminators_followed_by_capitals() {
        let s = sentences("Dr. Smith went home. He slept! Did he? Yes, at 3.5 pm. \"Quoted.\" End of J. R. Tolkien. Done");
        assert_eq!(s, vec!["Dr. Smith went home.", "He slept!", "Did he?", "Yes, at 3.5 pm.", "\"Quoted.\"", "End of J. R. Tolkien.", "Done"]);
    }

    #[test]
    fn packs_paragraphs_to_the_budget_and_splits_long_ones() {
        let para = |n: usize, w: &str| Block::Paragraph((0..n).map(|i| format!("{w}{i}.")).collect::<Vec<_>>().join(" "));
        let d = doc(vec![
            (None, 1, vec![para(100, "lead"), para(100, "more"), para(150, "third")]),
            (Some("History"), 2, vec![para(500, "hist")]),
            (Some("Early"), 3, vec![Block::List((0..30).map(|i| format!("item {i}")).collect())]),
            (Some("Stub"), 2, vec![Block::Paragraph("Tiny.".into())]),
        ]);
        let pieces = chunk(&d, &mut Dedupe::default());
        let lead: Vec<&Piece> = pieces.iter().filter(|p| p.section.is_empty()).collect();
        assert_eq!(lead.len(), 2, "200 + 150 words pack into two chunks");
        assert_eq!(words(&lead[0].text), 200);
        let hist: Vec<&Piece> = pieces.iter().filter(|p| p.section == "History").collect();
        assert_eq!(hist.len(), 2, "500 words split at sentence ends into <=300-word pieces");
        assert!(hist.iter().all(|p| words(&p.text) <= TARGET_WORDS));
        let early: Vec<&Piece> = pieces.iter().filter(|p| p.section == "History > Early").collect();
        assert_eq!(early.len(), 1, "30 list items -> 3 paragraphs of 12, packed into one chunk");
        assert!(early[0].text.contains("item 0; item 1"));
        let stub: Vec<&Piece> = pieces.iter().filter(|p| p.section == "Stub").collect();
        assert_eq!(stub.len(), 1, "a lone fragment stands");
    }

    #[test]
    fn dedupe_drops_repeats_across_documents() {
        let mut dd = Dedupe::default();
        let d = doc(vec![(None, 1, vec![Block::Paragraph("The same text appears in two articles here.".into())])]);
        assert_eq!(chunk(&d, &mut dd).len(), 1);
        assert_eq!(chunk(&d, &mut dd).len(), 0);
        assert_eq!(dd.dropped, 1);
    }

    #[test]
    fn respects_the_byte_cap() {
        let long_word = "x".repeat(500);
        let d = doc(vec![(None, 1, vec![Block::Paragraph((0..10).map(|_| long_word.clone()).collect::<Vec<_>>().join(" "))])]);
        for p in chunk(&d, &mut Dedupe::default()) {
            assert!(p.text.len() <= MAX_CHUNK_BYTES);
        }
    }
}
