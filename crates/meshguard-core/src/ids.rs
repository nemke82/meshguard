//! Identifiers shared by the engine, storage and transports.

use std::fmt;

use rand::RngCore;

/// Size of a [`MessageId`] in bytes.
pub const MESSAGE_ID_LEN: usize = 16;

/// Stable end-to-end message identifier.
///
/// Generated once by the sender from a CSPRNG and carried inside the
/// encrypted v2 envelope. Retries — including retries over a different
/// transport — reuse the same id so the receiver can deduplicate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId([u8; MESSAGE_ID_LEN]);

impl MessageId {
    /// A fresh random id from the OS CSPRNG.
    pub fn random() -> Self {
        let mut bytes = [0u8; MESSAGE_ID_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub const fn from_bytes(bytes: [u8; MESSAGE_ID_LEN]) -> Self {
        Self(bytes)
    }

    /// Parse from a slice; `None` unless it is exactly 16 bytes.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; MESSAGE_ID_LEN]>::try_from(bytes).ok().map(Self)
    }

    pub const fn as_bytes(&self) -> &[u8; MESSAGE_ID_LEN] {
        &self.0
    }

    /// Lower-case hex, suitable for UI and storage keys.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MessageId({})", self.to_hex())
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Stable local identifier of a conversation (one per peer for now).
///
/// Must not be derived from a mutable radio/BLE name; see ADR-003.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConversationId(pub String);

/// Opaque identity of a peer as used for deduplication and AEAD context.
///
/// The final, stable cryptographic identity is pending an identity ADR.
/// Until then callers use a transport-scoped value, e.g.
/// [`PeerIdentity::meshtastic_node`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerIdentity(Vec<u8>);

impl PeerIdentity {
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Interim identity: the Meshtastic node number (big-endian, tagged).
    pub fn meshtastic_node(node_num: u32) -> Self {
        let mut v = Vec::with_capacity(5);
        v.push(b'M');
        v.extend_from_slice(&node_num.to_be_bytes());
        Self(v)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Identifies which transport handled an attempt (for diagnostics).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransportId(pub String);

impl TransportId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_differ() {
        assert_ne!(MessageId::random(), MessageId::random());
    }

    #[test]
    fn from_slice_requires_exact_length() {
        assert!(MessageId::from_slice(&[0u8; 15]).is_none());
        assert!(MessageId::from_slice(&[0u8; 17]).is_none());
        assert!(MessageId::from_slice(&[7u8; 16]).is_some());
    }

    #[test]
    fn hex_is_lowercase_and_stable() {
        let id = MessageId::from_bytes([0xab; 16]);
        assert_eq!(id.to_hex(), "ab".repeat(16));
        assert_eq!(id.to_string(), id.to_hex());
    }

    #[test]
    fn meshtastic_identity_is_tagged() {
        let p = PeerIdentity::meshtastic_node(0x0102_0304);
        assert_eq!(p.as_bytes(), &[b'M', 1, 2, 3, 4]);
    }
}
