//! `inspect <pack-dir> [<chunk-id>…] [--doc <id>] [--random N]`: read a
//! pack back through the engine's `Pack` handle — the same seeks a query
//! makes — and print what is there.

use std::path::Path;

use rill_knowledge::Pack;

pub fn run(args: &[String]) {
    let Some(dir) = args.first() else {
        eprintln!("usage: rill-knowledge-build inspect <pack-dir> [<chunk-id>…] [--doc <id>] [--random N]");
        std::process::exit(2);
    };
    let root = Path::new(dir).join("knowledge");
    let pack = Pack::open(&root).unwrap_or_else(|e| {
        eprintln!("opening {}: {e}", root.display());
        std::process::exit(1)
    });
    println!("pack {}: {} chunks, {} documents, stage {}", root.display(), pack.chunk_count(), pack.doc_count(), pack.manifest().get("stage").unwrap_or("?"));
    let mut ids: Vec<u64> = Vec::new();
    let mut docs: Vec<u32> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--doc" => {
                docs.extend(args.get(i + 1).and_then(|s| s.parse::<u32>().ok()));
                i += 2;
            }
            "--random" => {
                let n: u64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(3);
                // A fixed stride, not a PRNG: deterministic, spread over the pack.
                let step = (pack.chunk_count() / n.max(1)).max(1);
                ids.extend((0..n).map(|k| (k * step + step / 2) % pack.chunk_count().max(1)));
                i += 2;
            }
            s => {
                ids.extend(s.parse::<u64>().ok());
                i += 1;
            }
        }
    }
    for id in ids {
        match pack.chunk(id) {
            Ok(c) => {
                let title = pack.doc(c.doc).map(|d| d.title).unwrap_or_else(|e| format!("<doc error: {e}>"));
                println!("\n[chunk {id:#x}] doc {:#x} {title:?} section {:?} ({} words)\n{}", c.doc, c.section, c.text.split_whitespace().count(), c.text);
            }
            Err(e) => println!("\n[chunk {id:#x}] {e}"),
        }
    }
    for id in docs {
        match pack.doc(id) {
            Ok(d) => {
                println!("\n[doc {id:#x}] {:?} page {} {} popularity {:.3e} chunks {:#x}..+{}", d.title, d.page_id, d.qid.as_deref().unwrap_or("-"), d.popularity, d.first_chunk, d.chunk_count);
                for c in d.first_chunk..d.first_chunk + u64::from(d.chunk_count) {
                    if let Ok(ch) = pack.chunk(c) {
                        println!("  {c:#x} [{}] {}…", ch.section, ch.text.chars().take(90).collect::<String>().replace('\n', " "));
                    }
                }
            }
            Err(e) => println!("\n[doc {id:#x}] {e}"),
        }
    }
}
