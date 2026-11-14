use meshguard_core::outbound::WireVersion;
use meshguard_core::status::StatusKind;
use serde::{Deserialize, Serialize};

/// Wire protocol negotiated with a peer (ADR-003). Pins both the version we
/// send and the minimum version we accept from that peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireProtocol {
    #[default]
    V1,
    V2,
}

impl From<WireProtocol> for WireVersion {
    fn from(p: WireProtocol) -> Self {
        match p {
            WireProtocol::V1 => WireVersion::V1,
            WireProtocol::V2 => WireVersion::V2,
        }
    }
}

impl From<WireVersion> for WireProtocol {
    fn from(v: WireVersion) -> Self {
        match v {
            WireVersion::V1 => WireProtocol::V1,
            WireVersion::V2 => WireProtocol::V2,
        }
    }
}

/// A paired peer — someone we've established a MeshGuard session with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub id: String,
    pub device_name: String,
    pub node_num: u32,
    /// Missing in configs written before 2.0, which therefore stay on v1.
    #[serde(default)]
    pub protocol: WireProtocol,
}

/// A one-tap check-in button: what kind of message it sends and its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPreset {
    pub kind: StatusKind,
    pub text: String,
    /// Attach the radio's GPS position when available.
    #[serde(default)]
    pub with_location: bool,
}

pub fn default_status_presets() -> Vec<StatusPreset> {
    let p = |kind, text: &str, with_location| StatusPreset {
        kind,
        text: text.into(),
        with_location,
    };
    vec![
        p(StatusKind::Ok, "I'm OK", false),
        p(StatusKind::OnMyWay, "On my way", false),
        p(StatusKind::NeedHelp, "I need help", true),
        p(StatusKind::Sos, "SOS — I need urgent help", true),
    ]
}

/// Persisted app configuration — saved to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// BLE address of the device we last connected to (for auto-reconnect).
    #[serde(default)]
    pub last_ble_address: Option<String>,

    /// Name of our last connected device (for display and key derivation).
    #[serde(default)]
    pub last_device_name: Option<String>,

    /// How to reach the radio again (auto-connect / auto-reconnect).
    #[serde(default)]
    pub last_link: Option<crate::link::LinkTarget>,

    /// One-tap check-in messages; the wording is the user's (editable).
    #[serde(default = "default_status_presets")]
    pub status_presets: Vec<StatusPreset>,

    /// Paired peers — each is a separate P2P conversation.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_ble_address: None,
            last_device_name: None,
            last_link: None,
            status_presets: default_status_presets(),
            peers: Vec::new(),
        }
    }
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
        std::fs::write(&path, data).map_err(|e| crate::error::MeshGuardError::Io(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_2_0_config_loads_as_v1() {
        let json = r#"{"peers":[{"id":"x","device_name":"Bob","node_num":7}]}"#;
        let cfg: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.peers[0].protocol, WireProtocol::V1);
    }

    #[test]
    fn old_config_gets_default_presets() {
        let cfg: AppConfig = serde_json::from_str(r#"{"peers":[]}"#).unwrap();
        assert_eq!(cfg.status_presets, default_status_presets());
        assert!(cfg
            .status_presets
            .iter()
            .any(|p| p.kind == StatusKind::Sos && p.with_location));
    }

    #[test]
    fn protocol_serializes_lowercase() {
        let peer = PeerConfig {
            id: "x".into(),
            device_name: "Bob".into(),
            node_num: 7,
            protocol: WireProtocol::V2,
        };
        let json = serde_json::to_string(&peer).unwrap();
        assert!(json.contains(r#""protocol":"v2""#), "{json}");
    }
}
