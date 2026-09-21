//! `eval <pack-dir> [--model <dir>] [--backend gpu|cpu] [--sample N]`:
//! the gold set (`specs/knowledge.md` §2). Automatic queries — each sampled
//! document's title, expected to return that document — plus a short list
//! of hand-written questions with their expected article, each run in
//! every mode the pack supports, reporting recall@1, recall@10 and MRR.

use std::path::Path;
use std::time::Instant;

use rill_knowledge::query::{Engine, Mode};
use rill_knowledge_embed::{Embedder, QUERY_PREFIX};

/// Questions whose answer article is unambiguous in Simple English.
const QUESTIONS: &[(&str, &str)] = &[
    ("why did einstein disagree with quantum mechanics", "EPR paradox"),
    ("what is the capital of france", "Paris"),
    ("how do plants turn sunlight into food", "Photosynthesis"),
    ("the highest mountain on earth", "Mount Everest"),
    ("largest moon of pluto", "Charon (moon)"),
    ("who painted the mona lisa", "Leonardo da Vinci"),
    ("what causes the seasons", "Season"),
    ("smallest prime number", "Prime number"),
    ("hormone released when you are scared", "Adrenaline"),
    ("the war between the north and south of the united states", "American Civil War"),
    ("animal with the longest neck", "Giraffe"),
    ("what do bees make", "Honey"),
];

