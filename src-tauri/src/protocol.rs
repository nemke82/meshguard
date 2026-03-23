use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::SessionKey;
use crate::error::MeshGuardError;

/// Message types in the MeshGuard protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MeshMessage {
    /// Initial key exchange — sends our X25519 public key.
    KeyExchange {
        id: String,
        sender_id: String,
        public_key: Vec<u8>,
        timestamp: i64,
    },

    /// Key exchange acknowledgment from peer.
    KeyExchangeAck {
        id: String,
        sender_id: String,
        public_key: Vec<u8>,
        timestamp: i64,
    },

    /// An encrypted text message.
    EncryptedText {
        id: String,
        sender_id: String,
        ciphertext: Vec<u8>,
        timestamp: i64,
    },

    /// Delivery receipt.
    Receipt {
        id: String,
        sender_id: String,
        message_id: String,
        status: DeliveryStatus,
        timestamp: i64,
    },

    /// Ping to check if peer is alive.
    Ping {
        id: String,
        sender_id: String,
        timestamp: i64,
    },

    /// Pong response.
    Pong {
        id: String,
        sender_id: String,
        timestamp: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeliveryStatus {
    Delivered,
    Read,
    Failed,
}

/// A decrypted, displayable message for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub sender_id: String,
    pub text: String,
    pub timestamp: i64,
    pub is_mine: bool,
    pub status: DeliveryStatus,
}

impl MeshMessage {
    /// Create a new key exchange initiation message.
    pub fn new_key_exchange(sender_id: &str, public_key: &[u8]) -> Self {
        Self::KeyExchange {
            id: Uuid::new_v4().to_string(),
            sender_id: sender_id.to_string(),
            public_key: public_key.to_vec(),
            timestamp: Utc::now().timestamp(),
        }
    }

    /// Create an encrypted text message.
    pub fn new_encrypted_text(
        sender_id: &str,
        plaintext: &str,
        session_key: &SessionKey,
    ) -> Result<Self, MeshGuardError> {
        let ciphertext = session_key.encrypt(plaintext.as_bytes())?;
        Ok(Self::EncryptedText {
            id: Uuid::new_v4().to_string(),
            sender_id: sender_id.to_string(),
            ciphertext,
            timestamp: Utc::now().timestamp(),
        })
    }

    /// Serialize the message to bytes for transmission.
    pub fn to_bytes(&self) -> Result<Vec<u8>, MeshGuardError> {
        serde_json::to_vec(self).map_err(|e| MeshGuardError::Serialization(e.to_string()))
    }

    /// Deserialize a message from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, MeshGuardError> {
        serde_json::from_slice(data).map_err(|e| MeshGuardError::Protocol(e.to_string()))
    }

    /// Get the message ID.
    pub fn id(&self) -> &str {
        match self {
            Self::KeyExchange { id, .. }
            | Self::KeyExchangeAck { id, .. }
            | Self::EncryptedText { id, .. }
            | Self::Receipt { id, .. }
            | Self::Ping { id, .. }
            | Self::Pong { id, .. } => id,
        }
    }
}
