use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::SessionKey;
use crate::error::MeshGuardError;

/// MeshGuard protocol messages — sent encrypted over the Meshtastic mesh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MeshMessage {
    /// Encrypted text message.
    Text {
        id: String,
        ciphertext: Vec<u8>,
        timestamp: i64,
    },

    /// Delivery receipt.
    Receipt {
        id: String,
        message_id: String,
        status: DeliveryStatus,
        timestamp: i64,
    },

    /// Ping — check if peer is reachable.
    Ping {
        id: String,
        timestamp: i64,
    },

    /// Pong — response to ping.
    Pong {
        id: String,
        timestamp: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeliveryStatus {
    Delivered,
    Read,
    Failed,
}

/// A decrypted message ready for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub text: String,
    pub timestamp: i64,
    pub is_mine: bool,
    pub status: DeliveryStatus,
}

impl MeshMessage {
    /// Create an encrypted text message.
    pub fn new_text(plaintext: &str, session_key: &SessionKey) -> Result<Self, MeshGuardError> {
        let ciphertext = session_key.encrypt(plaintext.as_bytes())?;
        Ok(Self::Text {
            id: Uuid::new_v4().to_string(),
            ciphertext,
            timestamp: Utc::now().timestamp(),
        })
    }

    /// Decrypt a text message.
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

    pub fn to_bytes(&self) -> Result<Vec<u8>, MeshGuardError> {
        serde_json::to_vec(self).map_err(|e| MeshGuardError::Serialization(e.to_string()))
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, MeshGuardError> {
        serde_json::from_slice(data).map_err(|e| MeshGuardError::Protocol(e.to_string()))
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::Receipt { id, .. }
            | Self::Ping { id, .. }
            | Self::Pong { id, .. } => id,
        }
    }
}
