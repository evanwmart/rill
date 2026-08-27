//! Convert a `.rillrec` recording into a `.rhs` segment — an
//! end-to-end check of the segment codec against real captured data, and the
//! way a session someone recorded becomes history someone can query.
//!
//! Not a migration: `.rillrec` is the current session-recording format (the
//! compositor writes it, `rill-vector --replay` plays it back). This bridges
//! two live formats — see the module docs on `rill_ui::recording` for why
//! there are two.
//!
//!   cargo run -p rill-history --example convert-rillrec -- in.rillrec out.rhs

use rill_history::event::{Event, Stamped, T0_ROUTINE, WindowState};
use rill_history::segment::{ChunkCodec, Header, SegmentWriter, read};
use rill_ui::recording::{RecEvent, decode_lossy};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (src, dst) = (&args[0], &args[1]);
    let bytes = std::fs::read(src).expect("read source");
    let (w, h, events, stopped) = decode_lossy(&bytes).expect("decode .rillrec");
    println!("source: {}x{}, {} events, {} bytes", w, h, events.len(), bytes.len());
    if let Some(why) = stopped {
        println!("  (source tail torn: {why})");
    }

    let header = Header {
        version: 1,
        device: "convert".into(),
        wall_start_ms: rill_history::wall_ms(),
        keyslots: Vec::new(),
    };
    let mut writer =
        SegmentWriter::create(std::path::Path::new(dst), &header, ChunkCodec::Zstd, 3)
            .expect("create segment");

    let mut prev_t = 0u32;
    let mut frames = 0usize;
    let mut texts = 0usize;
    // What each window last said, so a transcript entry is written only when
    // the text actually changes — the same rule the index applies.
    let mut last_text: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    for e in &events {
        let mut dt_ms = e.t_ms.saturating_sub(prev_t);
        prev_t = e.t_ms;
        // The transcript rides *beside* the frame, written by the producer
        // that already has the bytes. Without it the transcript is only
        // recoverable by decoding the frames, which means the frames can
        // never be dropped — and dropping them at 90 days while keeping the
        // transcript forever is the whole of decision 3.
        if let RecEvent::Frame { id, bytes } = &e.event
            && let Some(text) = rill_history::index::frame_text(bytes)
            && last_text.get(id) != Some(&text)
        {
            last_text.insert(*id, text.clone());
            texts += 1;
            writer
                .append(&Stamped {
                    dt_ms,
                    tier: T0_ROUTINE,
                    event: Event::Text { id: *id, text },
                })
                .expect("append text");
            // The text carried this event's delta; the frame it describes
            // happened at the same instant, so it adds none.
            dt_ms = 0;
        }
        let event = match &e.event {
            RecEvent::Window { id, x, y, w, h, title, vector } => Event::Window(WindowState {
                id: *id,
                x: *x,
                y: *y,
                w: *w,
                h: *h,
                title: title.chars().take(200).collect(),
                app: String::new(),
                vector: *vector,
                tier: T0_ROUTINE,
            }),
            RecEvent::Closed { id } => Event::Closed { id: *id },
            RecEvent::Order { ids } => Event::Order { ids: ids.clone() },
            RecEvent::Frame { id, bytes } => {
                frames += 1;
                Event::Frame { id: *id, bytes: bytes.clone() }
            }
            // Legacy pointer motion has no home in the new vocabulary (only
            // clicks/drags/scrolls are recorded now) — dropped, deliberately.
            RecEvent::Pointer { .. } => continue,
        };
        writer.append(&Stamped { dt_ms, tier: T0_ROUTINE, event }).expect("append");
    }
    let count = writer.events_written();
    let path = writer.finish().expect("finish");

    let out_len = std::fs::metadata(&path).unwrap().len();
    println!(
        "wrote {}: {count} events ({frames} frames, {texts} transcript entries), \
         {out_len} bytes ({:.1}x smaller)",
        path.display(),
        bytes.len() as f64 / out_len as f64
    );

    // Read it back and prove the frames survived byte-exact.
    let back = read(&path).expect("read back");
    assert!(back.stopped.is_none(), "clean segment reported damage: {:?}", back.stopped);
    let src_frames: Vec<&Vec<u8>> = events
        .iter()
        .filter_map(|e| match &e.event {
            RecEvent::Frame { bytes, .. } => Some(bytes),
            _ => None,
        })
        .collect();
    let out_frames: Vec<&Vec<u8>> = back
        .events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Frame { bytes, .. } => Some(bytes),
            _ => None,
        })
        .collect();
    assert_eq!(src_frames.len(), out_frames.len(), "frame count changed");
    assert!(src_frames == out_frames, "frame bytes changed");
    println!("verified: {} frames byte-identical after round trip", out_frames.len());
}
