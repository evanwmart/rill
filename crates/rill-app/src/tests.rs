use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rill_pack::PackBuilder;
use rill_store::Hash;

use crate::{InstallStore, Manifest, install_key};

static N: AtomicU32 = AtomicU32::new(0);

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rill-app-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sample_pack(marker: &str) -> Vec<u8> {
    let doc = rill_doc::compile(&format!("column {{ text \"{marker}\" }}")).unwrap();
    let mut b = PackBuilder::new();
    b.add("/app/index", doc.bytes).unwrap();
    b.add("/app/data", marker.as_bytes().to_vec()).unwrap();
    b.build().unwrap()
}

fn manifest_for(pack: &[u8], name: &str) -> String {
    format!(
        r##"
manifest_version = 1
app_id = "notes"
name = "{name}"
entry = "/app/index"
pack = "/apps/notes/app.rillpack"
pack_hash = "{}"

[window]
width = 900
height = 650
titlebar = "#26263a"

[permissions]
clipboard_write = true
"##,
        Hash::of(pack)
    )
}

#[test]
fn manifest_parsing_strict() {
    let pack = sample_pack("v1");
    let good = Manifest::parse(&manifest_for(&pack, "Notes")).unwrap();
    assert_eq!(good.app_id, "notes");
    assert_eq!(good.window.width, Some(900.0));
    assert_eq!(good.window.titlebar.unwrap().to_string(), "#26263a");
    assert_eq!(good.permissions.get("clipboard_write"), Some(&true));

    let base = manifest_for(&pack, "Notes");
    // Unknown key, bad id, bad entry, missing/newer version all reject.
    assert!(Manifest::parse(&format!("{base}\nsurprise = 1")).is_err());
    assert!(Manifest::parse(&base.replace("\"notes\"", "\"Has Space\"")).is_err());
    assert!(Manifest::parse(&base.replace("/app/index", "app/index")).is_err());
    assert!(Manifest::parse(&base.replace("manifest_version = 1", "manifest_version = 2"))
        .unwrap_err()
        .to_string()
        .contains("newer viewer"));
    assert!(Manifest::parse(&base.replace("manifest_version = 1", "")).is_err());
    assert!(Manifest::parse(&base.replace("width = 900", "width = 50")).is_err());
    // Unknown permission names reject — a silently-kept unknown grant reads
    // as security the app does not have.
    assert!(
        Manifest::parse(&base.replace("clipboard_write = true", "telepathy = true"))
            .unwrap_err()
            .to_string()
            .contains("unknown permission")
    );
    assert!(Manifest::parse(&base.replace("clipboard_write = true", "files = true")).is_ok());
}

#[test]
fn identity_binds_server_and_app() {
    let a = install_key("aaaa", "notes");
    let b = install_key("bbbb", "notes");
    assert_ne!(a, b, "same app id on different servers must differ");
    assert!(a.starts_with("notes-"));
    assert_eq!(a, install_key("aaaa", "notes"), "deterministic");
}

#[test]
fn install_read_remove_roundtrip() {
    let store = InstallStore::open(tmp()).unwrap();
    let pack = sample_pack("hello");
    let manifest = manifest_for(&pack, "Notes");

    let installed = store
        .install("home", 7331, "fp-aaaa", "/apps/notes/manifest", &manifest, &pack)
        .unwrap();
    assert_eq!(installed.name, "Notes");
    assert_eq!(store.list().unwrap().len(), 1);

    // Resources come from the pack, hash-verified.
    let data = store.read_resource(&installed.key, "/app/data").unwrap().unwrap();
    assert_eq!(data, b"hello");
    assert!(store.read_resource(&installed.key, "/app/missing").unwrap().is_none());

    // Manifest is pinned and re-readable.
    assert_eq!(store.manifest(&installed.key).unwrap().name, "Notes");

    assert!(store.remove(&installed.key).unwrap());
    assert!(store.list().unwrap().is_empty());
    assert!(!store.remove(&installed.key).unwrap());
}

#[test]
fn install_rejects_corruption() {
    let store = InstallStore::open(tmp()).unwrap();
    let pack = sample_pack("x");
    let manifest = manifest_for(&pack, "Notes");

    // Wrong bytes for the declared hash.
    let mut wrong = pack.clone();
    wrong[40] ^= 0xFF;
    let e = store
        .install("home", 7331, "fp", "/m", &manifest, &wrong)
        .unwrap_err();
    assert!(e.to_string().contains("hash mismatch"), "{e}");
    assert!(store.list().unwrap().is_empty(), "nothing recorded on failure");

    // Entry missing from pack.
    let mut b = PackBuilder::new();
    b.add("/other", vec![1]).unwrap();
    let no_entry = b.build().unwrap();
    let m2 = manifest_for(&no_entry, "Notes");
    let e = store.install("home", 7331, "fp", "/m", &m2, &no_entry).unwrap_err();
    assert!(e.to_string().contains("not present in pack"), "{e}");

    // A rejected install must leave nothing behind — no persisted pack, no
    // leftover temp file (write_verified_pack removes the temp on failure).
    let key = install_key("fp", &Manifest::parse(&m2).unwrap().app_id);
    let packs = store.app_dir(&key).join("packs");
    if let Ok(entries) = std::fs::read_dir(&packs) {
        let names: Vec<String> =
            entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.is_empty(), "rejected install left files behind: {names:?}");
    }
}

#[test]
fn staged_update_applies_on_promote() {
    let store = InstallStore::open(tmp()).unwrap();
    let v1 = sample_pack("v1");
    let installed = store
        .install("home", 7331, "fp", "/apps/notes/manifest", &manifest_for(&v1, "Notes"), &v1)
        .unwrap();

    // Nothing staged: promote is a no-op.
    assert!(!store.promote_staged(&installed.key).unwrap());

    // Stage v2; current stays v1 until promote.
    let v2 = sample_pack("v2");
    store
        .stage_update(&installed.key, &manifest_for(&v2, "Notes v2"), &v2)
        .unwrap();
    assert_eq!(
        store.read_resource(&installed.key, "/app/data").unwrap().unwrap(),
        b"v1"
    );

    // Promote: current becomes v2, name updates, previous retained.
    assert!(store.promote_staged(&installed.key).unwrap());
    let after = store.get(&installed.key).unwrap().unwrap();
    assert_eq!(after.name, "Notes v2");
    assert_eq!(after.current, Hash::of(&v2));
    assert_eq!(
        store.read_resource(&installed.key, "/app/data").unwrap().unwrap(),
        b"v2"
    );
    // Both packs on disk (rollback), no staged marker left.
    let packs_dir = store.root().join("apps").join(&installed.key).join("packs");
    assert_eq!(std::fs::read_dir(&packs_dir).unwrap().count(), 2);
    assert!(!store.promote_staged(&installed.key).unwrap());
}
