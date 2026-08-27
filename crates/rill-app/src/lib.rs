//! Application model foundation (`specs/application-model.md`):
//!
//! * [`Manifest`] — strict TOML parsing/validation of app manifests;
//! * application identity = server certificate fingerprint + app_id;
//! * [`InstallStore`] — readable per-app directories + `installed.toml`
//!   index under the data dir, with verified installs, retained previous
//!   packs, and staged updates that apply on the next launch.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use rill_doc::Color;
use rill_pack::Pack;
use rill_protocol::validate_path;
use rill_store::Hash;

#[derive(Debug)]
pub struct AppError(pub String);

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> AppError {
        AppError(e.to_string())
    }
}

fn err(m: impl Into<String>) -> AppError {
    AppError(m.into())
}

pub const MANIFEST_VERSION: i64 = 1;

/// `RILL_DATA` env override, else `~/.local/share/rill`.
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RILL_DATA") {
        return dir.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".local").join("share").join("rill")
}

// ---------------------------------------------------------------- manifest

/// A parsed, validated application manifest (application-model.md §2).
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub app_id: String,
    pub name: String,
    /// Entry document path: inside the pack under `/app/**`, or a server
    /// path for an app whose pages are generated live.
    pub entry: String,
    /// Server path of the current pack.
    pub pack: String,
    /// Expected pack hash — integrity and version identity.
    pub pack_hash: Hash,
    pub window: WindowPrefs,
    /// Permission requests: parsed and displayed now, enforced in Phase 6.
    pub permissions: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowPrefs {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub titlebar: Option<Color>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, AppError> {
        let table: toml::Table =
            text.parse().map_err(|e| err(format!("manifest: {e}")))?;

        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "manifest_version" | "app_id" | "name" | "entry" | "pack" | "pack_hash"
                    | "window" | "permissions"
                    // Presentation hints: a glyph name for launchers and a
                    // freeform grouping. Optional, and deliberately *not*
                    // load-bearing — a launcher that cannot read them still
                    // launches the app; unknown keys elsewhere stay fatal.
                    | "icon" | "category"
            ) {
                return Err(err(format!("manifest: unknown key {key:?}")));
            }
        }
        match table.get("manifest_version").and_then(|v| v.as_integer()) {
            Some(MANIFEST_VERSION) => {}
            Some(newer) => {
                return Err(err(format!(
                    "manifest version {newer} — requires a newer viewer"
                )));
            }
            None => return Err(err("manifest: missing manifest_version")),
        }

        let string = |key: &str| -> Result<String, AppError> {
            table
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| err(format!("manifest: missing string {key:?}")))
        };
        let app_id = string("app_id")?;
        if app_id.is_empty()
            || app_id.len() > 32
            || !app_id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(err("manifest: app_id must be [a-z0-9-]{1,32}"));
        }
        let name = string("name")?;
        if name.is_empty() || name.len() > 64 {
            return Err(err("manifest: name must be 1–64 bytes"));
        }
        let entry = string("entry")?;
        validate_path(&entry).map_err(|e| err(format!("manifest: entry: {e}")))?;
        let pack = string("pack")?;
        validate_path(&pack).map_err(|e| err(format!("manifest: pack: {e}")))?;
        let pack_hash = Hash::from_hex(&string("pack_hash")?)
            .ok_or_else(|| err("manifest: pack_hash must be blake3:<64 hex>"))?;

        let mut window = WindowPrefs::default();
        if let Some(value) = table.get("window") {
            let section = value.as_table().ok_or_else(|| err("manifest: [window] must be a table"))?;
            for key in section.keys() {
                if !matches!(key.as_str(), "width" | "height" | "titlebar") {
                    return Err(err(format!("manifest: [window]: unknown key {key:?}")));
                }
            }
            let dim = |key: &str| -> Result<Option<f32>, AppError> {
                match section.get(key) {
                    None => Ok(None),
                    Some(v) => {
                        let n = v
                            .as_integer()
                            .map(|i| i as f64)
                            .or_else(|| v.as_float())
                            .filter(|n| n.is_finite() && *n >= 200.0 && *n <= 10_000.0)
                            .ok_or_else(|| {
                                err(format!("manifest: [window] {key} must be 200–10000"))
                            })?;
                        Ok(Some(n as f32))
                    }
                }
            };
            window.width = dim("width")?;
            window.height = dim("height")?;
            if let Some(v) = section.get("titlebar") {
                let s = v.as_str().ok_or_else(|| err("manifest: titlebar must be a color string"))?;
                window.titlebar = Some(
                    Color::parse_hex(s)
                        .ok_or_else(|| err("manifest: titlebar must be #rrggbb"))?,
                );
            }
        }

        let mut permissions = BTreeMap::new();
        if let Some(value) = table.get("permissions") {
            let section =
                value.as_table().ok_or_else(|| err("manifest: [permissions] must be a table"))?;
            for (key, v) in section {
                // Only names the platform defines (application-model.md §2):
                // a silently-accepted unknown name is a grant that does
                // nothing, which reads as security the app does not have.
                if !matches!(key.as_str(), "files" | "clipboard_write") {
                    return Err(err(format!("manifest: unknown permission {key:?}")));
                }
                let flag = v
                    .as_bool()
                    .ok_or_else(|| err(format!("manifest: permission {key:?} must be a bool")))?;
                permissions.insert(key.clone(), flag);
            }
        }

        Ok(Manifest { app_id, name, entry, pack, pack_hash, window, permissions })
    }
}

