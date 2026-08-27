//! `pending.toml`: fingerprints of devices that offered a certificate the
//! server does not know (security.md §2, "unknown device").
//!
//! The enrollment workflow used to be *read the server's stderr and copy a
//! fingerprint out of a log line*. That works exactly when someone is
//! watching a terminal: not when the log is off, not when stderr goes to a
//! file nobody tails, and not when the server is an appliance service. The
//! fingerprint is a fact the server learned, so it belongs in a file it
//! keeps rather than in prose it printed — and `rill auth pending` reads it
//! back without parsing anything.
//!
//! Deliberately *not* a security decision: recording that a stranger
//! knocked grants nothing. Approval stays a human copying a name into
//! `devices.toml` via `rill auth enroll`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{AuthError, normalize_fingerprint};

/// One unknown device, as last seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Lowercase hex SHA-256 of the offered certificate.
    pub fingerprint: String,
    /// Unix seconds, first and most recent sighting.
    pub first_seen: u64,
    pub last_seen: u64,
    /// How many times this fingerprint has been recorded.
    pub count: u64,
}

/// The unknown-device list, keyed by fingerprint.
#[derive(Debug, Default)]
pub struct PendingDevices {
    map: BTreeMap<String, Pending>,
}

impl PendingDevices {
    /// Anyone who can reach the port can add an entry, so this file is
    /// attacker-influenced by construction: a fresh self-signed certificate
    /// per connection would grow it without bound. Capped, evicting the
    /// least recently seen — an operator enrolling a device does it within
    /// minutes of plugging it in, and a list longer than this is noise
    /// rather than a workflow.
    pub const MAX_ENTRIES: usize = 32;

    /// How stale a sighting must be before it is written again. A client
    /// that retries every second must not mean a file write every second.
    pub const REFRESH_AFTER_SECS: u64 = 60;

    pub fn parse(text: &str) -> Result<PendingDevices, AuthError> {
        let table: toml::Table =
            text.parse().map_err(|e| AuthError::new(format!("pending.toml: {e}")))?;
        let mut map = BTreeMap::new();
        // Absent or malformed *entries* are dropped rather than fatal: this
        // file is a convenience the server rewrites, and refusing to start
        // over a bad one would turn a hint into an outage.
        for (fp, value) in &table {
            let Ok(fingerprint) = normalize_fingerprint(fp) else { continue };
            let Some(entry) = value.as_table() else { continue };
            let num = |k: &str| entry.get(k).and_then(|v| v.as_integer()).unwrap_or(0).max(0) as u64;
            let first_seen = num("first_seen");
            map.insert(fingerprint.clone(), Pending {
                fingerprint,
                first_seen,
                last_seen: num("last_seen").max(first_seen),
                count: num("count").max(1),
            });
        }
        Ok(PendingDevices { map })
    }

    pub fn path(dir: &Path) -> PathBuf {
        dir.join("pending.toml")
    }

