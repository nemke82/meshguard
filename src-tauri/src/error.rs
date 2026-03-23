use thiserror::Error;

#[derive(Debug, Error)]
pub enum MeshGuardError {
    #[error("BLE error: {0}")]
    Ble(String),

    #[error("Device not found: {0}")]
    DeviceNotFound(String),

    #[error("Not connected to any device")]
    NotConnected,

    #[error("Key derivation failed")]
    KeyDerivation,

    #[error("Encryption failed: {0}")]
    Encryption(String),

    #[error("Decryption failed: {0}")]
    Decryption(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Peer rejected connection")]
    PeerRejected,

    #[error("Session expired")]
    SessionExpired,
}

// Allow Tauri to serialize our errors to the frontend.
impl serde::Serialize for MeshGuardError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