pub fn run(args: &[String]) {
    let mut pack_dir: Option<String> = None;
    let mut model: Option<String> = None;
    let mut backend = "gpu".to_string();
    let mut sample = 500usize;
    let mut or_weight: Option<f32> = None;
    let mut lex_weight: Option<f32> = None;
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
            "--lex-weight" => {
                lex_weight = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--sample" => {
                sample = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(sample);
                i += 2;
            }
            "--or-weight" => {
                or_weight = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            other if pack_dir.is_none() => {
                pack_dir = Some(other.to_string());
                i += 1;
            }
            other => {
                eprintln!("eval: unexpected argument {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(pack_dir) = pack_dir else {
        eprintln!("usage: rill-knowledge-build eval <pack-dir> [--model <dir>] [--backend gpu|cpu] [--sample N] [--or-weight W] [--lex-weight W]");
        std::process::exit(2);
    };
    let root = Path::new(&pack_dir).join("knowledge");
    let t0 = Instant::now();
    let mut engine = Engine::open(&root).unwrap_or_else(|e| {
        eprintln!("opening {}: {e}", root.display());
        std::process::exit(1)
    });
    if or_weight.is_some() || lex_weight.is_some() {
        let (lw, ow) = (lex_weight.unwrap_or(rill_knowledge::query::LEXICAL_WEIGHT), or_weight.unwrap_or(rill_knowledge::query::LEXICAL_OR_WEIGHT));
        engine.set_lexical_weights(lw, ow);
        println!("lexical weights: all-terms {lw}, OR fallback {ow}");
    }
    println!("engine open in {:?}: {} chunks, vectors {}", t0.elapsed(), engine.pack().chunk_count(), engine.semantic_available());
    let embedder: Option<Box<dyn Embedder>> = model.map(|m| {
        rill_knowledge_embed::open(Path::new(&m), &backend).unwrap_or_else(|e| {
            eprintln!("opening model {m}: {e}");
            std::process::exit(1)
        })
    });
    if let (Some(e), Some(h)) = (&embedder, engine.pack().manifest().get("embedding.model_hash"))
        && e.model_hash().to_string() != h
    {
        eprintln!("model hash {} does not match the pack's {h}; semantic runs would be meaningless", e.model_hash());
        std::process::exit(1);
    }
    let modes: Vec<Mode> = if embedder.is_some() && engine.semantic_available() { vec![Mode::Lexical, Mode::Semantic, Mode::Fused] } else { vec![Mode::Lexical] };

    // Automatic gold: every k-th document with at least one chunk, its
    // title as the query, itself as the answer.
    let n_docs = engine.pack().doc_count() as usize;
    let step = (n_docs / sample.max(1)).max(1);
    let gold: Vec<(String, u32)> = (0..n_docs)
        .step_by(step)
        .filter_map(|i| engine.doc(i as u32).filter(|d| d.chunk_count > 0 && d.title.split_whitespace().count() >= 2).map(|d| (d.title.clone(), d.id)))
        .take(sample)
        .collect();
    println!("automatic gold set: {} title queries (every {step}th document, multi-word titles)", gold.len());
    for mode in &modes {
        let t = Instant::now();
        let (mut r1, mut r10, mut rr) = (0usize, 0usize, 0.0f64);
        for (title, doc) in &gold {
            let rank = rank_of(&engine, embedder.as_deref(), *mode, title, *doc);
            if rank == Some(1) {
                r1 += 1;
            }
            if rank.is_some_and(|r| r <= 10) {
                r10 += 1;
            }
            if let Some(r) = rank {
                rr += 1.0 / r as f64;
            }
        }
        let n = gold.len().max(1) as f64;
        println!(
            "  {:<9} recall@1 {:.3}  recall@10 {:.3}  MRR {:.3}  ({:.1} ms/query)",
            format!("{mode:?}"),
            r1 as f64 / n,
            r10 as f64 / n,
            rr / n,
            t.elapsed().as_secs_f64() * 1000.0 / n
        );
    }

    println!("questions (rank of the expected article in the top 10, per mode):");
    println!("  {:<58} {}", "question → expected", modes.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>().join("  "));
    for (q, expected) in QUESTIONS {
        let Some(doc) = find_title(&engine, expected) else {
            println!("  {q:<58} (expected article {expected:?} not in this pack)");
            continue;
        };
        let ranks: Vec<String> = modes.iter().map(|m| rank_of(&engine, embedder.as_deref(), *m, q, doc).map_or("–".to_string(), |r| format!("#{r}"))).collect();
        println!("  {:<58} {}", format!("{q} → {expected}"), ranks.join("       "));
    }
}

fn rank_of(engine: &Engine, embedder: Option<&dyn Embedder>, mode: Mode, query: &str, doc: u32) -> Option<usize> {
    let qvec = match (mode, embedder) {
        (Mode::Lexical, _) | (_, None) => None,
        (_, Some(e)) => Some(e.embed(&[&format!("{QUERY_PREFIX}{query}")]).remove(0)),
    };
    let res = engine.search_with(query, 10, qvec.as_deref(), mode).ok()?;
    res.hits.iter().position(|h| h.doc == doc).map(|p| p + 1)
}

fn find_title(engine: &Engine, title: &str) -> Option<u32> {
    (0..engine.pack().doc_count() as u32).find(|&i| engine.doc(i).is_some_and(|d| d.title == title))
}


/// `query <pack-dir> "<text>" [--model <dir>] [--backend gpu|cpu] [--mode lexical|semantic|fused] [--or-weight W]`:
/// one search, every hit with its per-run ranks — the explain view on the
/// command line.
pub fn query(args: &[String]) {
    let mut pack_dir: Option<String> = None;
    let mut text: Option<String> = None;
    let mut model: Option<String> = None;
    let mut backend = "gpu".to_string();
    let mut mode = Mode::Fused;
    let mut or_weight: Option<f32> = None;
    let mut lex_weight: Option<f32> = None;
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
            "--lex-weight" => {
                lex_weight = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--mode" => {
                mode = match args.get(i + 1).map(String::as_str) {
                    Some("lexical") => Mode::Lexical,
                    Some("semantic") => Mode::Semantic,
                    _ => Mode::Fused,
                };
                i += 2;
            }
            "--or-weight" => {
                or_weight = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            other if pack_dir.is_none() => {
                pack_dir = Some(other.to_string());
                i += 1;
            }
            other if text.is_none() => {
                text = Some(other.to_string());
                i += 1;
            }
            other => {
                eprintln!("query: unexpected argument {other}");
                std::process::exit(2);
            }
        }
    }
    let (Some(pack_dir), Some(text)) = (pack_dir, text) else {
        eprintln!("usage: rill-knowledge-build query <pack-dir> \"<text>\" [--model <dir>] [--backend gpu|cpu] [--mode lexical|semantic|fused] [--or-weight W]");
        std::process::exit(2);
    };
    let root = Path::new(&pack_dir).join("knowledge");
    let mut engine = Engine::open(&root).unwrap_or_else(|e| {
        eprintln!("opening {}: {e}", root.display());
        std::process::exit(1)
    });
    if or_weight.is_some() || lex_weight.is_some() {
        engine.set_lexical_weights(lex_weight.unwrap_or(rill_knowledge::query::LEXICAL_WEIGHT), or_weight.unwrap_or(rill_knowledge::query::LEXICAL_OR_WEIGHT));
    }
    let embedder: Option<Box<dyn Embedder>> = model.map(|m| {
        rill_knowledge_embed::open(Path::new(&m), &backend).unwrap_or_else(|e| {
            eprintln!("opening model {m}: {e}");
            std::process::exit(1)
        })
    });
    let qvec = match (mode, &embedder) {
        (Mode::Lexical, _) | (_, None) => None,
        (_, Some(e)) => Some(e.embed(&[&format!("{QUERY_PREFIX}{text}")]).remove(0)),
    };
    let res = engine.search_with(&text, 10, qvec.as_deref(), mode).unwrap_or_else(|e| {
        eprintln!("search: {e}");
        std::process::exit(1)
    });
    println!(
        "{:?} {:?}: {} hits in {:.1} ms; terms {:?} skipped {:?}; candidates lexical {} entity {} semantic {}{}",
        mode,
        text,
        res.hits.len(),
        res.elapsed_ms,
        res.terms,
        res.skipped_terms,
        res.lexical_candidates,
        res.entity_candidates,
        res.semantic_candidates,
        if res.any_term { " (OR fallback)" } else { "" }
    );
    for (i, h) in res.hits.iter().enumerate() {
        let title = engine.doc(h.doc).map(|d| d.title.clone()).unwrap_or_default();
        let section = engine.chunk(h.chunk).map(|c| c.section).unwrap_or_default();
        println!(
            "  {:>2}. {:.4}  {}{}   lex {} ({}/{})  ent {}  sem {}{}{}",
            i + 1,
            h.score,
            title,
            if section.is_empty() { String::new() } else { format!(" — {section}") },
            h.lexical_rank.map_or("-".into(), |r| format!("#{r}")),
            h.matched,
            res.terms.len(),
            h.entity_rank.map_or("-".into(), |r| format!("#{r}")),
            h.semantic_rank.map_or("-".into(), |r| format!("#{r}")),
            h.cosine.map_or(String::new(), |c| format!(" cos {c:.3}")),
            if h.title_match { "  title" } else { "" }
        );
    }
}
