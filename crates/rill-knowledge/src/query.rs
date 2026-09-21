//! The query engine (§10), as far as the pack's stage allows: lexical and
//! entity runs fused by reciprocal rank, a title boost, one hit per
//! document. Semantic runs join when the pack carries vectors.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::doc::{self, Doc};
use crate::lexical::{Index, entity_key, terms};
use crate::{ChunkId, DIM, DocId, Pack, Result, SHARD, text, vector};

/// Reciprocal rank fusion constant.
pub const RRF_K: f32 = 60.0;
/// A term present in more than this share of chunks carries no signal and
/// is skipped unless every term is such.
pub const STOP_SHARE: f32 = 0.2;
/// Candidates kept from the lexical run before fusion.
pub const LEXICAL_RUN: usize = 200;
/// Multiplier when the hit's document title appears in the query.
pub const TITLE_BOOST: f32 = 1.25;
/// Candidates kept from the semantic run before fusion.
pub const SEMANTIC_RUN: usize = 200;
/// Weight of the lexical run's reciprocal ranks when every term matched.
/// Without term frequencies the run is ranked by article popularity among
/// the matches, which is weak next to a cosine; a gold-set sweep on
/// 2026-09-21 (specs/knowledge.md §12) chose 0.5 here and 0.25 below as
/// the only pair that kept every answerable hand question in the top ten.
pub const LEXICAL_WEIGHT: f32 = 0.5;
/// Weight when no chunk matched every term (the OR fallback): weaker
/// still, and at full weight it crowds a strong semantic hit out.
pub const LEXICAL_OR_WEIGHT: f32 = 0.25;
/// Question and function words dropped from the lexical run: they are
/// under the stop share yet say nothing about the answer, and with them in
/// the AND set the run is ranked by which popular article happens to say
/// "what". Kept if they are all the query has.
const STOP_TERMS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "did", "do", "does", "for", "from", "how", "in", "into", "is", "it", "its", "of", "on", "or",
    "that", "the", "their", "there", "these", "this", "those", "to", "was", "were", "what", "when", "where", "which", "who", "whom", "why", "will", "with", "you", "your",
];

/// Which runs a search performs. `Fused` is the product; the others exist
/// so the gold set can measure each run alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lexical,
    Semantic,
    Fused,
}

/// Every full vector, resident: `chunks × DIM` int8 plus a norm per row.
/// ~100 MB for this corpus, read once; the brute-force scan over it is
/// the recall ceiling every later index is measured against (§2).
struct Vectors {
    data: Vec<i8>,
    norms: Vec<f32>,
}

pub struct Engine {
    pack: Pack,
    docs: Vec<Doc>,
    lexical: Index,
    entity: Index,
    vectors: Option<Vectors>,
    lexical_weight: f32,
    lexical_or_weight: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub chunk: ChunkId,
    pub doc: DocId,
    pub score: f32,
    pub lexical_rank: Option<usize>,
    pub entity_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
    /// Cosine to the query vector, when the semantic run saw this chunk.
    pub cosine: Option<f32>,
    /// Query terms this chunk matched, of `Results::terms.len()`.
    pub matched: usize,
    pub title_match: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub terms: Vec<String>,
    pub skipped_terms: Vec<String>,
    pub lexical_candidates: usize,
    pub entity_candidates: usize,
    pub semantic_candidates: usize,
    /// Whether the lexical run needed the OR fallback (no chunk had every term).
    pub any_term: bool,
    pub semantic_available: bool,
    pub elapsed_ms: f32,
}

impl Engine {
    /// Open the pack at `root` (the directory holding `manifest`) and load
    /// the document table: ~100 bytes a document, read once.
    pub fn open(root: impl AsRef<Path>) -> Result<Engine> {
        let pack = Pack::open(&root)?;
        let shards = pack.doc_count().div_ceil(SHARD);
        let mut docs = Vec::with_capacity(pack.doc_count() as usize);
        for s in 0..shards {
            docs.extend(doc::read_shard(&pack.root().join("doc"), s as u32)?);
        }
        for (i, d) in docs.iter().enumerate() {
            if d.id as usize != i {
                return crate::format_err(format!("doc table: row {i} has id {:#x}", d.id));
            }
        }
        let lexical = Index::open(pack.root().join("lexical"));
        let entity = Index::open(pack.root().join("entity"));
        let vectors = Self::load_vectors(&pack)?;
        Ok(Engine { pack, docs, lexical, entity, vectors, lexical_weight: LEXICAL_WEIGHT, lexical_or_weight: LEXICAL_OR_WEIGHT })
    }

    fn load_vectors(pack: &Pack) -> Result<Option<Vectors>> {
        let dir = pack.root().join("vector/full");
        if !dir.is_dir() || pack.manifest().get("stage").is_none_or(|s| s == "text" || s == "lexical") {
            return Ok(None);
        }
        let n = pack.chunk_count() as usize;
        let mut data = Vec::with_capacity(n * DIM);
        let mut norms = Vec::with_capacity(n);
        for s in 0..pack.chunk_count().div_ceil(SHARD) as u32 {
            let shard = pack.full_shard(s)?;
            for (_, row) in shard.iter() {
                norms.push((vector::norm_sq(row) as f32).sqrt());
                data.extend_from_slice(row);
            }
        }
        if norms.len() != n {
            return crate::format_err(format!("vector shards hold {} rows, manifest says {n} chunks", norms.len()));
        }
        Ok(Some(Vectors { data, norms }))
    }

