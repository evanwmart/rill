//! Encryption at rest (specs/history.md decision 2): segments sealed to the
//! device's existing identity key, zero new UX, keyslots earning the table
//! the header has carried since v1.
//!
//! The shape:
//!
//! * Each encrypted segment gets its own random **data key**. Chunks and the
//!   seal's index blobs are XChaCha20-Poly1305 under it, nonce prepended.
//! * The data key is **wrapped** into a header keyslot under the device
//!   **KEK**, which is derived (`blake3::derive_key`) from the device's
//!   identity private key file. Per-segment data keys are what make the
//!   keyslot table worth having: an owner-passphrase slot later wraps the
//!   *same* data key beside the device slot, and no segment re-encrypts.
//! * The chunk header's 4-byte hash stays a hash **of the plaintext**, as it
//!   always was — it is the crash-honesty check and the merkle leaf, and it
//!   survives the encryption exactly as the format comment promised it
//!   would. Four truncated bytes of blake3 leak nothing anyone can use.
//!
//! Honest limits, stated rather than glossed (they are the decision's own):
//! whoever controls the unlocked device reads its history — the KEK derives
//! from a file on the same disk. What this buys is media theft, backup
//! leaks, and ciphertext-only replication later. And the KEK follows the
//! identity key: rotate `device-key.pem` and old segments need the old file
//! to unlock — rotation is not built anywhere yet, and when it is, it must
//! re-wrap keyslots (that is what they are for).

use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// Keyslot kind: wrapped under the device KEK.
pub const KEYSLOT_DEVICE: u8 = 1;

const NONCE_LEN: usize = 24;

fn random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("OS randomness");
}

/// The key-encryption key: what unwraps a segment's data key.
#[derive(Clone)]
pub struct Kek([u8; 32]);

impl Kek {
    /// Derive from the device identity's private key file
    /// (`<dir>/device-key.pem`). `None` when the device has no identity —
    /// an unenrolled machine records plaintext and says so, rather than
    /// refusing to record at all.
    pub fn from_identity_dir(dir: &Path) -> Option<Kek> {
        let pem = std::fs::read(dir.join("device-key.pem")).ok()?;
        Some(Kek(blake3::derive_key("rill history segment kek v1", &pem)))
    }

    /// For tests: a KEK from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Kek {
        Kek(bytes)
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }

    /// Wrap a data key into a keyslot blob: `nonce || aead(data_key)`.
    pub fn wrap(&self, key: &DataKey) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        random(&mut nonce);
        let ct = self
            .cipher()
            .encrypt(XNonce::from_slice(&nonce), key.0.as_slice())
            .expect("aead encrypt is infallible for sane lengths");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    /// Unwrap a keyslot blob. `None` is a wrong key or a mangled slot — the
    /// caller reports the segment locked either way, because AEAD does not
    /// distinguish and pretending to would be a lie.
    pub fn unwrap(&self, blob: &[u8]) -> Option<DataKey> {
        if blob.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let pt = self.cipher().decrypt(XNonce::from_slice(nonce), ct).ok()?;
        let bytes: [u8; 32] = pt.try_into().ok()?;
        Some(DataKey(bytes))
    }
}

/// One segment's own key. Random per segment, wrapped into keyslots.
#[derive(Clone)]
pub struct DataKey([u8; 32]);

impl DataKey {
    pub fn generate() -> DataKey {
        let mut k = [0u8; 32];
        random(&mut k);
        DataKey(k)
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }

    /// Encrypt one blob (a compressed chunk, an index): `nonce || ct`.
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        random(&mut nonce);
        let ct = self
            .cipher()
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .expect("aead encrypt is infallible for sane lengths");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    /// Decrypt `nonce || ct`. `None` on tamper or truncation.
    pub fn open(&self, blob: &[u8]) -> Option<Vec<u8>> {
        if blob.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        self.cipher().decrypt(XNonce::from_slice(nonce), ct).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_and_unwrap_round_trip() {
        let kek = Kek::from_bytes([7; 32]);
        let key = DataKey::generate();
        let slot = kek.wrap(&key);
        let back = kek.unwrap(&slot).expect("right key unwraps");
        assert_eq!(key.0, back.0);
        assert!(Kek::from_bytes([8; 32]).unwrap(&slot).is_none(), "wrong key is a locked slot");
    }

    #[test]
    fn sealed_blobs_open_only_untampered() {
        let key = DataKey::generate();
        let blob = key.seal(b"the transcript");
        assert_eq!(key.open(&blob).as_deref(), Some(b"the transcript".as_slice()));
        let mut bent = blob.clone();
        let at = bent.len() - 1;
        bent[at] ^= 1;
        assert!(key.open(&bent).is_none(), "a flipped byte must not decrypt");
    }

    /// Two devices' KEKs differ, and one device's is stable — the property
    /// that makes "zero new UX" true.
    #[test]
    fn the_kek_follows_the_identity_file() {
        let dir = std::env::temp_dir().join(format!("rill-kek-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("device-key.pem"), b"-----FAKE KEY A-----").unwrap();
        let a1 = Kek::from_identity_dir(&dir).unwrap();
        let a2 = Kek::from_identity_dir(&dir).unwrap();
        assert_eq!(a1.0, a2.0, "same file, same KEK");
        std::fs::write(dir.join("device-key.pem"), b"-----FAKE KEY B-----").unwrap();
        assert_ne!(Kek::from_identity_dir(&dir).unwrap().0, a1.0, "different key, different KEK");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
