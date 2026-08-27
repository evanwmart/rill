//! Saved rices: a whole desktop look, kept as a copy of `theme.toml`.
//!
//! Everything that makes a rice already lives in one file, so a preset needs
//! no format of its own — a rice *is* a theme.toml, filed under a name. That
//! is what makes loading one a file copy and nothing else: every live-watch
//! path in the compositor and in rill-vector already re-reads on mtime, so
//! the desktop changes the moment the bytes land.
//!
//! ```text
//! ~/.config/rill/theme.toml          the live desktop
//! ~/.config/rill/rices/midnight.toml a saved one
//! ```
//!
//! Shared between the studio (which saves, loads and deletes by name) and the
//! compositor (which cycles). One implementation rather than two, because a
//! rule about where files live is exactly the kind that drifts when mirrored.

use std::io;
use std::path::{Path, PathBuf};

/// Longest accepted rice name.
const MAX_NAME: usize = 32;

/// Where saved rices live, beside the theme they are copies of.
pub fn dir(config_dir: &Path) -> PathBuf {
    config_dir.join("rices")
}

/// A rice name reduced to what may become a filename, or `None` if nothing
/// usable is left.
///
/// Names arrive from a text field, so this is a security boundary as much as
/// a tidiness one: `../../.ssh/authorized_keys` must not be a rice. Lowercase
/// ASCII, digits, `-` and `_` only — a `.` or a `/` cannot survive, so the
/// result can never escape the rices directory or name a hidden file.
pub fn sanitize(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            ' ' => '-',
            _ => '\0',
        })
        .filter(|c| *c != '\0')
        .take(MAX_NAME)
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// The file a named rice lives in. `None` for a name that sanitizes to
/// nothing.
pub fn path(config_dir: &Path, name: &str) -> Option<PathBuf> {
    Some(dir(config_dir).join(format!("{}.toml", sanitize(name)?)))
}

