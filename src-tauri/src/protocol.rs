use meshguard_core::v1::V1Payload;
use uuid::Uuid;

use crate::crypto::SessionKey;
use crate::error::MeshGuardError;

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

    /// Compact v1 binary serialization for the LoRa wire.
    /// Only the type discriminant + payload — no id/timestamp. The encoding
    /// lives in `meshguard_core::v1` so every front end shares it.
    fn to_wire(&self) -> Vec<u8> {
        let payload = match self {
            Self::Text { text, .. } => V1Payload::Text(text.clone()),
            Self::PairRequest { sender_name, .. } => V1Payload::PairRequest(sender_name.clone()),
            Self::PairAccept { responder_name, .. } => {
                V1Payload::PairAccept(responder_name.clone())
            }
        };
        payload.to_wire()
    }

    /// Parse from compact v1 binary wire format.
    ///
    /// v1 carries no message id or timestamp, so a **new local UUID and the
    /// receive time** are assigned here. Two copies of the same packet
    /// therefore get different ids; stable ids arrive with the v2 envelope
    /// (`meshguard_core::envelope`).
    fn from_wire(data: &[u8]) -> Result<Self, MeshGuardError> {
        let payload =
            V1Payload::from_wire(data).map_err(|e| MeshGuardError::Protocol(e.to_string()))?;

        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();

        Ok(match payload {
            V1Payload::Text(text) => Self::Text {
                id,
                text,
                timestamp: now,
            },
            V1Payload::PairRequest(sender_name) => Self::PairRequest {
                id,
                sender_name,
                timestamp: now,
            },
            V1Payload::PairAccept(responder_name) => Self::PairAccept {
                id,
                responder_name,
                timestamp: now,
            },
        })
    }

    /// Encrypt the message for transmission: to_wire() → AES-256-GCM.
    pub fn encrypt_envelope(&self, session_key: &SessionKey) -> Result<Vec<u8>, MeshGuardError> {
        let wire = self.to_wire();
        Ok(session_key.encrypt(&wire)?)
    }

    /// Decrypt a received payload: AES-256-GCM → from_wire().
    pub fn decrypt_envelope(data: &[u8], session_key: &SessionKey) -> Result<Self, MeshGuardError> {
        let wire = session_key.decrypt(data)?;
        Self::from_wire(&wire)
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. } | Self::PairRequest { id, .. } | Self::PairAccept { id, .. } => {
                id
            }
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

    /// Frames from the shipping v1 send path must stay readable by the
    /// core dual-reader that v2-capable builds will use.
    #[test]
    fn v1_frames_decode_with_core_dual_reader() {
        use meshguard_core::codec::{decode_frame, DecodedFrame};
        use meshguard_core::v1::V1Payload;

        let alice = derive_p2p_key("Alice", "Bob", "secret").unwrap();
        let bob = derive_p2p_key("Bob", "Alice", "secret").unwrap();
        for (msg, expected) in [
            (
                MeshMessage::new_text("Zdravo 👋"),
                V1Payload::Text("Zdravo 👋".into()),
            ),
            (
                MeshMessage::new_pair_request("Alice-Radio"),
                V1Payload::PairRequest("Alice-Radio".into()),
            ),
            (
                MeshMessage::new_pair_accept("Bob-Radio"),
                V1Payload::PairAccept("Bob-Radio".into()),
            ),
        ] {
            let frame = msg.encrypt_envelope(&alice).unwrap();
            assert_eq!(
                decode_frame(&frame, Some(&bob), &[]),
                Ok(DecodedFrame::V1(expected))
            );
        }
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