    /// One chunk's full vector from the resident set.
    pub fn vector_of(&self, chunk: ChunkId) -> Option<&[i8]> {
        let v = self.vectors.as_ref()?;
        let i = chunk as usize;
        (i < v.norms.len()).then(|| &v.data[i * DIM..(i + 1) * DIM])
    }

    /// The semantic run: brute-force cosine over every chunk, top `k`.
    fn semantic_run(&self, query: &[f32], k: usize) -> Vec<(ChunkId, f32)> {
        let Some(v) = &self.vectors else { return Vec::new() };
        let q = vector::quantize(query);
        let qn = (vector::norm_sq(&q) as f32).sqrt();
        if qn == 0.0 {
            return Vec::new();
        }
        // A bounded min-heap keyed on cosine would be tidier; a full sort
        // of 258k floats is a few milliseconds and simpler to read.
        let mut scored: Vec<(ChunkId, f32)> = v
            .data
            .as_chunks::<DIM>()
            .0
            .iter()
            .zip(&v.norms)
            .enumerate()
            .filter(|(_, (_, n))| **n > 0.0)
            .map(|(i, (row, n))| (i as ChunkId, vector::dot(&q, row) as f32 / (qn * n)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }

    pub fn pack(&self) -> &Pack {
        &self.pack
    }

    /// Override [`LEXICAL_WEIGHT`] and [`LEXICAL_OR_WEIGHT`] (the gold-set
    /// eval sweeps them).
    pub fn set_lexical_weights(&mut self, all_terms: f32, or_fallback: f32) {
        self.lexical_weight = all_terms;
        self.lexical_or_weight = or_fallback;
    }

    pub fn doc(&self, id: DocId) -> Option<&Doc> {
        self.docs.get(id as usize)
    }

    pub fn chunk(&self, id: ChunkId) -> Result<text::Chunk> {
        self.pack.chunk(id)
    }

    pub fn lexical_available(&self) -> bool {
        self.lexical.exists()
    }

    /// Whether the pack carries vectors the engine could search. A query
    /// vector still has to come from an embedder that matches the manifest.
    pub fn semantic_available(&self) -> bool {
        self.vectors.is_some()
    }

    /// Lexical and entity runs only.
    pub fn search(&self, query: &str, limit: usize) -> Result<Results> {
        self.search_with(query, limit, None, Mode::Lexical)
    }

    /// The full query (§10): the runs `mode` asks for, fused by reciprocal
    /// rank. `query_vec` is the embedded query (with the model's query
    /// prefix applied by the caller); without one the semantic run is
    /// skipped whatever the mode says.
    pub fn search_with(&self, query: &str, limit: usize, query_vec: Option<&[f32]>, mode: Mode) -> Result<Results> {
        let t0 = Instant::now();
        let semantic = matches!(mode, Mode::Semantic | Mode::Fused) && query_vec.is_some() && self.vectors.is_some();
        let mut res = Results { semantic_available: semantic, ..Default::default() };
        let all_terms = if mode == Mode::Semantic { Vec::new() } else { terms(query) };
        if all_terms.is_empty() && !semantic {
            return Ok(res);
        }
        // Question and function words leave the lexical run unless they
        // are all there is.
        let content: Vec<String> = all_terms.iter().filter(|t| !STOP_TERMS.contains(&t.as_str())).cloned().collect();
        let (lexical_terms, stopped_words): (Vec<String>, Vec<String>) = if content.is_empty() {
            (all_terms.clone(), Vec::new())
        } else {
            (content, all_terms.iter().filter(|t| STOP_TERMS.contains(&t.as_str())).cloned().collect())
        };
        res.skipped_terms.extend(stopped_words);

        // Lexical run: postings per term, stop-terms by share, AND then OR.
        let stop_df = (self.pack.chunk_count() as f32 * STOP_SHARE) as usize;
        let mut lists: Vec<(String, Vec<ChunkId>)> = Vec::new();
        let mut stopped: Vec<(String, Vec<ChunkId>)> = Vec::new();
        for t in &lexical_terms {
            match self.lexical.lookup(t)? {
                Some(ids) if ids.len() > stop_df => stopped.push((t.clone(), ids)),
                Some(ids) => lists.push((t.clone(), ids)),
                None => res.skipped_terms.push(t.clone()),
            }
        }
        if lists.is_empty() {
            lists = std::mem::take(&mut stopped);
        } else {
            res.skipped_terms.extend(stopped.into_iter().map(|(t, _)| t));
        }
        res.terms = lists.iter().map(|(t, _)| t.clone()).collect();
        let n_terms = lists.len();
        let mut lexical: Vec<(ChunkId, usize)> = Vec::new();
        if n_terms > 0 {
            lists.sort_by_key(|(_, ids)| ids.len());
            let mut and = lists[0].1.clone();
            for (_, ids) in &lists[1..] {
                and = intersect(&and, ids);
                if and.is_empty() {
                    break;
                }
            }
            if !and.is_empty() {
                lexical = and.into_iter().map(|c| (c, n_terms)).collect();
            } else {
                res.any_term = true;
                let mut count: HashMap<ChunkId, usize> = HashMap::new();
                for (_, ids) in &lists {
                    for &c in ids {
                        *count.entry(c).or_insert(0) += 1;
                    }
                }
                lexical = count.into_iter().collect();
            }
        }
        res.lexical_candidates = lexical.len();
        // Rank: more terms matched, then the more-read article, then id.
        lexical.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| self.popularity_of(b.0).partial_cmp(&self.popularity_of(a.0)).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.0.cmp(&b.0))
        });
        lexical.truncate(LEXICAL_RUN);

