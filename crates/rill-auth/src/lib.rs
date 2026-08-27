//! Rill identity and authorization (`specs/security.md`).
//!
//! * device/server identities as self-signed certs pinned by SHA-256
//!   fingerprint (no CA; the [`DeviceAuth`] trait is the seam where a CA
//!   verifier could slot in later);
//! * `devices.toml` registry, `policy.toml` deny-by-default rules,
//!   `servers.toml` client-side pins;
//! * rustls config builders for both endpoints (TLS 1.3 only, ALPN `rill/1`).

mod certs;
mod pattern;
mod pins;
mod policy;
mod pending;
mod registry;
mod tls;

pub use certs::{PemIdentity, generate_identity, load_pem_identity, parse_cert_pem, parse_key_pem};
pub use pattern::Pattern;
pub use pending::{Pending, PendingDevices, unix_now};
pub use pins::Pins;
pub use policy::{Access, Policy};
pub use registry::DeviceRegistry;
pub use tls::{
    ALPN, client_tls_config, probe_tls_config, server_name, server_tls_config,
};

// Re-exports so dependents don't need their own rustls/tokio-rustls dep.
pub use tokio_rustls::client::TlsStream as ClientTlsStream;
pub use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
pub use tokio_rustls::{TlsAcceptor, TlsConnector};

use sha2::{Digest, Sha256};
use std::fmt;

/// Who is on the other end of a connection (security.md §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// No certificate, or a certificate not in the registry.
    Anonymous,
    /// An enrolled device, by registry name.
    Device(String),
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Identity::Anonymous => write!(f, "anonymous"),
            Identity::Device(name) => write!(f, "device {name}"),
        }
    }
}

/// Maps a presented client certificate to an identity. The registry
/// implements this; a CA-based verifier can replace it without touching the
/// server's dispatch path (security.md §10).
pub trait DeviceAuth: Send + Sync {
    fn identify(&self, cert_der: &[u8]) -> Identity;
}

/// Lowercase hex SHA-256 of a certificate's DER bytes.
///
/// Nibble lookup rather than `format!("{byte:02x}")` per byte: this runs on
/// the handshake path — once per client connection in `identify`, once per
/// server connection in the pinned verifier — and the formatting version
/// allocated a throwaway `String` for every one of the 32 bytes to produce
/// two characters each.
pub fn fingerprint_hex(cert_der: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(cert_der);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Errors from identity/policy loading and TLS configuration.
#[derive(Debug)]
pub struct AuthError(pub String);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AuthError {}

impl AuthError {
    pub(crate) fn new(message: impl Into<String>) -> AuthError {
        AuthError(message.into())
    }
}

/// True iff `s` is a well-formed lowercase-normalizable SHA-256 hex string.
pub(crate) fn normalize_fingerprint(s: &str) -> Result<String, AuthError> {
    let lower = s.to_ascii_lowercase();
    if lower.len() == 64 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(lower)
    } else {
        Err(AuthError::new(format!("not a SHA-256 hex fingerprint: {s:?}")))
    }
}

#[cfg(test)]
mod fingerprint_tests {
    use super::{fingerprint_hex, normalize_fingerprint};

    /// A *known-answer* test, deliberately — every other test of this
    /// function compares it against itself (enrol a fingerprint, look the
    /// same cert up again), which passes just as happily if the encoding is
    /// uniformly wrong. The NIST vector for "abc" is the useful one here
    /// because it contains the bytes 0x01 and 0x00: a hex encoder that drops
    /// a leading zero nibble produces a *shorter* string that still looks
    /// like a fingerprint, and would silently stop matching every enrolled
    /// device.
    #[test]
    fn matches_the_nist_vector_including_zero_padded_bytes() {
        assert_eq!(
            fingerprint_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            fingerprint_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Always 64 lowercase hex characters, so the registry's own
        // validator accepts whatever this produces.
        let fp = fingerprint_hex(b"abc");
        assert_eq!(fp.len(), 64);
        assert_eq!(normalize_fingerprint(&fp).unwrap(), fp);
    }
}