    pub fn load(dir: &Path) -> Result<PendingDevices, AuthError> {
        let path = PendingDevices::path(dir);
        if !path.exists() {
            return Ok(PendingDevices::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| AuthError::new(format!("{}: {e}", path.display())))?;
        PendingDevices::parse(&text)
    }

    /// Most recently seen first — the device someone just plugged in is the
    /// one they are about to enroll.
    pub fn list(&self) -> Vec<&Pending> {
        let mut all: Vec<&Pending> = self.map.values().collect();
        all.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(a.fingerprint.cmp(&b.fingerprint)));
        all
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Record a sighting. Returns whether anything changed — a caller that
    /// gets `false` has nothing to save, which is how a retrying client
    /// avoids costing a file write per attempt.
    pub fn record(&mut self, fingerprint: &str, now: u64) -> bool {
        let Ok(fp) = normalize_fingerprint(fingerprint) else { return false };
        if let Some(seen) = self.map.get_mut(&fp) {
            seen.count += 1;
            if now.saturating_sub(seen.last_seen) < PendingDevices::REFRESH_AFTER_SECS {
                return false;
            }
            seen.last_seen = now;
            return true;
        }
        if self.map.len() >= PendingDevices::MAX_ENTRIES
            && let Some(oldest) =
                self.map.values().min_by_key(|p| p.last_seen).map(|p| p.fingerprint.clone())
        {
            self.map.remove(&oldest);
        }
        self.map.insert(fp.clone(), Pending {
            fingerprint: fp,
            first_seen: now,
            last_seen: now,
            count: 1,
        });
        true
    }

    /// Forget a fingerprint — what enrolling one should do to it.
    pub fn remove(&mut self, fingerprint: &str) -> bool {
        match normalize_fingerprint(fingerprint) {
            Ok(fp) => self.map.remove(&fp).is_some(),
            Err(_) => false,
        }
    }

    /// Write via temp + rename: a crash mid-write must leave the old list
    /// or the new one, never a half-parsed mix.
    pub fn save(&self, dir: &Path) -> Result<(), AuthError> {
        let mut out = String::from(
            "# Devices that offered an unknown certificate (security.md §2).\n\
             # Written by the server; approve one with:\n\
             #   rill auth enroll <server-dir> <device-name> <fingerprint>\n",
        );
        for p in self.map.values() {
            out.push_str(&format!(
                "\n[{:?}]\nfirst_seen = {}\nlast_seen = {}\ncount = {}\n",
                p.fingerprint, p.first_seen, p.last_seen, p.count
            ));
        }
        let path = PendingDevices::path(dir);
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, out).map_err(|e| AuthError::new(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| AuthError::new(format!("{}: {e}", path.display())))
    }
}

/// Unix seconds now, saturating at the epoch on a clock before 1970.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    /// A sighting is recorded once, then goes quiet: a client retrying every
    /// second must not mean a file write every second.
    #[test]
    fn a_repeat_sighting_is_counted_but_not_rewritten() {
        let mut p = PendingDevices::default();
        assert!(p.record(&fp('a'), 1_000), "first sighting is news");
        assert!(!p.record(&fp('a'), 1_001), "a second later is not");
        assert!(p.record(&fp('a'), 1_000 + PendingDevices::REFRESH_AFTER_SECS), "a minute later is");
        let seen = p.list();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].count, 3, "every sighting counts even when not written");
        assert_eq!(seen[0].first_seen, 1_000, "and the first is remembered");
    }

    /// Anyone who can reach the port can add an entry, so the list is
    /// bounded and drops the least recently seen.
    #[test]
    fn the_list_is_bounded_against_a_stranger_with_many_certificates() {
        let mut p = PendingDevices::default();
        for i in 0..(PendingDevices::MAX_ENTRIES + 10) {
            let mut f = format!("{i:04x}");
            f.push_str(&"0".repeat(60));
            p.record(&f, 1_000 + i as u64);
        }
        assert_eq!(p.list().len(), PendingDevices::MAX_ENTRIES, "capped");
        assert_eq!(p.list()[0].last_seen, 1_000 + (PendingDevices::MAX_ENTRIES + 9) as u64,
            "newest first, and it survived");
    }

    /// The file round-trips, and enrolling a device forgets it.
    #[test]
    fn round_trips_and_forgets_on_enrollment() {
        let mut p = PendingDevices::default();
        p.record(&fp('a'), 1_000);
        p.record(&fp('b'), 2_000);

        let dir = std::env::temp_dir().join(format!("rill-pending-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        p.save(&dir).unwrap();
        let back = PendingDevices::load(&dir).unwrap();
        assert_eq!(back.list().len(), 2);
        assert_eq!(back.list()[0].fingerprint, fp('b'), "most recent first");
        assert_eq!(back.list()[0].first_seen, 2_000);

        let mut back = back;
        assert!(back.remove(&fp('b')));
        assert!(!back.remove(&fp('b')), "removing twice is a shrug");
        assert_eq!(back.list().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt entry is dropped, not fatal: this file is a convenience,
    /// and refusing to start over it would turn a hint into an outage.
    #[test]
    fn a_bad_entry_is_dropped_rather_than_fatal() {
        let text = format!(
            "[{:?}]\nfirst_seen = 5\nlast_seen = 9\ncount = 2\n\
             [\"not-a-fingerprint\"]\nfirst_seen = 1\n",
            fp('c')
        );
        let p = PendingDevices::parse(&text).expect("parses");
        assert_eq!(p.list().len(), 1);
        assert_eq!(p.list()[0].count, 2);
    }
}
