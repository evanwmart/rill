//! `build embed <pack-dir> --model <dir> [--backend gpu|cpu] [--batch N]`:
//! the vector stage (`specs/knowledge.md` §5, §6). Every chunk, prefixed
//! with its document title and section, through the embedder; unit
//! vectors quantised to int8 into `vector/full`, projected and quantised
//! into `vector/coarse`; the model contract into the manifest.

use std::path::Path;
use std::time::Instant;

use rill_knowledge::coarse::Projector;
use rill_knowledge::{DIM, Pack, SHARD, doc, text, vector};
use rill_knowledge_embed::QUERY_PREFIX;

/// Default projection seed: "rill" in ASCII.
pub const DEFAULT_SEED: u64 = 0x7269_6c6c;

/// The text the embedder sees for a chunk (manifest `embedding.doc_prefix`).
pub fn doc_input(title: &str, section: &str, text: &str) -> String {
    if section.is_empty() { format!("{title}\n{text}") } else { format!("{title} — {section}\n{text}") }
}

pub fn run(args: &[String]) {
    let mut pack_dir: Option<String> = None;
    let mut model: Option<String> = None;
    let mut backend = "gpu".to_string();
    let mut batch = 128usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                model = args.get(i + 1).cloned();
                i += 2;
            }
            "--backend" => {
                backend = args.get(i + 1).cloned().unwrap_or(backend);
                i += 2;
            }
            "--batch" => {
                batch = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(batch);
                i += 2;
            }
            other if pack_dir.is_none() => {
                pack_dir = Some(other.to_string());
                i += 1;
            }
            other => {
                eprintln!("build embed: unexpected argument {other}");
                std::process::exit(2);
            }
        }
    }
    let (Some(pack_dir), Some(model)) = (pack_dir, model) else {
        eprintln!("usage: rill-knowledge-build build embed <pack-dir> --model <dir> [--backend gpu|cpu] [--batch N]");
        std::process::exit(2);
    };
    let root = Path::new(&pack_dir).join("knowledge");
    let pack = Pack::open(&root).unwrap_or_else(|e| {
        eprintln!("opening {}: {e}", root.display());
        std::process::exit(1)
    });
    let t0 = Instant::now();
    let embedder = rill_knowledge_embed::open(Path::new(&model), &backend).unwrap_or_else(|e| {
        eprintln!("opening model {model}: {e}");
        std::process::exit(1)
    });
    println!("model {} {} on {backend}, loaded in {:?}", embedder.model_name(), embedder.model_hash(), t0.elapsed());
    // Titles for the prefix: the document table, in memory.
    let mut titles: Vec<String> = Vec::with_capacity(pack.doc_count() as usize);
    for s in 0..pack.doc_count().div_ceil(SHARD) as u32 {
        titles.extend(doc::read_shard(&root.join("doc"), s).expect("read doc shard").into_iter().map(|d| d.title));
    }
    let seed = pack.manifest().get("projection.seed").and_then(|h| u64::from_str_radix(h, 16).ok()).unwrap_or(DEFAULT_SEED);
    let projector = Projector::new(seed);

    let tmp_full = root.join("vector/full.tmp");
    let tmp_coarse = root.join("vector/coarse.tmp");
    let _ = std::fs::remove_dir_all(&tmp_full);
    let _ = std::fs::remove_dir_all(&tmp_coarse);
    let mut full = vector::Writer::create(&tmp_full, DIM).expect("full writer");
    let mut coarse = vector::Writer::create(&tmp_coarse, rill_knowledge::COARSE_DIM).expect("coarse writer");
    let shards = pack.chunk_count().div_ceil(SHARD) as u32;
    let t1 = Instant::now();
    let mut done = 0u64;
    for s in 0..shards {
        let ts = Instant::now();
        let chunks = text::read_shard(&root.join("text"), s).expect("read text shard");
        let inputs: Vec<String> = chunks.iter().map(|c| doc_input(titles.get(c.doc as usize).map_or("", String::as_str), &c.section, &c.text)).collect();
        // Sort by length so each batch pads to a similar longest input;
        // scatter the vectors back into chunk order.
        let mut order: Vec<usize> = (0..inputs.len()).collect();
        order.sort_by_key(|&i| inputs[i].len());
        let mut vectors: Vec<Option<Vec<f32>>> = vec![None; inputs.len()];
        for group in order.chunks(batch) {
            let texts: Vec<&str> = group.iter().map(|&i| inputs[i].as_str()).collect();
            let out = embedder.embed(&texts);
            for (&i, v) in group.iter().zip(out) {
                vectors[i] = Some(v);
            }
        }
        for (c, v) in chunks.iter().zip(vectors) {
            let v = v.expect("every chunk embedded");
            debug_assert_eq!(full.next_id(), c.id);
            full.push(&vector::quantize(&v)).expect("write full");
            coarse.push(&projector.project_i8(&v)).expect("write coarse");
        }
        done += chunks.len() as u64;
        let rate = done as f64 / t1.elapsed().as_secs_f64();
        println!("shard {s:04x}: {} chunks in {:?}, {done}/{} total, {rate:.0} chunks/s", chunks.len(), ts.elapsed(), pack.chunk_count());
    }
    let n = full.finish().expect("finish full");
    coarse.finish().expect("finish coarse");
    let _ = std::fs::remove_dir_all(root.join("vector/full"));
    let _ = std::fs::remove_dir_all(root.join("vector/coarse"));
    std::fs::rename(&tmp_full, root.join("vector/full")).expect("rename full");
    std::fs::rename(&tmp_coarse, root.join("vector/coarse")).expect("rename coarse");

    let mut m = pack.manifest().clone();
    m.set("stage", "vector")
        .set("embedding.model", embedder.model_name())
        .set("embedding.model_hash", embedder.model_hash().to_string())
        .set("embedding.pooling", "cls")
        .set("embedding.query_prefix", QUERY_PREFIX.trim_end())
        .set("embedding.doc_prefix", "title-section")
        .set("embedding.max_tokens", rill_knowledge_embed::MAX_TOKENS.to_string())
        .set("projection.seed", format!("{seed:016x}"));
    m.write(&root.join("manifest")).expect("write manifest");
    println!("embedded {n} chunks in {:?}; manifest stage=vector, model {}", t1.elapsed(), embedder.model_hash());
}
