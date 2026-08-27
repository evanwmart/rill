//! `devices.toml`: the flat name → fingerprint device registry
//! (security.md §4). Implements [`DeviceAuth`] — the seam a CA verifier
//! would replace.

use std::collections::HashMap;

use crate::{AuthError, DeviceAuth, Identity, fingerprint_hex, normalize_fingerprint};

#[derive(Debug, Default)]
pub struct DeviceRegistry {
    /// fingerprint (lowercase hex) → device name
    by_fingerprint: HashMap<String, String>,
}

impl DeviceRegistry {
    pub fn parse(text: &str) -> Result<DeviceRegistry, AuthError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e| AuthError::new(format!("devices.toml: {e}")))?;
        let mut by_fingerprint = HashMap::new();
        for (name, value) in &table {
            if name == "anonymous" {
                return Err(AuthError::new(
                    "devices.toml: \"anonymous\" is reserved and cannot be a device name",
                ));
            }
            let fp = value.as_str().ok_or_else(|| {
                AuthError::new(format!("devices.toml: {name}: value must be a fingerprint string"))
            })?;
            let fp = normalize_fingerprint(fp)
                .map_err(|e| AuthError::new(format!("devices.toml: {name}: {e}")))?;
            if let Some(existing) = by_fingerprint.insert(fp, name.clone()) {
                return Err(AuthError::new(format!(
                    "devices.toml: {name} and {existing} share a fingerprint"
                )));
            }
        }
        Ok(DeviceRegistry { by_fingerprint })
    }

    pub fn len(&self) -> usize {
        self.by_fingerprint.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_fingerprint.is_empty()
    }
}

impl DeviceAuth for DeviceRegistry {
    fn identify(&self, cert_der: &[u8]) -> Identity {
        match self.by_fingerprint.get(&fingerprint_hex(cert_der)) {
            Some(name) => Identity::Device(name.clone()),
            None => Identity::Anonymous,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceRegistry;
    use crate::{DeviceAuth, Identity, fingerprint_hex};

    #[test]
    fn lookup_and_validation() {
        let cert = b"fake-der-bytes";
        let fp = fingerprint_hex(cert);
        let registry =
            DeviceRegistry::parse(&format!("desktop = \"{}\"", fp.to_uppercase())).unwrap();
        assert_eq!(registry.identify(cert), Identity::Device("desktop".into()));
        assert_eq!(registry.identify(b"other"), Identity::Anonymous);

        assert!(DeviceRegistry::parse("desktop = \"nothex\"").is_err());
        assert!(DeviceRegistry::parse(&format!("anonymous = \"{fp}\"")).is_err());
        assert!(
            DeviceRegistry::parse(&format!("a = \"{fp}\"\nb = \"{fp}\"")).is_err(),
            "duplicate fingerprints rejected"
        );
    }
}