        // Entity run: the whole query as a title, plus any Q-ids in it.
        let mut entity: Vec<ChunkId> = Vec::new();
        let key = entity_key(query);
        if let Some(ids) = self.entity.lookup(&key)? {
            entity.extend(ids);
        }
        for t in &all_terms {
            if t.len() > 1 && t.starts_with('q') && t[1..].bytes().all(|b| b.is_ascii_digit())
                && let Some(ids) = self.entity.lookup(t)?
            {
                entity.extend(ids);
            }
        }
        entity.dedup();
        res.entity_candidates = entity.len();

        // Semantic run.
        let semantic_hits = match (semantic, query_vec) {
            (true, Some(q)) => self.semantic_run(q, SEMANTIC_RUN),
            _ => Vec::new(),
        };
        res.semantic_candidates = semantic_hits.len();

        // Fusion.
        let blank = |c: ChunkId| Hit { chunk: c, doc: 0, score: 0.0, lexical_rank: None, entity_rank: None, semantic_rank: None, cosine: None, matched: 0, title_match: false };
        let mut fused: HashMap<ChunkId, Hit> = HashMap::new();
        let lexical_weight = if res.any_term { self.lexical_or_weight } else { self.lexical_weight };
        for (rank, (c, matched)) in lexical.iter().enumerate() {
            let h = fused.entry(*c).or_insert_with(|| blank(*c));
            h.score += lexical_weight / (RRF_K + rank as f32 + 1.0);
            h.lexical_rank = Some(rank + 1);
            h.matched = *matched;
        }
        for (rank, c) in entity.iter().enumerate() {
            let h = fused.entry(*c).or_insert_with(|| blank(*c));
            h.score += 1.0 / (RRF_K + rank as f32 + 1.0);
            h.entity_rank = Some(rank + 1);
        }
        for (rank, (c, cos)) in semantic_hits.iter().enumerate() {
            let h = fused.entry(*c).or_insert_with(|| blank(*c));
            h.score += 1.0 / (RRF_K + rank as f32 + 1.0);
            h.semantic_rank = Some(rank + 1);
            h.cosine = Some(*cos);
        }
        // Resolve documents, boost title matches, keep one chunk per document.
        let lower_query = entity_key(query);
        let mut best: HashMap<DocId, Hit> = HashMap::new();
        for (c, mut h) in fused {
            let Some(d) = self.doc_of(c) else { continue };
            h.doc = d.id;
            let title = entity_key(&d.title);
            if !title.is_empty() && lower_query.contains(&title) {
                h.title_match = true;
                h.score *= TITLE_BOOST;
            }
            match best.get(&d.id) {
                Some(prev) if prev.score >= h.score => {}
                _ => {
                    best.insert(d.id, h);
                }
            }
        }
        let mut hits: Vec<Hit> = best.into_values().collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.chunk.cmp(&b.chunk)));
        hits.truncate(limit);
        res.hits = hits;
        res.elapsed_ms = t0.elapsed().as_secs_f32() * 1000.0;
        Ok(res)
    }

    /// The document a chunk belongs to: documents' chunk ranges are
    /// contiguous and ascending, so a binary search on `first_chunk`.
    pub fn doc_of(&self, chunk: ChunkId) -> Option<&Doc> {
        let i = self.docs.partition_point(|d| d.first_chunk <= chunk);
        // Empty documents share a first_chunk with the next; walk back to
        // the one that actually owns the chunk.
        self.docs[..i].iter().rev().find(|d| d.chunk_count > 0 && chunk < d.first_chunk + u64::from(d.chunk_count))
    }

    fn popularity_of(&self, chunk: ChunkId) -> f64 {
        self.doc_of(chunk).map_or(0.0, |d| d.popularity)
    }
}

fn intersect(a: &[ChunkId], b: &[ChunkId]) -> Vec<ChunkId> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_is_sorted_merge() {
        assert_eq!(intersect(&[1, 3, 5, 7], &[3, 4, 5, 8]), vec![3, 5]);
        assert_eq!(intersect(&[], &[1]), Vec::<u64>::new());
    }
}
