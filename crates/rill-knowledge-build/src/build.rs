//! `build text <zim> <cirrus-dump> <out-dir> [--fraction 1/3] [--limit N]`:
//! the first stage — documents, chunks, text shards, document table, and
//! a manifest that says so. Later stages (embed, coarse, postings, tree)
//! add to the same tree and rewrite the manifest.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rill_knowledge::{doc, manifest, text};
use rill_store::Hash;

use crate::{chunk, cirrus, ingest};

fn hash_file(path: &Path) -> std::io::Result<Hash> {
    let mut f = fs::File::open(path)?;
    let mut hasher = rill_store::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

pub fn run(args: &[String]) {
    let Some(stage) = args.first().map(String::as_str) else {
        eprintln!("usage: rill-knowledge-build build text <zim> <cirrus-dump> <out-dir> [--fraction 1/3] [--limit N]");
        std::process::exit(2);
    };
    match stage {
        "text" => text_stage(&args[1..]),
        other => {
            eprintln!("unknown build stage {other:?} (stages: text)");
            std::process::exit(2);
        }
    }
}

fn text_stage(args: &[String]) {
    let mut positional = Vec::new();
    let mut fraction = 1.0 / 3.0;
    let mut limit = usize::MAX;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--fraction" => {
                fraction = args.get(i + 1).and_then(|s| cirrus::parse_fraction(s)).unwrap_or_else(|| {
                    eprintln!("--fraction wants a value in (0, 1], like 1/3");
                    std::process::exit(2)
                });
                i += 2;
            }
            "--limit" => {
                limit = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
                i += 2;
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }
    let [zim_path, dump_path, out] = positional.as_slice() else {
        eprintln!("usage: rill-knowledge-build build text <zim> <cirrus-dump> <out-dir> [--fraction 1/3] [--limit N]");
        std::process::exit(2);
    };
    let (zim_path, dump_path, out) = (Path::new(zim_path), Path::new(dump_path), PathBuf::from(out));
    if out.exists() {
        eprintln!("{}: already exists; a build never overwrites", out.display());
        std::process::exit(1);
    }
    let tmp = out.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp);
    let root = tmp.join("knowledge");
    fs::create_dir_all(&root).unwrap_or_else(|e| {
        eprintln!("creating {}: {e}", root.display());
        std::process::exit(1)
    });

    let t0 = Instant::now();
    let loaded = ingest::load(zim_path, dump_path, fraction, limit).unwrap_or_else(|e| {
        eprintln!("loading corpus: {e}");
        std::process::exit(1)
    });
    println!(
        "corpus: {} of {} dump articles in the cut (threshold {:.3e}), {} missing from the zim, {} documents loaded ({:?})",
        loaded.docs.len() + loaded.missing,
        loaded.dump_articles,
        loaded.cut_threshold,
        loaded.missing,
        loaded.docs.len(),
        t0.elapsed()
    );

    let t1 = Instant::now();
    let mut texts = text::Writer::create(root.join("text"), 0).expect("text writer");
    let mut docs = doc::Writer::create(root.join("doc")).expect("doc writer");
    let mut dedupe = chunk::Dedupe::default();
    let mut total_words = 0usize;
    let mut chunk_words: Vec<u32> = Vec::new();
    let mut empty_docs = 0usize;
    let mut over_cap = 0usize;
    for d in &loaded.docs {
        let pieces = chunk::chunk(d, &mut dedupe);
        let first = texts.next_id();
        if pieces.is_empty() {
            empty_docs += 1;
        }
        let doc_id = docs.next_id();
        for p in pieces {
            let w = p.text.split_whitespace().count();
            total_words += w;
            chunk_words.push(w as u32);
            if p.text.len() > rill_knowledge::MAX_CHUNK_BYTES {
                over_cap += 1;
            }
            texts.push(&text::Chunk { id: texts.next_id(), doc: doc_id, section: p.section, text: p.text }).unwrap_or_else(|e| {
                eprintln!("writing chunk: {e}");
                std::process::exit(1)
            });
        }
        let count = texts.next_id() - first;
        docs.push(&doc::Doc {
            id: doc_id,
            page_id: d.page_id,
            qid: d.wikibase_item.clone(),
            first_chunk: first,
            chunk_count: u16::try_from(count).unwrap_or(u16::MAX),
            popularity: d.popularity,
            title: d.title.clone(),
        })
        .unwrap_or_else(|e| {
            eprintln!("writing doc: {e}");
            std::process::exit(1)
        });
    }
    let chunks = texts.finish().expect("finish text");
    let documents = docs.finish().expect("finish docs");
    chunk_words.sort_unstable();
    let q = |p: f64| chunk_words.get(((chunk_words.len() as f64) * p) as usize).copied().unwrap_or(0);
    println!(
        "chunks: {chunks} from {documents} documents ({} empty after chunking, {} duplicates dropped, {over_cap} over the byte cap); {total_words} words; words/chunk p10 {} p50 {} p90 {} max {} ({:?})",
        empty_docs,
        dedupe.dropped,
        q(0.1),
        q(0.5),
        q(0.9),
        chunk_words.last().copied().unwrap_or(0),
        t1.elapsed()
    );

    let t2 = Instant::now();
    let zim_hash = hash_file(zim_path).expect("hash zim");
    let dump_hash = hash_file(dump_path).expect("hash dump");
    let mut m = manifest::base(chunks, u64::from(documents));
    m.set("stage", "text")
        .set("source.zim", zim_hash.to_string())
        .set("source.cirrus", dump_hash.to_string())
        .set("source.cut", format!("popularity-top-{fraction:.6}"))
        .set("source.cut_threshold", format!("{:.6e}", loaded.cut_threshold))
        .set("source.wiki", "simple.wikipedia.org")
        .set("chunk.target_words", chunk::TARGET_WORDS.to_string())
        .set("chunk.max_words", chunk::MAX_WORDS.to_string())
        .set("chunk.min_words", chunk::MIN_WORDS.to_string())
        .set("chunk.max_bytes", rill_knowledge::MAX_CHUNK_BYTES.to_string());
    m.write(&root.join("manifest")).expect("write manifest");
    fs::rename(&tmp, &out).unwrap_or_else(|e| {
        eprintln!("renaming {} to {}: {e}", tmp.display(), out.display());
        std::process::exit(1)
    });
    let mut bytes = 0u64;
    let mut files = 0usize;
    for sub in ["text", "doc"] {
        for e in fs::read_dir(out.join("knowledge").join(sub)).expect("read out").flatten() {
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            files += 1;
        }
    }
    println!("wrote {} : {files} files, {:.1} MiB; sources hashed ({:?})", out.display(), bytes as f64 / 1048576.0, t2.elapsed());
}
