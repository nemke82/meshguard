use thiserror::Error;

#[derive(Debug, Error)]
pub enum MeshGuardError {
    #[error("BLE error: {0}")]
    Ble(String),

    #[error("Device not found: {0}")]
    DeviceNotFound(String),

    #[error("Not connected to local device")]
    NotConnected,

    #[error("No peer configured")]
    NoPeer,

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

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Session not established — complete P2P pairing first")]
    NoSession,

    #[error("Bluetooth is turned off. Please enable Bluetooth in your device settings and try again.")]
    BluetoothDisabled,

    #[error("Bluetooth permissions not granted. Please allow Bluetooth access in your device settings.")]
    BluetoothPermission,

    #[error("IO error: {0}")]
    Io(String),

    #[error("Mesh radio error: {0}")]
    MeshRadio(String),

    #[error("Node not found on mesh: {0}")]
    NodeNotFound(u32),

    #[error("Passphrase mismatch — decryption failed")]
    PassphraseMismatch,
}

impl serde::Serialize for MeshGuardError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
