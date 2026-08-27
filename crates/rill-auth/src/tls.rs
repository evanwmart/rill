//! rustls configuration for both endpoints (security.md §3).
//!
//! TLS 1.3 only, ALPN `rill/1`. Trust is fingerprint pinning, not WebPKI:
//! the server accepts any *well-formed* client certificate at the TLS layer
//! (identity is decided afterwards by the registry — possession of the key
//! is what the handshake proves); the client accepts exactly one pinned
//! server certificate.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::{AuthError, fingerprint_hex, normalize_fingerprint};

/// The ALPN protocol identifier — protocol version negotiation
/// (protocol.md §2).
pub const ALPN: &[u8] = b"rill/1";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Owned ServerName for connecting (handles DNS names and IP literals).
pub fn server_name(host: &str) -> Result<ServerName<'static>, AuthError> {
    ServerName::try_from(host.to_string())
        .map_err(|e| AuthError::new(format!("invalid server name {host:?}: {e}")))
}

/// Server side: present `cert`, request (but don't require) a client
/// certificate.
pub fn server_tls_config(
    key: PrivateKeyDer<'static>,
    cert: CertificateDer<'static>,
) -> Result<Arc<rustls::ServerConfig>, AuthError> {
    let provider = provider();
    let verifier: Arc<dyn ClientCertVerifier> =
        Arc::new(AnyClientCert { provider: provider.clone() });
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| AuthError::new(format!("TLS config: {e}")))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert], key)
        .map_err(|e| AuthError::new(format!("server certificate: {e}")))?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

/// Client side: verify the server by pinned fingerprint; present a device
/// certificate if one is given.
pub fn client_tls_config(
    pinned_fingerprint: &str,
    device: Option<(PrivateKeyDer<'static>, CertificateDer<'static>)>,
) -> Result<Arc<rustls::ClientConfig>, AuthError> {
    let pinned = normalize_fingerprint(pinned_fingerprint)?;
    let provider = provider();
    let verifier = Arc::new(PinnedServerCert { pinned: Some(pinned), provider: provider.clone() });
    build_client_config(provider, verifier, device)
}

/// Client config that accepts any server certificate — ONLY for
/// `rill auth trust`, which shows the fingerprint to the user before pinning.
pub fn probe_tls_config() -> Result<Arc<rustls::ClientConfig>, AuthError> {
    let provider = provider();
    let verifier = Arc::new(PinnedServerCert { pinned: None, provider: provider.clone() });
    build_client_config(provider, verifier, None)
}

fn build_client_config(
    provider: Arc<CryptoProvider>,
    verifier: Arc<PinnedServerCert>,
    device: Option<(PrivateKeyDer<'static>, CertificateDer<'static>)>,
) -> Result<Arc<rustls::ClientConfig>, AuthError> {
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| AuthError::new(format!("TLS config: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let mut config = match device {
        Some((key, cert)) => builder
            .with_client_auth_cert(vec![cert], key)
            .map_err(|e| AuthError::new(format!("device certificate: {e}")))?,
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

/// Accepts any well-formed client certificate; the handshake proves key
/// possession, and the registry decides identity afterwards.
#[derive(Debug)]
struct AnyClientCert {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false // absence → Anonymous (security.md §2)
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Exact-fingerprint server verification (`pinned: Some`), or accept-and-
/// report for the trust probe (`pinned: None`).
#[derive(Debug)]
struct PinnedServerCert {
    pinned: Option<String>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match &self.pinned {
            None => Ok(ServerCertVerified::assertion()),
            Some(pinned) if fingerprint_hex(end_entity) == *pinned => {
                Ok(ServerCertVerified::assertion())
            }
            Some(_) => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
