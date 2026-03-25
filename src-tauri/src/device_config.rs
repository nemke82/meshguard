use serde::{Deserialize, Serialize};

/// Full Meshtastic device configuration — replaces the need for the
/// official Meshtastic Android app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// BLE address of the local Meshtastic device (entered once, saved).
    pub ble_address: String,

    /// Human-readable device name (e.g. "Alice-P1000").
    pub device_name: String,

    /// Whether the device has been BLE-bonded (paired).
    #[serde(default)]
    pub bonded: bool,

    /// Radio configuration.
    pub radio: RadioConfig,

    /// Channel configuration for the private P2P channel.
    pub channel: ChannelConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioConfig {
    /// LoRa region (must match between peers).
    pub region: LoraRegion,

    /// Modem preset — controls range vs speed tradeoff.
    pub modem_preset: ModemPreset,

    /// Transmit power in dBm (device max varies, typically 20-30).
    pub tx_power: u8,

    /// Hop limit — how many mesh hops a message can take (1 = direct only).
    pub hop_limit: u8,
}

impl Default for RadioConfig {
    fn default() -> Self {
        Self {
            region: LoraRegion::EU868,
            modem_preset: ModemPreset::LongRange,
            tx_power: 20,
            hop_limit: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Private channel name — auto-generated from pairing, but editable.
    pub name: String,

    /// 256-bit PSK — derived from the P2P pairing (device names + passphrase).
    /// This is written to the Meshtastic device so LoRa frames are encrypted at the mesh layer.
    #[serde(with = "hex_bytes")]
    pub psk: [u8; 32],

    /// Channel index on the device (0 = primary, 1-7 = secondary).
    pub index: u8,

    /// Uplink enabled (for MQTT gateway — disabled for privacy).
    pub uplink: bool,

    /// Downlink enabled (for MQTT gateway — disabled for privacy).
    pub downlink: bool,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            name: "MeshGuard".to_string(),
            psk: [0u8; 32],
            index: 0,
            uplink: false,
            downlink: false,
        }
    }
}

/// Meshtastic LoRa regions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum LoraRegion {
    US,        // 915 MHz
    EU868,     // 868 MHz
    EU433,     // 433 MHz
    CN,        // 470 MHz
    JP,        // 920 MHz
    ANZ,       // 915 MHz (Australia/NZ)
    KR,        // 920 MHz
    TW,        // 923 MHz
    RU,        // 868 MHz
    IN,        // 865 MHz
    NZ865,     // 865 MHz
    TH,        // 920 MHz
    UA868,     // 868 MHz
    UA433,     // 433 MHz
}

/// Meshtastic modem presets.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ModemPreset {
    /// Best range, slowest speed (~40 bps).
    LongRange,
    /// Good range, slow (~150 bps).
    LongModerate,
    /// Moderate range and speed (~1 kbps).
    MediumRange,
    /// Shorter range, faster (~5 kbps).
    ShortRange,
    /// Shortest range, fastest (~18 kbps).
    ShortFast,
}

/// P2P peer configuration — one per conversation partner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Unique peer ID (auto-generated UUID).
    pub id: String,

    /// Peer's device name.
    pub device_name: String,
}

/// Persisted app configuration — saved to disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub device: Option<DeviceConfig>,
    /// Multiple peers — each is a separate conversation.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

impl AppConfig {
    /// Load config from disk, or return default if not found.
    pub fn load(config_dir: &std::path::Path) -> Self {
        let path = config_dir.join("meshguard.json");
        match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save config to disk.
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

/// Hex serde helper for [u8; 32].
mod hex_bytes {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_string: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        serializer.serialize_str(&hex_string)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect::<Result<Vec<u8>, _>>()?;
        let mut arr = [0u8; 32];
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom("expected 32 bytes"));
        }
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }
}
