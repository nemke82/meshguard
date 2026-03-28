use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::SessionKey;
use crate::error::MeshGuardError;

/// MeshGuard protocol messages — sent encrypted over the Meshtastic mesh
/// on PortNum::PrivateApp (256).
///
/// The outer transport is a Meshtastic MeshPacket; the payload bytes are
/// our AES-256-GCM encrypted JSON of this enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MeshMessage {
    /// Encrypted text message.
    Text {
        id: String,
        ciphertext: Vec<u8>,
        timestamp: i64,
    },

    /// Pairing request — sent when initiating a chat with a new peer.
    /// Encrypted with the derived key so the receiver can verify the
    /// passphrase by attempting to decrypt.
    PairRequest {
        id: String,
        sender_name: String,
        timestamp: i64,
    },

    /// Pairing accepted — sent back to confirm the passphrase matched.
    PairAccept {
        id: String,
        responder_name: String,
        timestamp: i64,
    },

    /// Delivery receipt.
    Receipt {
        id: String,
        message_id: String,
        status: DeliveryStatus,
        timestamp: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeliveryStatus {
    Delivered,
    Read,
    Failed,
}

impl MeshMessage {
    pub fn new_text(plaintext: &str, session_key: &SessionKey) -> Result<Self, MeshGuardError> {
        let ciphertext = session_key.encrypt(plaintext.as_bytes())?;
        Ok(Self::Text {
            id: Uuid::new_v4().to_string(),
            ciphertext,
            timestamp: chrono::Utc::now().timestamp(),
        })
    }

    pub fn new_pair_request(sender_name: &str) -> Self {
        Self::PairRequest {
            id: Uuid::new_v4().to_string(),
            sender_name: sender_name.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
        }
    }

    pub fn new_pair_accept(responder_name: &str) -> Self {
        Self::PairAccept {
            id: Uuid::new_v4().to_string(),
            responder_name: responder_name.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
        }
    }

    pub fn decrypt_text(&self, session_key: &SessionKey) -> Result<String, MeshGuardError> {
        match self {
            Self::Text { ciphertext, .. } => {
                let plaintext = session_key.decrypt(ciphertext)?;
                String::from_utf8(plaintext)
                    .map_err(|e| MeshGuardError::Decryption(e.to_string()))
            }
            _ => Err(MeshGuardError::Protocol("not a text message".into())),
        }
    }

    /// Encrypt the entire message envelope with a session key.
    /// Returns nonce || ciphertext of the JSON serialization.
    pub fn encrypt_envelope(&self, session_key: &SessionKey) -> Result<Vec<u8>, MeshGuardError> {
        let json = serde_json::to_vec(self)
            .map_err(|e| MeshGuardError::Serialization(e.to_string()))?;
        session_key.encrypt(&json)
    }

    /// Decrypt an envelope produced by `encrypt_envelope`.
    pub fn decrypt_envelope(
        data: &[u8],
        session_key: &SessionKey,
    ) -> Result<Self, MeshGuardError> {
        let json = session_key.decrypt(data)?;
        serde_json::from_slice(&json).map_err(|e| MeshGuardError::Protocol(e.to_string()))
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::PairRequest { id, .. }
            | Self::PairAccept { id, .. }
            | Self::Receipt { id, .. } => id,
        }
    }
}
