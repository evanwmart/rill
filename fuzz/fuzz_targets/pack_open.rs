//! Fuzz target: arbitrary bytes → `rill_pack::Pack` (the `.rillpack`
//! reader). A pack is the artifact an *installed application* is made of,
//! so this parser eats bytes that arrived over the network and its failure
//! mode is the worst one available.
//!
//! Different in shape from the other targets, and deliberately so: `Pack`
//! is a path-based API — it seeks a `File` rather than parsing a slice, so
//! ranged reads never hold the whole artifact in memory. The bytes must
//! therefore land in a real file before each iteration, which makes this
//! target slower than the slice-based ones (thousands of executions a
//! second rather than millions). It writes to `/dev/shm` when that exists
//! so the cost is a memcpy rather than a disk write, and reuses one file
//! per process instead of littering.
//!
//! Run with `cargo +nightly fuzz run pack_open` from the repo root.

#![no_main]

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use rill_pack::{FOOTER_LEN, Pack};

/// One scratch file per process, in RAM where the platform offers it.
fn scratch() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let shm = PathBuf::from("/dev/shm");
        let dir = if shm.is_dir() { shm } else { std::env::temp_dir() };
        dir.join(format!("rill-fuzz-pack-{}.rillpack", std::process::id()))
    })
}

fuzz_target!(|data: &[u8]| {
    let path = scratch();
    {
        let Ok(mut file) = std::fs::File::create(path) else { return };
        if file.write_all(data).is_err() || file.flush().is_err() {
            return;
        }
    }

    let Ok(mut pack) = Pack::open(path) else {
        return;
    };

    // What `open` promises about a pack it accepted. Each of these is a
    // check `open` performs, restated where a violation is a crash rather
    // than a wrong answer somewhere later.
    let entries = pack.entries().to_vec();
    let file_len = data.len() as u64;
    let mut previous: Option<&str> = None;
    for entry in &entries {
        assert!(entry.path.starts_with('/'), "unvalidated path survived open: {:?}", entry.path);
        if let Some(prev) = previous {
            assert!(prev < entry.path.as_str(), "index not strictly sorted: {prev:?} then {:?}", entry.path);
        }
        previous = Some(&entry.path);

        let end = entry
            .blob_offset
            .checked_add(entry.encoded_size)
            .expect("blob range overflow survived open");
        assert!(end <= file_len - FOOTER_LEN, "blob range runs past the body");
        assert!(entry.encoding <= rill_pack::ENCODING_ZSTD, "unknown encoding survived open");

        // Lookup by path must find the entry the index lists, and the two
        // must agree — a mismatch here means `entry()`'s binary search and
        // the table it searches disagree about ordering.
        let found = pack.entry(&entry.path).expect("listed entry not findable by its own path");
        assert_eq!(found.hash, entry.hash, "entry() returned a different entry");
    }

    // Extraction never panics, whatever the blob holds: a truncated zstd
    // stream, a decoded size that lies, a hash that does not match.
    let verified = pack.verify().is_ok();
    for entry in &entries {
        let got = pack.get(&entry.path);
        if verified {
            // `verify` extracted and hash-checked every resource, so an
            // individual extraction of the same resource cannot now fail.
            let bytes = got
                .expect("get failed on a pack that verified")
                .expect("verified entry vanished");
            assert_eq!(
                bytes.len() as u64, entry.decoded_size,
                "decoded size disagrees with the index it verified against"
            );
            assert_eq!(rill_store::Hash::of(&bytes), entry.hash, "verified bytes do not hash to their entry");
        }
    }

    // Paths nobody stored answer None rather than reaching into the file.
    assert!(pack.get("/definitely-not-here").expect("absent path is not an error").is_none());
    assert!(pack.entry("").is_none());
});
