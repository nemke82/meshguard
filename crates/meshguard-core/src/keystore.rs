//! Sealing session keys for storage on the device (ADR-002).
//!
//! Each stored session key is encrypted with the local *vault key* using
//! AES-256-GCM. The associated data binds the blob to the local radio and the
//! peer, so a blob copied to another entry does not open. Protecting the
//! vault key itself is platform work (Android Keystore) and lives outside
//! this crate.

use crate::crypto::SessionKey;
use crate::error::CoreError;

const AAD_LABEL: &[u8] = b"meshguard-keystore-v1";

fn aad(my_node: u32, peer_node: u32) -> Vec<u8> {
    let mut a = Vec::with_capacity(AAD_LABEL.len() + 8);
    a.extend_from_slice(AAD_LABEL);
    a.extend_from_slice(&my_node.to_be_bytes());
    a.extend_from_slice(&peer_node.to_be_bytes());
    a
}

/// Encrypt `key` (our chat key with `peer_node`, used from radio `my_node`).
pub fn seal_session_key(
    vault: &SessionKey,
    my_node: u32,
    peer_node: u32,
    key: &SessionKey,
) -> Result<Vec<u8>, CoreError> {
    let bytes = key.to_bytes();
    vault.encrypt_with_aad(bytes.as_slice(), &aad(my_node, peer_node))
}

/// Decrypt a blob produced by [`seal_session_key`] for the same nodes.
pub fn open_session_key(
    vault: &SessionKey,
    my_node: u32,
    peer_node: u32,
    blob: &[u8],
) -> Result<SessionKey, CoreError> {
    let plain = zeroize::Zeroizing::new(vault.decrypt_with_aad(blob, &aad(my_node, peer_node))?);
    let bytes: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| CoreError::Decryption("stored key has the wrong length".into()))?;
    Ok(SessionKey::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_p2p_key;

    #[test]
    fn roundtrip_restores_a_working_key() {
        let vault = SessionKey::random();
        let key = derive_p2p_key("A", "B", "pw").unwrap();
        let blob = seal_session_key(&vault, 1, 2, &key).unwrap();
        let restored = open_session_key(&vault, 1, 2, &blob).unwrap();
        let ct = key.encrypt(b"hello").unwrap();
        assert_eq!(restored.decrypt(&ct).unwrap(), b"hello");
    }

    #[test]
    fn blob_is_bound_to_both_nodes_and_the_vault() {
        let vault = SessionKey::random();
        let key = derive_p2p_key("A", "B", "pw").unwrap();
        let blob = seal_session_key(&vault, 1, 2, &key).unwrap();
        assert!(open_session_key(&vault, 1, 3, &blob).is_err(), "other peer");
        assert!(
            open_session_key(&vault, 9, 2, &blob).is_err(),
            "other radio"
        );
        assert!(
            open_session_key(&SessionKey::random(), 1, 2, &blob).is_err(),
            "other vault"
        );
        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(
            open_session_key(&vault, 1, 2, &tampered).is_err(),
            "tampered"
        );
    }

    #[test]
    fn blob_does_not_contain_the_key() {
        let vault = SessionKey::random();
        let key = derive_p2p_key("A", "B", "pw").unwrap();
        let raw = key.to_bytes();
        let blob = seal_session_key(&vault, 1, 2, &key).unwrap();
        assert!(!blob.windows(32).any(|w| w == raw.as_slice()));
    }
}
