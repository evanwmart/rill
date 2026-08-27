//! Fuzz target: arbitrary bytes → `rill_ui::stream::decode`.
//!
//! This decoder eats live client bytes since W4 (the rill_stream_v1 attach
//! path), so it must never panic, hang, or over-allocate on hostile input.
//! On every input that decodes, it also asserts the codec's central
//! property: the decoded commands re-encode (encode validates the same
//! limits decode enforces, so this must succeed) and re-decode equal.
//! Run with `cargo +nightly fuzz run stream_decode` from the repo root.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rill_ui::stream::{decode, encode};

fuzz_target!(|data: &[u8]| {
    let Ok(commands) = decode(data) else {
        return;
    };
    let bytes = encode(&commands).expect("decoded stream must re-encode");
    let again = decode(&bytes).expect("re-encoded stream must decode");
    assert_eq!(commands, again, "stream round-trip changed the commands");
});
