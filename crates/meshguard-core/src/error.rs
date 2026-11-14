use thiserror::Error;

/// Errors from the cryptographic primitives.
///
/// Messages are deliberately generic: they never contain key material,
/// plaintext or ciphertext.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("Key derivation failed")]
    KeyDerivation,

    #[error("Encryption failed: {0}")]
    Encryption(String),

    #[error("Decryption failed: {0}")]
    Decryption(String),

    #[error("Invalid message content: {0}")]
    InvalidContent(String),
}
