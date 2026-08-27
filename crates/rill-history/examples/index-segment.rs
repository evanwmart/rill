//! Index a `.rhs` segment and search it — the end-to-end check that the
//! seal-time index works against real captured frames, and a preview of
//! what `rill history grep` will do.
//!
//!   cargo run -p rill-history --example index-segment -- seg.rhs [query…]

use rill_history::index;
use rill_history::segment::read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = std::path::Path::new(&args[0]);

    let t0 = std::time::Instant::now();
    let seg = read(path).expect("read segment");
    let read_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if let Some(why) = &seg.stopped {
        println!("note: segment tail torn ({why})");
    }

    let t1 = std::time::Instant::now();
    let idx = index::build(&seg.events, 0);
    let build_ms = t1.elapsed().as_secs_f64() * 1000.0;

    let transcript_bytes: usize = idx.transcript.iter().map(|e| e.text.len()).sum();
    let frame_bytes: usize = seg
        .events
        .iter()
        .filter_map(|e| match &e.event {
            rill_history::Event::Frame { bytes, .. } => Some(bytes.len()),
            _ => None,
        })
        .sum();

    println!("segment {}", path.display());
    println!("  {} events, read in {read_ms:.1} ms", seg.events.len());
    println!("  index built in {build_ms:.1} ms");
    println!("  transcript: {} entries, {transcript_bytes} B", idx.transcript.len());
    println!(
        "  vs frames:  {frame_bytes} B  ({:.2}% — the tiered-decay ratio)",
        100.0 * transcript_bytes as f64 / frame_bytes.max(1) as f64
    );
    println!("  postings: {} tokens, bloom {} B", idx.postings.len(), idx.bloom.bytes());
    println!("  span: {:?} ms", idx.span);

    for q in args.iter().skip(1) {
        let t = std::time::Instant::now();
        let hits = idx.search(q);
        let us = t.elapsed().as_secs_f64() * 1e6;
        println!("\nsearch {q:?} — {} hits in {us:.0} µs", hits.len());
        for h in hits.iter().take(5) {
            let snippet: String = h.text.chars().take(90).collect();
            println!("  {:>7} ms  win {}  {snippet}", h.t_ms, h.window);
        }
    }

    // The agent's hot path: the most recent transcript, no frame decoding.
    let t = std::time::Instant::now();
    let tail = idx.tail(5);
    let us = t.elapsed().as_secs_f64() * 1e6;
    println!("\nagent tail(5) in {us:.0} µs:");
    for e in tail {
        let snippet: String = e.text.chars().take(70).collect();
        println!("  {:>7} ms  {snippet}", e.t_ms);
    }
}