/// Saved rice names, sorted. Sorted rather than in directory order because
/// this doubles as the cycle order, and a cycle whose order depends on the
/// filesystem is a cycle nobody can predict.
pub fn list(config_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir(config_dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        // Only names this module would itself produce. Everything downstream
        // addresses a rice by its sanitized name — `path` sanitizes before
        // looking the file up — so a hand-dropped `My Rice.toml` could be
        // listed but never loaded. Filtering here makes the list mean "rices
        // you can use", and keeps names that never passed `sanitize` from
        // reaching pages that interpolate them.
        .filter(|stem| sanitize(stem).as_ref() == Some(stem))
        .collect();
    names.sort();
    names
}

/// Copy the live theme into `rices/<name>.toml`, overwriting any rice of
/// that name.
pub fn save(config_dir: &Path, theme: &Path, name: &str) -> io::Result<String> {
    let name = sanitize(name).ok_or_else(|| bad("a rice needs a name"))?;
    let target = dir(config_dir).join(format!("{name}.toml"));
    std::fs::create_dir_all(dir(config_dir))?;
    let text = std::fs::read_to_string(theme)?;
    write_atomic(&target, &text)?;
    remember_last(config_dir, &name);
    Ok(name)
}

/// Copy a saved rice over the live theme.
///
/// This *is* the whole of "apply": the compositor polls `theme.toml`'s mtime
/// and every vector client watches it too, so the desktop re-skins itself as
/// soon as the file changes.
pub fn load(config_dir: &Path, theme: &Path, name: &str) -> io::Result<()> {
    let source = path(config_dir, name).ok_or_else(|| bad("no such rice"))?;
    let text = std::fs::read_to_string(&source)?;
    if let Some(parent) = theme.parent() {
        std::fs::create_dir_all(parent)?;
    }
    remember_last(config_dir, name);
    write_atomic(theme, &text)
}

/// Forget a saved rice. Deleting one that is not there is not an error —
/// the desired state is "gone" either way.
pub fn delete(config_dir: &Path, name: &str) -> io::Result<()> {
    let Some(target) = path(config_dir, name) else {
        return Ok(());
    };
    match std::fs::remove_file(&target) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Which saved rice the live theme currently *is*, by content.
///
/// Compared by bytes rather than tracked in a marker file, so it stays true
/// across a hand-edited theme, a rice deleted behind the studio's back, or a
/// desktop restarted. The moment someone edits the live theme it stops
/// matching any rice, which is correct: they are no longer *on* one.
/// The rice the desktop is *based on*: the last one loaded or saved, held
/// in a marker beside the rices. This is what makes "modified" sayable —
/// [`current`] only knows exact equality, and the moment a knob turns, the
/// question worth answering becomes "diverged from what?".
pub fn last(config_dir: &Path) -> Option<String> {
    let name = std::fs::read_to_string(dir(config_dir).join(".last")).ok()?;
    let name = name.trim();
    // The marker is data, not authority: it names a rice only while that
    // rice still exists.
    path(config_dir, name).is_some().then(|| name.to_string())
}

fn remember_last(config_dir: &Path, name: &str) {
    // Best-effort on purpose: the marker is a convenience, and a read-only
    // config dir must not fail a load that already succeeded.
    let _ = std::fs::create_dir_all(dir(config_dir));
    let _ = std::fs::write(dir(config_dir).join(".last"), name);
}

pub fn current(config_dir: &Path, theme: &Path) -> Option<String> {
    let live = std::fs::read_to_string(theme).ok()?;
    list(config_dir).into_iter().find(|name| {
        path(config_dir, name)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|saved| saved == live)
    })
}

/// The next rice to cycle to, wrapping. `None` when none are saved.
///
/// A theme matching no saved rice (hand-edited, or never saved) cycles to the
/// first — so the shortcut always does something, rather than appearing
/// broken exactly when someone has been experimenting.
pub fn next(config_dir: &Path, theme: &Path) -> Option<String> {
    let names = list(config_dir);
    if names.is_empty() {
        return None;
    }
    let at = current(config_dir, theme).and_then(|c| names.iter().position(|n| *n == c));
    Some(match at {
        Some(i) => names[(i + 1) % names.len()].clone(),
        None => names[0].clone(),
    })
}

/// Write beside the target and rename over it. The compositor polls these
/// files; it must never read a half-written one and flash a default desktop.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rill-rices-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A rice name becomes a filename, so it is a security boundary: nothing
    /// that could escape the directory or name a hidden file may survive.
    #[test]
    fn a_name_cannot_escape_the_rices_directory() {
        assert_eq!(sanitize("Midnight Blue").as_deref(), Some("midnight-blue"));
        assert_eq!(sanitize("  spaced  ").as_deref(), Some("spaced"));
        assert_eq!(sanitize("keep_underscores").as_deref(), Some("keep_underscores"));
        // The dangerous shapes all collapse to something harmless.
        assert_eq!(sanitize("../../etc/passwd").as_deref(), Some("etcpasswd"));
        assert_eq!(sanitize("/absolute").as_deref(), Some("absolute"));
        assert_eq!(sanitize(".hidden").as_deref(), Some("hidden"));
        // Nothing usable left is a refusal, not an empty filename.
        assert_eq!(sanitize("../.."), None);
        assert_eq!(sanitize("   "), None);
        assert_eq!(sanitize(""), None);
        // And a name can never be long enough to matter.
        assert_eq!(sanitize(&"a".repeat(200)).unwrap().len(), MAX_NAME);

        // The derived path really does stay put.
        let config = Path::new("/tmp/cfg");
        let p = path(config, "../../escape").unwrap();
        assert_eq!(p.parent().unwrap(), dir(config), "every rice lands in rices/");
    }

    /// Save, load, cycle — the whole feature, which is a file copy each way.
    #[test]
    fn saving_loading_and_cycling_a_rice() {
        let config = scratch("cycle");
        let theme = config.join("theme.toml");

        std::fs::write(&theme, "# midnight\npage = \"#000010\"\n").unwrap();
        assert_eq!(save(&config, &theme, "Midnight").unwrap(), "midnight");
        std::fs::write(&theme, "# noon\npage = \"#f0f0e0\"\n").unwrap();
        save(&config, &theme, "noon").unwrap();

        assert_eq!(list(&config), vec!["midnight".to_string(), "noon".to_string()]);
        // The live theme *is* noon right now, by content.
        assert_eq!(current(&config, &theme).as_deref(), Some("noon"));

        // Cycling wraps, in sorted order, and actually changes the desktop.
        let step = next(&config, &theme).unwrap();
        assert_eq!(step, "midnight", "noon wraps round to the first");
        load(&config, &theme, &step).unwrap();
        assert!(std::fs::read_to_string(&theme).unwrap().contains("#000010"));
        assert_eq!(current(&config, &theme).as_deref(), Some("midnight"));
        assert_eq!(next(&config, &theme).as_deref(), Some("noon"));

        // A hand-edited theme belongs to no rice, and cycling still works.
        std::fs::write(&theme, "# mine\n").unwrap();
        assert_eq!(current(&config, &theme), None, "an edited theme is not a saved rice");
        assert_eq!(next(&config, &theme).as_deref(), Some("midnight"), "falls to the first");

        // Deleting is idempotent, and the list shrinks.
        delete(&config, "midnight").unwrap();
        delete(&config, "midnight").unwrap();
        assert_eq!(list(&config), vec!["noon".to_string()]);

        std::fs::remove_dir_all(&config).ok();
    }

    /// With nothing saved, cycling does nothing rather than failing.
    #[test]
    fn cycling_with_no_rices_is_a_no_op() {
        let config = scratch("empty");
        let theme = config.join("theme.toml");
        std::fs::write(&theme, "x = 1\n").unwrap();
        assert!(list(&config).is_empty());
        assert_eq!(next(&config, &theme), None);
        assert_eq!(current(&config, &theme), None);
        std::fs::remove_dir_all(&config).ok();
    }

    /// Loading leaves no `.tmp` litter beside the theme it wrote.
    #[test]
    fn writes_land_atomically_and_leave_nothing_behind() {
        let config = scratch("atomic");
        let theme = config.join("theme.toml");
        std::fs::write(&theme, "a = 1\n").unwrap();
        save(&config, &theme, "one").unwrap();
        std::fs::write(&theme, "b = 2\n").unwrap();
        load(&config, &theme, "one").unwrap();

        assert_eq!(std::fs::read_to_string(&theme).unwrap(), "a = 1\n");
        for d in [config.clone(), dir(&config)] {
            let strays: Vec<_> = std::fs::read_dir(&d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains("tmp"))
                .collect();
            assert!(strays.is_empty(), "left temporaries in {d:?}: {strays:?}");
        }
        std::fs::remove_dir_all(&config).ok();
    }
}
