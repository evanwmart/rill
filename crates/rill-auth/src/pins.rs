//! `servers.toml`: client-side pinned server fingerprints (security.md §4).

use std::collections::BTreeMap;
use std::path::Path;

use crate::{AuthError, normalize_fingerprint};

#[derive(Debug, Default)]
pub struct Pins {
    /// "host:port" → fingerprint (lowercase hex). BTreeMap keeps the file
    /// deterministically ordered on save.
    map: BTreeMap<String, String>,
}

impl Pins {
    pub fn parse(text: &str) -> Result<Pins, AuthError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e| AuthError::new(format!("servers.toml: {e}")))?;
        let mut map = BTreeMap::new();
        for (key, value) in &table {
            let fp = value.as_str().ok_or_else(|| {
                AuthError::new(format!("servers.toml: {key}: value must be a fingerprint string"))
            })?;
            map.insert(key.clone(), normalize_fingerprint(fp)?);
        }
        Ok(Pins { map })
    }

    pub fn load(dir: &Path) -> Result<Pins, AuthError> {
        let path = dir.join("servers.toml");
        if !path.exists() {
            return Ok(Pins::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| AuthError::new(format!("{}: {e}", path.display())))?;
        Pins::parse(&text)
    }

    pub fn get(&self, host: &str, port: u16) -> Option<&str> {
        self.map.get(&format!("{host}:{port}")).map(String::as_str)
    }

    pub fn set(&mut self, host: &str, port: u16, fingerprint: &str) -> Result<(), AuthError> {
        self.map.insert(format!("{host}:{port}"), normalize_fingerprint(fingerprint)?);
        Ok(())
    }

    pub fn save(&self, dir: &Path) -> Result<(), AuthError> {
        let mut out = String::from("# pinned server certificate fingerprints (sha-256 hex)\n");
        for (key, fp) in &self.map {
            out.push_str(&format!("{key:?} = {fp:?}\n"));
        }
        let path = dir.join("servers.toml");
        std::fs::write(&path, out)
            .map_err(|e| AuthError::new(format!("{}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::Pins;

    #[test]
    fn parse_get_set() {
        let fp = "a".repeat(64);
        let mut pins = Pins::parse(&format!("\"h.example:7331\" = \"{fp}\"")).unwrap();
        assert_eq!(pins.get("h.example", 7331), Some(fp.as_str()));
        assert_eq!(pins.get("h.example", 1), None);
        pins.set("other", 7331, &"B".repeat(64)).unwrap();
        assert_eq!(pins.get("other", 7331), Some("b".repeat(64).as_str()));
        assert!(Pins::parse("\"h:1\" = \"short\"").is_err());
    }
}
