//! THROWAWAY SPIKE (2026-08-11) — measures the load-bearing assumptions in
//! specs/history.md against a real `.rillrec` recording, before the writer is
//! built around them. Delete once the numbers land in the spec.
//!
//!   cargo run -p rill-ui --example history-spike -- <file.rillrec> <outdir>
//!
//! Questions it answers:
//!   1. frame bytes vs everything else        (the 99%/1% claim)
//!   2. transcript size vs frame size         (tiered-decay economics)
//!   3. how well frames compress (zstd, dict) (done in the shell after)
//!   4. frame dedup + naive command-level diff win

use std::collections::HashMap;
use std::io::Write;

use rill_ui::recording::{RecEvent, decode_lossy};
use rill_ui::{DrawCommand, stream};

/// The text a frame puts on screen, in paint order — a transcript line.
fn frame_text(cmds: &[DrawCommand]) -> String {
    let mut out = String::new();
    for c in cmds {
        if let DrawCommand::Text { text, .. } = c {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
    }
    out
}

/// Cheap stand-in for a real command-list diff: how many commands this frame
/// shares, positionally and by equality, with the previous frame of the same
/// window. Real diffing would do better (alignment, not position); this is a
/// deliberate lower bound.
fn shared_prefix_suffix(a: &[DrawCommand], b: &[DrawCommand]) -> usize {
    let mut same = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        if x == y {
            same += 1;
        }
    }
    same
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, outdir) = (&args[0], &args[1]);
    std::fs::create_dir_all(outdir).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let total_file = bytes.len();

    let (w, h, events, stopped) = decode_lossy(&bytes).unwrap();
    println!("recording {path}");
    println!("  output {w}x{h}, {} events, {total_file} bytes on disk", events.len());
    if let Some(why) = stopped {
        println!("  (tail truncated: {why})");
    }

    // ---- 1. byte budget by event kind ------------------------------------
    let mut frame_bytes = 0usize;
    let mut frame_count = 0usize;
    let mut other_count: HashMap<&str, usize> = HashMap::new();
    for e in &events {
        match &e.event {
            RecEvent::Frame { bytes, .. } => {
                frame_bytes += bytes.len();
                frame_count += 1;
            }
            RecEvent::Window { .. } => *other_count.entry("Window").or_default() += 1,
            RecEvent::Closed { .. } => *other_count.entry("Closed").or_default() += 1,
            RecEvent::Order { .. } => *other_count.entry("Order").or_default() += 1,
            RecEvent::Pointer { .. } => *other_count.entry("Pointer").or_default() += 1,
        }
    }
    let non_frame_bytes = total_file.saturating_sub(frame_bytes);
    println!("\n1. byte budget");
    println!("   frames        {frame_count:>6} events  {frame_bytes:>9} B  ({:.1}%)",
        100.0 * frame_bytes as f64 / total_file as f64);
    println!("   everything else              {non_frame_bytes:>9} B  ({:.1}%)",
        100.0 * non_frame_bytes as f64 / total_file as f64);
    let mut kinds: Vec<_> = other_count.iter().collect();
    kinds.sort();
    for (k, n) in kinds {
        println!("     {k:<8} {n:>6}");
    }

    // ---- 2. transcript extraction ----------------------------------------
    // One line per *change* in a window's text — repeats are what a real
    // transcript would drop, so measure both.
    let mut transcript_all = String::new();
    let mut transcript_changed = String::new();
    let mut last_text: HashMap<u32, String> = HashMap::new();
    let mut decoded_ok = 0usize;
    let mut decode_fail = 0usize;
    // frames per window, for the diff experiment
    let mut prev_cmds: HashMap<u32, Vec<DrawCommand>> = HashMap::new();
    let mut dedup: HashMap<u64, usize> = HashMap::new();
    let mut shared_total = 0usize;
    let mut cmd_total = 0usize;

    for e in &events {
        let RecEvent::Frame { id, bytes } = &e.event else { continue };
        // dedup by content hash
        let mut hash = 0xcbf29ce484222325u64;
        for b in bytes {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        *dedup.entry(hash).or_default() += 1;

        match stream::decode(bytes) {
            Ok(cmds) => {
                decoded_ok += 1;
                let text = frame_text(&cmds);
                if !text.is_empty() {
                    transcript_all.push_str(&text);
                    transcript_all.push('\n');
                    if last_text.get(id) != Some(&text) {
                        transcript_changed.push_str(&format!("{} {id} {text}\n", e.t_ms));
                        last_text.insert(*id, text);
                    }
                }
                if let Some(prev) = prev_cmds.get(id) {
                    shared_total += shared_prefix_suffix(prev, &cmds);
                    cmd_total += cmds.len();
                }
                prev_cmds.insert(*id, cmds);
            }
            Err(_) => decode_fail += 1,
        }
    }
    println!("\n2. transcript");
    println!("   frames decoded {decoded_ok} ok, {decode_fail} failed");
    println!("   transcript (every frame)   {:>9} B", transcript_all.len());
    println!("   transcript (on change)     {:>9} B  ({:.2}% of frame bytes)",
        transcript_changed.len(),
        100.0 * transcript_changed.len() as f64 / frame_bytes.max(1) as f64);

    // ---- 3. dedup + naive diff -------------------------------------------
    let unique = dedup.len();
    let dup_bytes: usize = 0; // reported via ratio below
    let _ = dup_bytes;
    println!("\n3. redundancy");
    println!("   unique frames {unique} of {frame_count}  ({:.1}% duplicates)",
        100.0 * (frame_count.saturating_sub(unique)) as f64 / frame_count.max(1) as f64);
    println!("   commands identical to previous frame of same window: {:.1}%",
        100.0 * shared_total as f64 / cmd_total.max(1) as f64);

    // ---- dump for shell-side zstd ----------------------------------------
    let mut f = std::fs::File::create(format!("{outdir}/frames.bin")).unwrap();
    for e in &events {
        if let RecEvent::Frame { bytes, .. } = &e.event {
            f.write_all(bytes).unwrap();
        }
    }
    std::fs::write(format!("{outdir}/transcript.txt"), &transcript_changed).unwrap();
    // individual frames, for dictionary training
    let dir = format!("{outdir}/samples");
    std::fs::create_dir_all(&dir).unwrap();
    for (i, e) in events.iter().enumerate() {
        if let RecEvent::Frame { bytes, .. } = &e.event
            && i % 7 == 0
        {
            std::fs::write(format!("{dir}/{i:05}.frame"), bytes).unwrap();
        }
    }
    println!("\nwrote {outdir}/{{frames.bin,transcript.txt,samples/}}");
}
