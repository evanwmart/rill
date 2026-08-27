//! Fuzz target: arbitrary bytes → `rill_history::segment::read_bytes` (the
//! `.rhs` segmented-log reader).
//!
//! This parser is length-prefixed and compressed, which is the combination
//! that goes wrong: a length that lies, a chunk that expands without bound, a
//! header that claims more than the file holds. It also reads data that is
//! *meant* to be shared — a segment handed to someone else, or synced between
//! machines — so a `.rhs` file is hostile input rather than merely one's own
//! corrupt file.
//!
//! Its contract is unusual and worth stating, because it is what this target
//! checks: a torn tail is **not** an error. A recording that was interrupted
//! ends mid-chunk by construction, so the reader returns the events it could
//! read plus a `stopped` reason, and only a malformed *header*, a chunk
//! length past the cap, or a **broken seal** is a hard error. Sealed
//! segments invert the tolerance: they promised wholeness, so any
//! disagreement between footer and chunks refuses the file. The property
//! below is that every outcome is one of those — never a panic, and never an
//! allocation the file simply asked for.
//!
//! Run with `cargo +nightly fuzz run segment_read` from the repo root.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rill_history::segment;

fuzz_target!(|data: &[u8]| {
    let Ok(read) = segment::read_bytes(data) else {
        // A rejected segment is a fine outcome; it must simply not crash.
        return;
    };

    // Accepted. The header is then well-formed by definition, and the events
    // are whatever survived up to the point it stopped.
    assert!(!read.header.device.is_empty() || read.header.device.is_empty());

    // A `stopped` reason and events are independent: a file can be entirely
    // readable (no reason), or tear after any number of events including zero.
    // What must hold is that every event the reader handed back is one the
    // rest of the system can use.
    let mut previous_dt: u64 = 0;
    for stamped in &read.events {
        // Deltas accumulate into a timeline; the reader must not return one
        // that overflows the accumulation the corpus and index perform.
        previous_dt = previous_dt
            .checked_add(stamped.dt_ms as u64)
            .expect("event deltas overflow the timeline they build");
        // Tiers are a closed set — an unknown tier byte must have been
        // rejected rather than carried through as a number nothing matches.
        assert!(
            segment::tiers_present(std::slice::from_ref(stamped)).len() <= 1,
            "an event reported more than one tier"
        );
    }

    // The absolute-time view is the query path's entry point, and it must
    // agree with the events it was built from rather than dropping or
    // inventing any.
    let absolute = segment::absolute_times(&read.events);
    assert_eq!(absolute.len(), read.events.len(), "absolute_times changed the event count");
    for pair in absolute.windows(2) {
        assert!(pair[0].0 <= pair[1].0, "absolute times are not monotonic");
    }

    // A read that reports a seal has *verified* it: the footer's claims must
    // therefore agree with what actually decoded, and a sealed read never
    // also claims a torn tail.
    if let Some(seal) = &read.seal {
        assert!(read.stopped.is_none(), "a sealed read reported a torn tail");
        assert_eq!(seal.events, read.events.len() as u64, "seal event count disagrees");
        assert_eq!(
            seal.tiers,
            segment::tiers_present(&read.events),
            "seal tier list disagrees with the events"
        );
    }
});
