//! `math-coverage <pack-dir> [--show N]`: how much of the corpus's TeX the
//! renderer can actually show (`specs/knowledge-math.md` §6).
//!
//! The subset in `rill-math` was chosen from what Simple English articles
//! looked likely to use, not from what they do use. This measures it before
//! the structural backend is built on top — the same order as brute force
//! before the tree: know the ceiling, then decide what is worth indexing.
//!
//! Reports, per tier, how many formulas parse and how many render, and
//! lists the commonest failures so the subset can be widened where it pays.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use rill_knowledge::{Pack, SHARD, text};
use rill_math::{Segment, segments};

#[derive(Default)]
struct Tally {
    total: usize,
    parsed: usize,
    rendered: usize,
    /// Failing source → how often, for the report's tail.
    failures: HashMap<String, usize>,
}

impl Tally {
    fn see(&mut self, tex: &str) {
        self.total += 1;
        let parsed = rill_math::parse(tex);
        if parsed.is_some() {
            self.parsed += 1;
        }
        if parsed.as_ref().and_then(rill_math::to_unicode).is_some() {
            self.rendered += 1;
        } else {
            *self.failures.entry(tex.trim().to_string()).or_insert(0) += 1;
        }
    }

    fn report(&self, name: &str, show: usize) {
        if self.total == 0 {
            println!("{name}: none found");
            return;
        }
        let pct = |n: usize| 100.0 * n as f64 / self.total as f64;
        println!("{name}: {} formulas, {} parse ({:.1}%), {} render to Unicode ({:.1}%)", self.total, self.parsed, pct(self.parsed), self.rendered, pct(self.rendered));
        if show == 0 || self.failures.is_empty() {
            return;
        }
        let mut worst: Vec<(&String, &usize)> = self.failures.iter().collect();
        worst.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        println!("  commonest failures ({} distinct):", self.failures.len());
        for (tex, n) in worst.into_iter().take(show) {
            let flat: String = tex.chars().take(110).collect();
            println!("    {n:>6}  {flat}");
        }
    }
}

pub fn run(args: &[String]) {
    let mut dir: Option<&str> = None;
    let mut show = 20usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--show" => {
                show = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(show);
                i += 2;
            }
            other if dir.is_none() => {
                dir = Some(other);
                i += 1;
            }
            other => {
                eprintln!("math-coverage: unexpected argument {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!("usage: rill-knowledge-build math-coverage <pack-dir> [--show N]");
        std::process::exit(2);
    };
    let root = Path::new(dir).join("knowledge");
    let pack = Pack::open(&root).unwrap_or_else(|e| {
        eprintln!("opening {}: {e}", root.display());
        std::process::exit(1)
    });

    let t0 = Instant::now();
    let (mut display, mut inline) = (Tally::default(), Tally::default());
    let (mut chunks, mut with_math) = (0usize, 0usize);
    // Whole shards, not `Pack::chunk` per id: this reads every chunk once
    // and a shard is under 20 MiB.
    for shard in 0..pack.chunk_count().div_ceil(SHARD) as u32 {
        let rows = text::read_shard(&root.join("text"), shard).unwrap_or_else(|e| {
            eprintln!("reading text shard {shard:04x}: {e}");
            std::process::exit(1)
        });
        for c in &rows {
            chunks += 1;
            let mut any = false;
            for seg in segments(&c.text) {
                match seg {
                    Segment::Display(tex) => {
                        display.see(tex);
                        any = true;
                    }
                    Segment::Inline(tex) => {
                        inline.see(tex);
                        any = true;
                    }
                    Segment::Text(_) => {}
                }
            }
            if any {
                with_math += 1;
            }
        }
    }

    println!("{chunks} chunks scanned in {:?}; {with_math} carry a formula ({:.2}%)", t0.elapsed(), 100.0 * with_math as f64 / chunks.max(1) as f64);
    println!();
    // Display math is what the structural backend would serve; inline math
    // has to fit one text node, so its Unicode rate is the whole story.
    display.report("display ($$…$$)", show);
    println!();
    inline.report("inline ($…$)", show);
}
