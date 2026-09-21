//! `build lexical <pack-dir>`: the postings stage (`specs/knowledge.md` §8).
//! Every chunk's terms → `lexical/<prefix>`; every document's title and
//! Wikidata id → `entity/<prefix>`, pointing at its first chunk. In memory
//! for this corpus (a few hundred MB at 258k chunks); a bigger corpus
//! would sort on disk.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use rill_knowledge::lexical::{entity_key, terms};
use rill_knowledge::{Pack, SHARD, doc, postings, text};

pub fn run(pack_dir: &Path) {
    let root = pack_dir.join("knowledge");
    let pack = Pack::open(&root).unwrap_or_else(|e| {
        eprintln!("opening {}: {e}", root.display());
        std::process::exit(1)
    });
    let t0 = Instant::now();
    let shards = pack.chunk_count().div_ceil(SHARD) as u32;
    let mut index: HashMap<String, Vec<u64>> = HashMap::new();
    let mut pairs = 0usize;
    for s in 0..shards {
        for c in text::read_shard(&root.join("text"), s).expect("read text shard") {
            for t in terms(&c.text) {
                index.entry(t).or_default().push(c.id);
                pairs += 1;
            }
        }
    }
    println!("lexical: {} terms, {pairs} postings from {} chunks ({:?})", index.len(), pack.chunk_count(), t0.elapsed());

    let t1 = Instant::now();
    let doc_shards = pack.doc_count().div_ceil(SHARD) as u32;
    let mut entity: HashMap<String, Vec<u64>> = HashMap::new();
    for s in 0..doc_shards {
        for d in doc::read_shard(&root.join("doc"), s).expect("read doc shard") {
            if d.chunk_count == 0 {
                continue;
            }
            let title = entity_key(&d.title);
            if !title.is_empty() {
                entity.entry(title).or_default().push(d.first_chunk);
            }
            if let Some(q) = &d.qid {
                entity.entry(q.to_ascii_lowercase()).or_default().push(d.first_chunk);
            }
        }
    }
    println!("entity: {} keys ({:?})", entity.len(), t1.elapsed());

    let t2 = Instant::now();
    let n_lex = write_index(&root.join("lexical"), index);
    let n_ent = write_index(&root.join("entity"), entity);
    let mut m = pack.manifest().clone();
    m.set("stage", "lexical").set("lexical.terms", n_lex.0.to_string()).set("lexical.files", n_lex.1.to_string()).set("entity.terms", n_ent.0.to_string()).set("entity.files", n_ent.1.to_string());
    m.write(&root.join("manifest")).expect("write manifest");
    println!("wrote lexical/ ({} files) and entity/ ({} files) ({:?}); manifest stage=lexical", n_lex.1, n_ent.1, t2.elapsed());
}

/// Write a postings directory: one sorted file per prefix. Returns
/// (terms, files).
fn write_index(dir: &Path, index: HashMap<String, Vec<u64>>) -> (usize, usize) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).expect("create index dir");
    let mut by_prefix: BTreeMap<String, Vec<(String, Vec<u64>)>> = BTreeMap::new();
    let mut n = 0usize;
    for (term, mut ids) in index {
        ids.sort_unstable();
        ids.dedup();
        by_prefix.entry(postings::prefix_of(&term)).or_default().push((term, ids));
        n += 1;
    }
    let files = by_prefix.len();
    for (prefix, mut entries) in by_prefix {
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut f = std::io::BufWriter::new(fs::File::create(dir.join(prefix)).expect("create postings file"));
        for (term, ids) in entries {
            f.write_all(postings::format_line(&term, &ids).as_bytes()).expect("write");
            f.write_all(b"\n").expect("write");
        }
        f.flush().expect("flush");
    }
    (n, files)
}
