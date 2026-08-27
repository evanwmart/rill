//! Fuzz target: arbitrary bytes → decode_header → decode_payload.
//!
//! Also asserts the codec's central property on every input that decodes:
//! encode(decode(bytes)) must itself decode to an equal Frame. Run with
//! `cargo +nightly fuzz run decode` from the repo root.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rill_protocol::{HEADER_LEN, decode_header, decode_payload, encode};

fuzz_target!(|data: &[u8]| {
    if data.len() < HEADER_LEN {
        return;
    }
    let header_bytes: &[u8; 16] = data[..HEADER_LEN].try_into().unwrap();
    let Ok(header) = decode_header(header_bytes) else {
        return;
    };
    let payload = &data[HEADER_LEN..];
    if payload.len() != header.payload_len as usize {
        // Still exercise the mismatch path — must error, not panic.
        let _ = decode_payload(&header, payload);
        return;
    }
    let Ok(frame) = decode_payload(&header, payload) else {
        return;
    };

    // Round-trip property: a decoded frame re-encodes and re-decodes equal.
    let mut bytes = Vec::new();
    encode(&frame, &mut bytes).expect("decoded frame must re-encode");
    let header2 = decode_header(bytes[..HEADER_LEN].try_into().unwrap())
        .expect("re-encoded header must decode");
    let frame2 = decode_payload(&header2, &bytes[HEADER_LEN..])
        .expect("re-encoded payload must decode");
    assert_eq!(frame, frame2, "round trip must be identity");
});
