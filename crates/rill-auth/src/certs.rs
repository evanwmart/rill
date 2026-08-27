//! Certificate generation (rcgen) and PEM loading (security.md §4–§5).

use std::path::Path;

// PEM parsing comes from rustls-pki-types, which rustls already re-exports —
// so this is a dependency removed, not swapped. `rustls-pemfile` was archived
// in August 2025 (RUSTSEC-2025-0134) and its final release was already a thin
// wrapper over exactly this code.
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::AuthError;

/// A key + certificate pair in PEM form, as stored on disk.
#[derive(Debug, Clone)]
pub struct PemIdentity {
    pub key_pem: String,
    pub cert_pem: String,
}

/// Generate a self-signed identity (ECDSA P-256). Long-lived by rcgen
/// default; expiry is not enforced in the pinned-fingerprint model
/// (security.md §3).
pub fn generate_identity(common_name: &str) -> Result<PemIdentity, AuthError> {
    let key_pair = rcgen::KeyPair::generate()
        .map_err(|e| AuthError::new(format!("key generation: {e}")))?;
    let params = rcgen::CertificateParams::new(vec![common_name.to_string()])
        .map_err(|e| AuthError::new(format!("certificate params: {e}")))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| AuthError::new(format!("certificate generation: {e}")))?;
    Ok(PemIdentity { key_pem: key_pair.serialize_pem(), cert_pem: cert.pem() })
}

/// The first certificate in a PEM blob. A chain is not expected here — an
/// identity file holds one self-signed certificate — so taking the first and
/// ignoring any remainder is the same behaviour as before.
pub fn parse_cert_pem(pem: &str) -> Result<CertificateDer<'static>, AuthError> {
    CertificateDer::from_pem_slice(pem.as_bytes())
        .map_err(|e| AuthError::new(format!("invalid certificate PEM: {e}")))
}

pub fn parse_key_pem(pem: &str) -> Result<PrivateKeyDer<'static>, AuthError> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes())
        .map_err(|e| AuthError::new(format!("invalid key PEM: {e}")))
}

/// Load `<prefix>-key.pem` + `<prefix>-cert.pem` from an identity directory.
/// `Ok(None)` when the files simply aren't there (e.g. anonymous client).
pub fn load_pem_identity(dir: &Path, prefix: &str) -> Result<Option<PemIdentity>, AuthError> {
    let key_path = dir.join(format!("{prefix}-key.pem"));
    let cert_path = dir.join(format!("{prefix}-cert.pem"));
    if !key_path.exists() && !cert_path.exists() {
        return Ok(None);
    }
    let read = |p: &Path| {
        std::fs::read_to_string(p).map_err(|e| AuthError::new(format!("{}: {e}", p.display())))
    };
    Ok(Some(PemIdentity { key_pem: read(&key_path)?, cert_pem: read(&cert_path)? }))
}

#[cfg(test)]
mod tests {
    use super::{generate_identity, parse_cert_pem, parse_key_pem};
    use crate::fingerprint_hex;

    #[test]
    fn generate_and_parse_roundtrip() {
        let id = generate_identity("test-device").unwrap();
        let cert = parse_cert_pem(&id.cert_pem).unwrap();
        parse_key_pem(&id.key_pem).unwrap();
        assert_eq!(fingerprint_hex(&cert).len(), 64);
        // Distinct identities get distinct fingerprints.
        let other = generate_identity("test-device").unwrap();
        let other_cert = parse_cert_pem(&other.cert_pem).unwrap();
        assert_ne!(fingerprint_hex(&cert), fingerprint_hex(&other_cert));
    }
}
