//! Per-field encryption (ADR-005 §2).

use hkdf::Hkdf;
use meshguard_core::crypto::SessionKey;
use meshguard_core::ids::{ConversationId, MessageId};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{Direction, Result, StoreError};

const SALT: &[u8] = b"meshguard-store-v1";
const INFO: &[u8] = b"store key";
const AAD_LABEL: &[u8] = b"mgs1";

pub(crate) struct StoreCipher {
    key: SessionKey,
}

impl StoreCipher {
    /// Store key = HKDF-SHA256(salt, vault key, info).
    pub(crate) fn new(vault: &SessionKey) -> Self {
        let ikm = vault.to_bytes();
        let mut okm = Zeroizing::new([0u8; 32]);
        Hkdf::<Sha256>::new(Some(SALT), ikm.as_slice())
            .expand(INFO, okm.as_mut_slice())
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self {
            key: SessionKey::from_bytes(*okm),
        }
    }

    pub(crate) fn seal(&self, plain: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        self.key
            .encrypt_with_aad(plain, aad)
            .map_err(|_| StoreError::Encrypt)
    }

    pub(crate) fn open(&self, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        self.key
            .decrypt_with_aad(blob, aad)
            .map_err(|_| StoreError::Decrypt)
    }
}

/// `label ‖ len-prefixed parts`, so no two (table, column, key) tuples
/// produce the same AAD.
fn aad(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = AAD_LABEL.to_vec();
    for p in parts {
        out.extend_from_slice(&(p.len() as u32).to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

pub(crate) fn title_aad(conv: &ConversationId) -> Vec<u8> {
    aad(&[b"conversations", b"title", conv.0.as_bytes()])
}

pub(crate) fn body_aad(conv: &ConversationId, id: &MessageId, dir: Direction) -> Vec<u8> {
    let d: &[u8] = match dir {
        Direction::In => b"in",
        Direction::Out => b"out",
    };
    aad(&[b"messages", b"body", conv.0.as_bytes(), id.as_bytes(), d])
}

pub(crate) fn key_check_aad() -> Vec<u8> {
    aad(&[b"meta", b"key_check"])
}

/// Plaintext of the key check value.
pub(crate) const KEY_CHECK: &[u8] = b"meshguard-store";
