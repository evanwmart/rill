//! Fuzz target: arbitrary bytes → `rill_doc::decode` (the `.rill` binary
//! document parser — strict cross-table index validation is the high-value
//! surface per TODO P1). Must never panic, hang, or over-allocate.
//! Run with `cargo +nightly fuzz run doc_decode` from the repo root.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rill_doc::{Node, decode, encode};

fuzz_target!(|data: &[u8]| {
    let Ok(document) = decode(data) else {
        return;
    };

    // A document may contain node types from the *ignorable* half that this
    // build has never heard of; `decode` keeps them as placeholders so the
    // rest of the tree still renders (document-format.md §"critical/
    // ignorable"). Their bodies are skipped, not stored — so this build
    // cannot reproduce them, and `encode` says so rather than inventing
    // bytes. That refusal is the property worth pinning: re-emitting
    // content you could not inspect is a laundering channel, and Rill's
    // posture is that what the semantic layer cannot see, it does not pass
    // on.
    let has_unknown = document.nodes.iter().any(|n| matches!(n, Node::UnknownIgnorable { .. }));
    if has_unknown {
        let refused = encode(&document).expect_err("encoding an unknown node must be refused");
        assert!(
            refused.to_string().contains("unknown node"),
            "refusal should name the reason, got: {refused}"
        );
        return;
    }

    // Everything this build fully understands is canonical: decoding and
    // re-encoding must reproduce the same document, byte for byte.
    let bytes = encode(&document).expect("decoded document must re-encode");
    let again = decode(&bytes).expect("re-encoded document must decode");
    assert_eq!(document, again, "document round-trip changed the document");
});