// ---------------------------------------------------------------- identity

/// Install key: `<app_id>-<blake3(fingerprint ":" app_id)[..8]>`. The server
/// fingerprint participates so the app ID alone is never trusted
/// (application-model.md §3).
pub fn install_key(server_fingerprint: &str, app_id: &str) -> String {
    let digest = Hash::of(format!("{server_fingerprint}:{app_id}").as_bytes());
    format!("{app_id}-{}", &digest.to_hex()[..8])
}

/// Write bytes to `path` atomically: write a sibling temp file, then rename it
/// over the target (rename is atomic on the same filesystem). A crash mid-write
/// leaves either the old file or the complete new one — never a truncated mix
/// that would corrupt `installed.toml` or a manifest.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Paths a pack owns. An entry outside this prefix is served, not packed.
const PACK_NAMESPACE: &str = "/app/";

/// Write a pack to its final `{hash}.rillpack` path **only after it verifies**.
/// Writes to a temp file, verifies that (and, if `entry` is given, that the
/// entry resource is present), then renames into place. On any failure the temp
/// is removed, so a rejected pack never lingers on disk.
fn write_verified_pack(
    pack_path: &Path,
    pack_bytes: &[u8],
    entry: Option<&str>,
) -> Result<(), AppError> {
    let tmp = pack_path.with_extension("tmp");
    std::fs::write(&tmp, pack_bytes)?;
    let checked = (|| -> Result<(), AppError> {
        Pack::open(&tmp)
            .and_then(|mut p| p.verify())
            .map_err(|e| err(format!("pack failed verification: {e}")))?;
        // Only a *packed* entry can be verified here. An app whose UI is
        // generated live — a file explorer, anything backed by a handler —
        // has its entry served, and the runtime already resolves pack-first,
        // server-second (`Source::App`). `/app/**` is the pack's own
        // namespace, so a typo there still fails at install rather than at
        // launch; a served entry is the server's to answer for.
        if let Some(entry) = entry.filter(|e| e.starts_with(PACK_NAMESPACE)) {
            let pack = Pack::open(&tmp).map_err(|e| err(e.to_string()))?;
            if pack.entry(entry).is_none() {
                return Err(err(format!("entry {entry:?} not present in pack")));
            }
        }
        Ok(())
    })();
    match checked {
        Ok(()) => {
            std::fs::rename(&tmp, pack_path)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ------------------------------------------------------------ install store

/// One installed app, as recorded in `installed.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub key: String,
    pub app_id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub server_fingerprint: String,
    /// Server path of the manifest (for update checks).
    pub manifest_path: String,
    pub current: Hash,
}

pub struct InstallStore {
    root: PathBuf,
}

impl InstallStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<InstallStore, AppError> {
        let root = root.into();
        std::fs::create_dir_all(root.join("apps"))?;
        Ok(InstallStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn app_dir(&self, key: &str) -> PathBuf {
        self.root.join("apps").join(key)
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("installed.toml")
    }

    pub fn list(&self) -> Result<Vec<Installed>, AppError> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let table: toml::Table = std::fs::read_to_string(&path)?
            .parse()
            .map_err(|e| err(format!("installed.toml: {e}")))?;
        let mut out = Vec::new();
        if let Some(apps) = table.get("apps").and_then(|v| v.as_table()) {
            for (key, value) in apps {
                let entry =
                    value.as_table().ok_or_else(|| err("installed.toml: malformed entry"))?;
                let get = |k: &str| -> Result<String, AppError> {
                    entry
                        .get(k)
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| err(format!("installed.toml: {key}: missing {k}")))
                };
                out.push(Installed {
                    key: key.clone(),
                    app_id: get("app_id")?,
                    name: get("name")?,
                    host: get("host")?,
                    port: entry
                        .get("port")
                        .and_then(|v| v.as_integer())
                        .and_then(|p| u16::try_from(p).ok())
                        .ok_or_else(|| err("installed.toml: bad port"))?,
                    server_fingerprint: get("server_fingerprint")?,
                    manifest_path: get("manifest_path")?,
                    current: Hash::from_hex(&get("current")?)
                        .ok_or_else(|| err("installed.toml: bad current hash"))?,
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn save_index(&self, apps: &[Installed]) -> Result<(), AppError> {
        let mut out = String::from("# installed Rill applications (managed by `rill app`)\n");
        for app in apps {
            out.push_str(&format!(
                "\n[apps.{:?}]\napp_id = {:?}\nname = {:?}\nhost = {:?}\nport = {}\nserver_fingerprint = {:?}\nmanifest_path = {:?}\ncurrent = {:?}\n",
                app.key,
                app.app_id,
                app.name,
                app.host,
                app.port,
                app.server_fingerprint,
                app.manifest_path,
                app.current.to_hex(),
            ));
        }
        write_atomic(&self.index_path(), out.as_bytes())?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<Installed>, AppError> {
        Ok(self.list()?.into_iter().find(|a| a.key == key))
    }

    /// Verify and record an install. `manifest_text` and `pack_bytes` are
    /// as fetched; nothing is recorded unless the pack bytes hash to the
    /// manifest's `pack_hash` AND pass full pack verification.
    #[allow(clippy::too_many_arguments)]
    pub fn install(
        &self,
        host: &str,
        port: u16,
        server_fingerprint: &str,
        manifest_path: &str,
        manifest_text: &str,
        pack_bytes: &[u8],
    ) -> Result<Installed, AppError> {
        let manifest = Manifest::parse(manifest_text)?;
        let actual = Hash::of(pack_bytes);
        if actual != manifest.pack_hash {
            return Err(err(format!(
                "pack hash mismatch: manifest says {}, bytes are {}",
                manifest.pack_hash, actual
            )));
        }

        let key = install_key(server_fingerprint, &manifest.app_id);
        let dir = self.app_dir(&key);
        std::fs::create_dir_all(dir.join("packs"))?;
        std::fs::create_dir_all(dir.join("state"))?;

        let pack_path = dir.join("packs").join(format!("{}.rillpack", actual.to_hex()));
        // Verify before persisting: a rejected pack leaves nothing behind.
        write_verified_pack(&pack_path, pack_bytes, Some(&manifest.entry))?;
        write_atomic(&dir.join("manifest.toml"), manifest_text.as_bytes())?;

        let installed = Installed {
            key: key.clone(),
            app_id: manifest.app_id.clone(),
            name: manifest.name.clone(),
            host: host.to_string(),
            port,
            server_fingerprint: server_fingerprint.to_string(),
            manifest_path: manifest_path.to_string(),
            current: actual,
        };
        let mut apps: Vec<Installed> =
            self.list()?.into_iter().filter(|a| a.key != key).collect();
        apps.push(installed.clone());
        apps.sort_by(|a, b| a.name.cmp(&b.name));
        self.save_index(&apps)?;
        Ok(installed)
    }

    pub fn remove(&self, key: &str) -> Result<bool, AppError> {
        let apps = self.list()?;
        let existed = apps.iter().any(|a| a.key == key);
        if existed {
            let remaining: Vec<Installed> =
                apps.into_iter().filter(|a| a.key != key).collect();
            self.save_index(&remaining)?;
            let dir = self.app_dir(key);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
        }
        Ok(existed)
    }

    /// The pinned manifest of an installed app.
    pub fn manifest(&self, key: &str) -> Result<Manifest, AppError> {
        Manifest::parse(&std::fs::read_to_string(self.app_dir(key).join("manifest.toml"))?)
    }

    /// Read one resource out of the app's current pack (hash-verified by the
    /// pack layer). This is how a running app loads everything.
    pub fn read_resource(&self, key: &str, path: &str) -> Result<Option<Vec<u8>>, AppError> {
        let installed = self.get(key)?.ok_or_else(|| err(format!("{key}: not installed")))?;
        let pack_path = self
            .app_dir(key)
            .join("packs")
            .join(format!("{}.rillpack", installed.current.to_hex()));
        let mut pack = Pack::open(&pack_path).map_err(|e| err(e.to_string()))?;
        pack.get(path).map_err(|e| err(e.to_string()))
    }

    /// Stage an update: verified pack + manifest written beside the current
    /// ones; applied by [`InstallStore::promote_staged`] at next launch.
    pub fn stage_update(
        &self,
        key: &str,
        manifest_text: &str,
        pack_bytes: &[u8],
    ) -> Result<(), AppError> {
        let manifest = Manifest::parse(manifest_text)?;
        let actual = Hash::of(pack_bytes);
        if actual != manifest.pack_hash {
            return Err(err("staged pack hash mismatch"));
        }
        let dir = self.app_dir(key);
        let pack_path = dir.join("packs").join(format!("{}.rillpack", actual.to_hex()));
        write_verified_pack(&pack_path, pack_bytes, None)?;
        write_atomic(&dir.join("manifest.staged.toml"), manifest_text.as_bytes())?;
        Ok(())
    }

    /// Apply a fully-downloaded staged update, keeping the previous pack for
    /// rollback and pruning older ones. Call at launch, before opening.
    pub fn promote_staged(&self, key: &str) -> Result<bool, AppError> {
        let dir = self.app_dir(key);
        let staged_path = dir.join("manifest.staged.toml");
        if !staged_path.exists() {
            return Ok(false);
        }
        let staged_text = std::fs::read_to_string(&staged_path)?;
        let staged = Manifest::parse(&staged_text)?;
        let pack_file =
            dir.join("packs").join(format!("{}.rillpack", staged.pack_hash.to_hex()));
        if !pack_file.exists() {
            return Ok(false); // download incomplete; keep current
        }

        let mut apps = self.list()?;
        let Some(entry) = apps.iter_mut().find(|a| a.key == key) else {
            return Ok(false);
        };
        let previous = entry.current;
        entry.current = staged.pack_hash;
        entry.name = staged.name.clone();
        // Each write is atomic. Update the pinned manifest and the index (the
        // source of truth for `current`) before clearing the staged marker, so
        // a crash never leaves the index pointing at a pruned pack.
        write_atomic(&dir.join("manifest.toml"), staged_text.as_bytes())?;
        self.save_index(&apps)?;
        std::fs::remove_file(&staged_path)?;

        // Prune packs beyond current + previous.
        let keep = [staged.pack_hash.to_hex(), previous.to_hex()];
        if let Ok(entries) = std::fs::read_dir(dir.join("packs")) {
            for f in entries.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if !keep.iter().any(|h| name == format!("{h}.rillpack")) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
