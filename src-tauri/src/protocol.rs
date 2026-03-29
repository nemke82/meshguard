use uuid::Uuid;

use crate::crypto::SessionKey;
use crate::error::MeshGuardError;

const MSG_TYPE_TEXT: u8 = 0x01;
const MSG_TYPE_PAIR_REQUEST: u8 = 0x02;
const MSG_TYPE_PAIR_ACCEPT: u8 = 0x03;

/// MeshGuard protocol messages — sent encrypted over the Meshtastic mesh
/// on PortNum::PrivateApp (256).
///
/// Wire format is a compact binary encoding to stay within the ~200-byte
/// Meshtastic LoRa payload limit:
///   [1 byte type][payload bytes]
///
/// The entire wire blob is AES-256-GCM encrypted (single layer).
#[derive(Debug, Clone)]
pub enum MeshMessage {
    Text {
        id: String,
        text: String,
        timestamp: i64,
    },
    PairRequest {
        id: String,
        sender_name: String,
        timestamp: i64,
    },
    PairAccept {
        id: String,
        responder_name: String,
        timestamp: i64,
    },
}

impl MeshMessage {
    pub fn new_text(text: &str) -> Self {
        Self::Text {
            id: Uuid::new_v4().to_string(),
            text: text.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
        }
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

    /// Compact binary serialization for the LoRa wire.
    /// Only the type discriminant + payload — no id/timestamp
    /// (those are local metadata, not needed over the air).
    fn to_wire(&self) -> Vec<u8> {
        match self {
            Self::Text { text, .. } => {
                let mut buf = vec![MSG_TYPE_TEXT];
                buf.extend_from_slice(text.as_bytes());
                buf
            }
            Self::PairRequest { sender_name, .. } => {
                let mut buf = vec![MSG_TYPE_PAIR_REQUEST];
                buf.extend_from_slice(sender_name.as_bytes());
                buf
            }
            Self::PairAccept { responder_name, .. } => {
                let mut buf = vec![MSG_TYPE_PAIR_ACCEPT];
                buf.extend_from_slice(responder_name.as_bytes());
                buf
            }
        }
    }

    /// Parse from compact binary wire format.
    fn from_wire(data: &[u8]) -> Result<Self, MeshGuardError> {
        if data.is_empty() {
            return Err(MeshGuardError::Protocol("empty wire message".into()));
        }

        let payload = &data[1..];
        let payload_str = std::str::from_utf8(payload)
            .map_err(|e| MeshGuardError::Protocol(format!("invalid UTF-8: {e}")))?;

        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();

        match data[0] {
            MSG_TYPE_TEXT => Ok(Self::Text {
                id,
                text: payload_str.to_string(),
                timestamp: now,
            }),
            MSG_TYPE_PAIR_REQUEST => Ok(Self::PairRequest {
                id,
                sender_name: payload_str.to_string(),
                timestamp: now,
            }),
            MSG_TYPE_PAIR_ACCEPT => Ok(Self::PairAccept {
                id,
                responder_name: payload_str.to_string(),
                timestamp: now,
            }),
            t => Err(MeshGuardError::Protocol(format!(
                "unknown message type: 0x{t:02x}"
            ))),
        }
    }

    /// Encrypt the message for transmission: to_wire() → AES-256-GCM.
    pub fn encrypt_envelope(&self, session_key: &SessionKey) -> Result<Vec<u8>, MeshGuardError> {
        let wire = self.to_wire();
        session_key.encrypt(&wire)
    }

    /// Decrypt a received payload: AES-256-GCM → from_wire().
    pub fn decrypt_envelope(
        data: &[u8],
        session_key: &SessionKey,
    ) -> Result<Self, MeshGuardError> {
        let wire = session_key.decrypt(data)?;
        Self::from_wire(&wire)
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::PairRequest { id, .. }
            | Self::PairAccept { id, .. } => id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_p2p_key;

    #[test]
    fn text_roundtrip() {
        let key = derive_p2p_key("Alice", "Bob", "secret").unwrap();
        let msg = MeshMessage::new_text("Hello world!");
        let encrypted = msg.encrypt_envelope(&key).unwrap();

        // Verify the encrypted payload is small enough for Meshtastic
        assert!(encrypted.len() < 200, "encrypted len = {}", encrypted.len());

        let decrypted = MeshMessage::decrypt_envelope(&encrypted, &key).unwrap();
        match decrypted {
            MeshMessage::Text { text, .. } => assert_eq!(text, "Hello world!"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn pair_request_roundtrip() {
        let key = derive_p2p_key("Alice", "Bob", "secret").unwrap();
        let msg = MeshMessage::new_pair_request("Alice-Radio");
        let encrypted = msg.encrypt_envelope(&key).unwrap();
        assert!(encrypted.len() < 200);

        let decrypted = MeshMessage::decrypt_envelope(&encrypted, &key).unwrap();
        match decrypted {
            MeshMessage::PairRequest { sender_name, .. } => {
                assert_eq!(sender_name, "Alice-Radio");
            }
            _ => panic!("expected PairRequest"),
        }
    }

    #[test]
    fn max_length_message_fits() {
        let key = derive_p2p_key("Alice", "Bob", "secret").unwrap();
        let long_text = "A".repeat(160);
        let msg = MeshMessage::new_text(&long_text);
        let encrypted = msg.encrypt_envelope(&key).unwrap();
        // 1 (type) + 160 (text) = 161 plaintext → 12 (nonce) + 161 + 16 (tag) = 189 bytes
        assert!(
            encrypted.len() <= 228,
            "160-char message = {} bytes, must fit in 228",
            encrypted.len()
        );
    }

    #[test]
    fn wrong_key_fails() {
        let key_a = derive_p2p_key("Alice", "Bob", "secret1").unwrap();
        let key_b = derive_p2p_key("Alice", "Bob", "secret2").unwrap();
        let msg = MeshMessage::new_text("test");
        let encrypted = msg.encrypt_envelope(&key_a).unwrap();
        assert!(MeshMessage::decrypt_envelope(&encrypted, &key_b).is_err());
    }
}
