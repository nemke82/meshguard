use serde::{Deserialize, Serialize};

/// A paired peer — someone we've established a MeshGuard session with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub id: String,
    pub device_name: String,
    pub node_num: u32,
}

/// Persisted app configuration — saved to disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// BLE address of the device we last connected to (for auto-reconnect).
    #[serde(default)]
    pub last_ble_address: Option<String>,

    /// Name of our last connected device (for display and key derivation).
    #[serde(default)]
    pub last_device_name: Option<String>,

    /// Paired peers — each is a separate P2P conversation.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

impl AppConfig {
    pub fn load(config_dir: &std::path::Path) -> Self {
        let path = config_dir.join("meshguard.json");
        match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, config_dir: &std::path::Path) -> Result<(), crate::error::MeshGuardError> {
        std::fs::create_dir_all(config_dir)
            .map_err(|e| crate::error::MeshGuardError::Io(e.to_string()))?;
        let path = config_dir.join("meshguard.json");
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| crate::error::MeshGuardError::Serialization(e.to_string()))?;
        std::fs::write(&path, data)
            .map_err(|e| crate::error::MeshGuardError::Io(e.to_string()))?;
        Ok(())
    }
}
