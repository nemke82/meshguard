use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::error::MeshGuardError;

const NONCE_SIZE: usize = 12;

/// Derive a deterministic AES-256 session key from the P2P pairing info.
///
/// Both peers compute the same key because they both know:
///   - Their own device name
///   - The peer's device name
///   - A shared passphrase they agreed on out-of-band
///
/// The inputs are sorted before hashing so that order doesn't matter —
/// both sides arrive at the identical key.
pub fn derive_p2p_key(
    my_device_name: &str,
    peer_device_name: &str,
    shared_passphrase: &str,
) -> Result<SessionKey, MeshGuardError> {
    // Sort the two device names so both sides get the same order
    let my_name = my_device_name.trim();
    let peer_name = peer_device_name.trim();

    let (first, second) = if my_name <= peer_name {
        (my_name, peer_name)
    } else {
        (peer_name, my_name)
    };

    // Build the input keying material: SHA-256(first || second || passphrase)
    let mut hasher = Sha256::new();
    hasher.update(first.as_bytes());
    hasher.update(b"|");
    hasher.update(second.as_bytes());
    hasher.update(b"|");
    hasher.update(shared_passphrase.as_bytes());
    let ikm = hasher.finalize();

    // HKDF to derive the actual AES-256 key
    let hk = Hkdf::<Sha256>::new(Some(b"meshguard-p2p-v1"), &ikm);
    let mut key_bytes = [0u8; 32];
    hk.expand(b"aes-256-gcm-p2p-key", &mut key_bytes)
        .map_err(|_| MeshGuardError::KeyDerivation)?;

    Ok(SessionKey { key: key_bytes })
}

/// Derive a Meshtastic channel PSK (32 bytes) from the pairing info.
/// This is set on the Meshtastic device so only paired devices can decode
/// the LoRa frames. Our AES-256-GCM layer encrypts on top of this.
pub fn derive_channel_psk(
    my_device_name: &str,
    peer_device_name: &str,
    shared_passphrase: &str,
) -> Result<[u8; 32], MeshGuardError> {
    let my_name = my_device_name.trim();
    let peer_name = peer_device_name.trim();

    let (first, second) = if my_name <= peer_name {
        (my_name, peer_name)
    } else {
        (peer_name, my_name)
    };

    let mut hasher = Sha256::new();
    hasher.update(b"meshguard-channel-psk|");
    hasher.update(first.as_bytes());
    hasher.update(b"|");
    hasher.update(second.as_bytes());
    hasher.update(b"|");
    hasher.update(shared_passphrase.as_bytes());
    let result = hasher.finalize();

    let mut psk = [0u8; 32];
    psk.copy_from_slice(&result);
    Ok(psk)
}

/// A derived AES-256 session key. Zeroized on drop.
#[derive(Zeroize)]
#[zeroize(drop)]
pub struct SessionKey {
    key: [u8; 32],
}

impl SessionKey {
    /// Encrypt plaintext with AES-256-GCM. Returns nonce || ciphertext.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, MeshGuardError> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| MeshGuardError::Encryption("invalid key".into()))?;

        let mut nonce_bytes = [0u8; NONCE_SIZE];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| MeshGuardError::Encryption(e.to_string()))?;

        let mut output = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&ciphertext);
        Ok(output)
    }

    /// Decrypt a message produced by `encrypt`. Expects nonce || ciphertext.
    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, MeshGuardError> {
        if data.len() < NONCE_SIZE {
            return Err(MeshGuardError::Decryption("data too short".into()));
        }

        let (nonce_bytes, ciphertext) = data.split_at(NONCE_SIZE);
        let nonce = Nonce::from_slice(nonce_bytes);

        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| MeshGuardError::Decryption("invalid key".into()))?;

        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| MeshGuardError::Decryption(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p2p_key_derivation_is_symmetric() {
        // Alice's side
        let alice_key = derive_p2p_key(
            "Alice-P1000",
            "Bob-P1000",
            "our-secret-phrase",
        ).unwrap();

        // Bob's side — same inputs but swapped my/peer
        let bob_key = derive_p2p_key(
            "Bob-P1000",
            "Alice-P1000",
            "our-secret-phrase",
        ).unwrap();

        // Both must derive identical keys
        let msg = b"Hello from MeshGuard P2P!";
        let encrypted = alice_key.encrypt(msg).unwrap();
        let decrypted = bob_key.decrypt(&encrypted).unwrap();
        assert_eq!(&decrypted, msg);
    }

    #[test]
    fn channel_psk_is_symmetric() {
        let psk_a = derive_channel_psk(
            "Alice-P1000",
            "Bob-P1000",
            "our-secret",
        ).unwrap();

        let psk_b = derive_channel_psk(
            "Bob-P1000",
            "Alice-P1000",
            "our-secret",
        ).unwrap();

        assert_eq!(psk_a, psk_b);
    }

    #[test]
    fn different_passphrase_gives_different_key() {
        let key_a = derive_p2p_key("A", "B", "pass1").unwrap();
        let key_b = derive_p2p_key("A", "B", "pass2").unwrap();

        let msg = b"test";
        let encrypted = key_a.encrypt(msg).unwrap();
        assert!(key_b.decrypt(&encrypted).is_err());
    }
}
