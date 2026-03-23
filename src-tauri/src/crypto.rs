use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey, SharedSecret};
use zeroize::Zeroize;

use crate::error::MeshGuardError;

/// Size of AES-256-GCM nonce in bytes.
const NONCE_SIZE: usize = 12;

/// Holds an identity keypair for X25519 key exchange.
pub struct Identity {
    secret: EphemeralSecret,
    pub public_key: PublicKey,
}

impl Identity {
    /// Generate a new random X25519 identity.
    pub fn generate() -> Self {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public_key = PublicKey::from(&secret);
        Self { secret, public_key }
    }

    /// Perform X25519 Diffie-Hellman with a peer's public key and derive
    /// an AES-256 key using HKDF-SHA256.
    pub fn derive_shared_key(self, peer_public: &PublicKey) -> Result<SessionKey, MeshGuardError> {
        let shared_secret: SharedSecret = self.secret.diffie_hellman(peer_public);

        let hk = Hkdf::<Sha256>::new(Some(b"meshguard-v1"), shared_secret.as_bytes());
        let mut key_bytes = [0u8; 32];
        hk.expand(b"aes-256-gcm-key", &mut key_bytes)
            .map_err(|_| MeshGuardError::KeyDerivation)?;

        Ok(SessionKey { key: key_bytes })
    }
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

        // Prepend nonce to ciphertext
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
    fn roundtrip_encrypt_decrypt() {
        let alice = Identity::generate();
        let bob = Identity::generate();

        let alice_pub = alice.public_key;
        let bob_pub = bob.public_key;

        // Both sides derive the same shared key
        let alice_key = alice.derive_shared_key(&bob_pub).unwrap();
        let bob_key = bob.derive_shared_key(&alice_pub).unwrap();

        let message = b"Hello from MeshGuard!";
        let encrypted = alice_key.encrypt(message).unwrap();
        let decrypted = bob_key.decrypt(&encrypted).unwrap();

        assert_eq!(&decrypted, message);
    }
}
