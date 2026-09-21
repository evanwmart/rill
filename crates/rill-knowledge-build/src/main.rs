//! `rill-knowledge-build <command> …` — see `specs/knowledge.md`.
//!
//! Commands land as their stages do; the build is a pipeline of
//! independently runnable steps so a failed stage reruns alone.

mod build;
mod chunk;
mod cirrus;
mod html;
mod ingest;
mod inspect;
mod lexical;
mod source;
mod zim;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe-source") => probe_source(&args[1..]),
        Some("build") => build::run(&args[1..]),
        Some("inspect") => inspect::run(&args[1..]),
        _ => {
            eprintln!("usage: rill-knowledge-build <command> …");
            std::process::exit(2);
        }
    }
}

/// `probe-source <zim> <cirrus-dump> [--fraction 1/3] [--sample N] [--limit N]`:
/// read the dump, take the cut, resolve every title in the ZIM, walk the
/// bodies, and report what the ingest will see.
fn probe_source(args: &[String]) {
    use std::path::Path;
    use std::time::Instant;

    let mut positional = Vec::new();
    let mut fraction = 1.0 / 3.0;
    let mut sample = 0usize;
    let mut limit = usize::MAX;
    let mut show: Vec<String> = Vec::new();
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
            "--sample" => {
                sample = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
                i += 2;
            }
            "--show" => {
                show.extend(args.get(i + 1).cloned());
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
    let [zim_path, dump_path] = positional.as_slice() else {
        eprintln!("usage: rill-knowledge-build probe-source <zim> <cirrus-dump> [--fraction 1/3] [--sample N] [--limit N]");
        std::process::exit(2);
    };

    let t0 = Instant::now();
    let metas = cirrus::read(Path::new(dump_path)).unwrap_or_else(|e| {
        eprintln!("reading {dump_path}: {e}");
        std::process::exit(1)
    });
    let total = metas.len();
    let (cut, threshold) = cirrus::cut(metas, fraction);
    println!("dump: {total} namespace-0 articles ({:?})", t0.elapsed());
    println!("cut: top {fraction:.4} = {} articles, popularity >= {threshold:.3e}", cut.len());

    let t1 = Instant::now();
    let mut z = zim::Zim::open(Path::new(zim_path)).unwrap_or_else(|e| {
        eprintln!("opening {zim_path}: {e}");
        std::process::exit(1)
    });
    println!("zim: {} entries, mime types {} ({:?})", z.entry_count(), z.mime_types.len(), t1.elapsed());

    // Resolve every title, then read in (cluster, blob) order so each
    // cluster is decompressed exactly once.
    let mut found = 0usize;
    let mut via_redirect = 0usize;
    let mut not_html = 0usize;
    let mut missing: Vec<&str> = Vec::new();
    let mut jobs: Vec<(u32, u32, usize)> = Vec::new();
    for (mi, m) in cut.iter().enumerate().take(limit) {
        let url = m.title.replace(' ', "_");
        match z.lookup(b'C', &url).and_then(|idx| z.resolve(idx)) {
            Some((idx, hops)) => {
                found += 1;
                if hops > 0 {
                    via_redirect += 1;
                }
                if let Some(e) = z.entry(idx) {
                    if z.mime_types.get(e.mime as usize).is_none_or(|m| !m.starts_with("text/html")) {
                        not_html += 1;
                    }
                    if let zim::Kind::Item { cluster, blob } = e.kind {
                        jobs.push((cluster, blob, mi));
                    }
                }
            }
            None => missing.push(&m.title),
        }
    }
    let asked = found + missing.len();
    println!(
        "zim resolve: {found} of {asked} ({:.2}%), {via_redirect} via redirect, {not_html} not text/html, {} missing{}",
        100.0 * found as f64 / asked.max(1) as f64,
        missing.len(),
        if missing.is_empty() { String::new() } else { format!(": {:?}", &missing[..missing.len().min(10)]) }
    );
    jobs.sort_unstable();

    let t2 = Instant::now();
    let mut docs: Vec<source::Document> = Vec::with_capacity(jobs.len());
    let mut total_words = 0usize;
    let mut ratios: Vec<f64> = Vec::new();
    let mut lowest: Vec<(f64, String, usize, u32)> = Vec::new();
    let mut lead_ratios: Vec<f64> = Vec::new();
    let mut lead_low: Vec<(f64, String, usize, u32)> = Vec::new();
    let mut wikitables = 0usize;
    let mut other_tables = 0usize;
    let mut empty = 0usize;
    let mut malformed = 0usize;
    let mut clusters = 0usize;
    let mut cur: Option<(u32, zim::Cluster)> = None;
    for (cluster, blob, mi) in jobs {
        if cur.as_ref().is_none_or(|(c, _)| *c != cluster) {
            let c = z.cluster(cluster).unwrap_or_else(|e| {
                eprintln!("cluster {cluster}: {e}");
                std::process::exit(1)
            });
            cur = Some((cluster, c));
            clusters += 1;
        }
        let bytes = cur.as_ref().unwrap().1.blob(blob).unwrap_or(&[]);
        let html = String::from_utf8_lossy(bytes);
        let walked = html::walk(&html);
        if !walked.unclosed.is_empty() || walked.ended_skipping {
            malformed += 1;
            if show.contains(&cut[mi].title) || malformed <= 3 {
                println!("  walk of {:?} ended with {} open tags (skipping: {}): {:?}", cut[mi].title, walked.unclosed.len(), walked.ended_skipping, &walked.unclosed[..walked.unclosed.len().min(12)]);
            }
        }
        wikitables += walked.skipped_wikitables;
        other_tables += walked.skipped_other_tables;
        let m = &cut[mi];
        let doc = source::Document {
            page_id: m.page_id,
            title: m.title.clone(),
            wikibase_item: m.wikibase_item.clone(),
            popularity: m.popularity,
            sections: walked.sections,
        };
        let words = doc.word_count();
        if words == 0 {
            empty += 1;
        }
        total_words += words;
        if m.text_words >= 50 {
            let r = words as f64 / m.text_words as f64;
            ratios.push(r);
            lowest.push((r, m.title.clone(), words, m.text_words));
        }
        if m.opening_words >= 20 {
            let lead = doc.sections.first().filter(|s| s.heading.is_none()).map(|s| s.blocks.iter().map(|b| match b {
                source::Block::Paragraph(p) => p.split_whitespace().count(),
                source::Block::List(items) => items.iter().map(|i| i.split_whitespace().count()).sum(),
            }).sum::<usize>()).unwrap_or(0);
            let r = lead as f64 / m.opening_words as f64;
            lead_ratios.push(r);
            lead_low.push((r, m.title.clone(), lead, m.opening_words));
        }
        if show.contains(&m.title) {
            println!("\n=== {} ({} words walked, {} in dump)", m.title, words, m.text_words);
            for s in &doc.sections {
                println!("## [{}] {}", s.level, s.heading.as_deref().unwrap_or("(lead)"));
                for b in &s.blocks {
                    match b {
                        source::Block::Paragraph(p) => println!("{p}\n"),
                        source::Block::List(items) => {
                            for it in items {
                                println!("- {it}");
                            }
                            println!();
                        }
                    }
                }
            }
        }
        docs.push(doc);
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| ratios.get(((ratios.len() as f64) * p) as usize).copied().unwrap_or(f64::NAN);
    println!(
        "walk: {} documents from {clusters} clusters ({:?}); {total_words} words; {empty} empty; {malformed} walks ended with open tags; wikitables skipped {wikitables}, other tables skipped {other_tables}",
        docs.len(),
        t2.elapsed()
    );
    println!("walked/dump word ratio over {} articles: p10 {:.2} p50 {:.2} p90 {:.2}", ratios.len(), q(0.1), q(0.5), q(0.9));
    lead_ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let lq = |p: f64| lead_ratios.get(((lead_ratios.len() as f64) * p) as usize).copied().unwrap_or(f64::NAN);
    println!("walked lead / dump opening_text word ratio over {} articles: p10 {:.2} p50 {:.2} p90 {:.2}", lead_ratios.len(), lq(0.1), lq(0.5), lq(0.9));
    lead_low.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    for (r, t, w, d) in lead_low.iter().take(6) {
        println!("  low lead ratio {r:.2}: {t} ({w} walked / {d} opening)");
    }
    lowest.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    for (r, t, w, d) in lowest.iter().take(8) {
        println!("  low ratio {r:.2}: {t} ({w} walked / {d} dump)");
    }
    let sections: usize = docs.iter().map(|d| d.sections.len()).sum();
    let with_headings = docs.iter().filter(|d| d.sections.iter().any(|s| s.heading.is_some())).count();
    println!("structure: {sections} sections, {with_headings} documents with at least one heading");

    // Deterministic sample: a small LCG over the document index.
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..sample.min(docs.len()) {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let d = &docs[(seed >> 33) as usize % docs.len()];
        println!("
--- {} (page {}, {}, pop {:.2e}, {} words)", d.title, d.page_id, d.wikibase_item.as_deref().unwrap_or("no Q"), d.popularity, d.word_count());
        for s in &d.sections {
            let (paras, lists) = s.blocks.iter().fold((0, 0), |(p, l), b| match b {
                source::Block::Paragraph(_) => (p + 1, l),
                source::Block::List(_) => (p, l + 1),
            });
            println!("  [{}] {:<40} {paras} paragraphs, {lists} lists", s.level, s.heading.as_deref().unwrap_or("(lead)"));
        }
        if let Some(source::Block::Paragraph(p)) = d.sections.first().and_then(|s| s.blocks.first()) {
            let cut_at = p.char_indices().nth(240).map(|(i, _)| i).unwrap_or(p.len());
            println!("  {}{}", &p[..cut_at], if cut_at < p.len() { "…" } else { "" });
        }
    }
    println!("
total {:?}", t0.elapsed());
}
