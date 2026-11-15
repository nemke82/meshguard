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
    /// Message features the peer's app is known to understand
    /// (`meshguard_core::status::FEATURE_*`); learned, never assumed.
    #[serde(default)]
    pub features: u32,
    /// The peer is known to hold our key: it sent us an authenticated frame,
    /// or we accepted its request. Only then may we send it background
    /// frames (feature probes) — before that, any frame from us would look
    /// like a new chat request and replace the pending one.
    #[serde(default)]
    pub confirmed: bool,
}

/// Record that `node_num` holds our key; saves only on change.
pub fn mark_peer_confirmed(config: &mut AppConfig, config_dir: &std::path::Path, node_num: u32) {
    if let Some(p) = config.peers.iter_mut().find(|p| p.node_num == node_num) {
        if !p.confirmed {
            p.confirmed = true;
            if let Err(e) = config.save(config_dir) {
                tracing::warn!("Could not save peer state: {e}");
            }
        }
    }
}

/// Record that `node_num` understands `bits`; saves only on change.
pub fn mark_peer_features(
    config: &mut AppConfig,
    config_dir: &std::path::Path,
    node_num: u32,
    bits: u32,
) {
    if let Some(p) = config.peers.iter_mut().find(|p| p.node_num == node_num) {
        if p.features & bits != bits {
            p.features |= bits;
            if let Err(e) = config.save(config_dir) {
                tracing::warn!("Could not save peer features: {e}");
            }
        }
    }
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

/// How many radios the connect screen remembers.
pub const MAX_SAVED_LINKS: usize = 12;

/// A radio we connected to before, shown under "Saved radios".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLink {
    /// Stable key: transport kind + node number (`tcp:!a1b2c3d4`). A Wi-Fi
    /// radio that gets a new IP from DHCP keeps its entry.
    pub id: String,
    pub target: crate::link::LinkTarget,
    /// The radio's own name as it reported it (long name).
    pub name: String,
    #[serde(default)]
    pub node_num: Option<u32>,
    /// Unix seconds of the last successful connect.
    pub last_connected: i64,
}

pub fn saved_link_id(target: &crate::link::LinkTarget, node_num: Option<u32>) -> String {
    match node_num {
        Some(n) => format!("{}:!{n:08x}", target.kind()),
        None => format!("{}:{}", target.kind(), target.label()),
    }
}

/// Record a successful connect: update the radio's entry (new address,
/// name, time) or add it, newest first, capped at [`MAX_SAVED_LINKS`].
pub fn remember_link(
    config: &mut AppConfig,
    target: &crate::link::LinkTarget,
    name: Option<&str>,
    node_num: Option<u32>,
    now: i64,
) {
    let id = saved_link_id(target, node_num);
    let old = config.saved_links.iter().position(|l| l.id == id);
    let old_name = old.map(|i| config.saved_links.remove(i).name);
    // Another entry for the same address belonged to whatever radio had it
    // before; keep it unless it was only known by that address.
    config
        .saved_links
        .retain(|l| !(l.target == *target && l.node_num.is_none()));
    let name = name
        .map(str::to_owned)
        .or(old_name)
        .unwrap_or_else(|| target.label());
    config.saved_links.insert(
        0,
        SavedLink {
            id,
            target: target.clone(),
            name,
            node_num,
            last_connected: now,
        },
    );
    config.saved_links.truncate(MAX_SAVED_LINKS);
}

/// Remove a saved radio. Forgetting the auto-connect radio also stops the
/// app from connecting to it on start. Returns whether anything was removed.
pub fn forget_link(config: &mut AppConfig, id: &str) -> bool {
    let Some(i) = config.saved_links.iter().position(|l| l.id == id) else {
        return false;
    };
    let removed = config.saved_links.remove(i);
    if config.last_link.as_ref() == Some(&removed.target) {
        config.last_link = None;
        config.last_ble_address = None;
    }
    true
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

    /// Radios connected to before, newest first.
    #[serde(default)]
    pub saved_links: Vec<SavedLink>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_ble_address: None,
            last_device_name: None,
            last_link: None,
            status_presets: default_status_presets(),
            peers: Vec::new(),
            saved_links: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn load(config_dir: &std::path::Path) -> Self {
        let path = config_dir.join("meshguard.json");
        let mut config: Self = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        config.seed_saved_links();
        config
    }

    /// Configs from before saved radios only know the last link: list it.
    fn seed_saved_links(&mut self) {
        if !self.saved_links.is_empty() {
            return;
        }
        if let Some(target) = self.last_link.clone() {
            let name = match &target {
                crate::link::LinkTarget::Ble { name, .. } => Some(name.clone()),
                _ => self.last_device_name.clone(),
            };
            remember_link(self, &target, name.as_deref(), None, 0);
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
    use crate::link::LinkTarget;

    fn tcp(addr: &str) -> LinkTarget {
        LinkTarget::Tcp {
            address: addr.into(),
        }
    }

    #[test]
    fn new_ip_updates_the_same_radio() {
        let mut cfg = AppConfig::default();
        remember_link(
            &mut cfg,
            &tcp("10.0.0.5:4403"),
            Some("Base"),
            Some(0xa1b2),
            1,
        );
        remember_link(
            &mut cfg,
            &tcp("10.0.0.9:4403"),
            Some("Base"),
            Some(0xa1b2),
            2,
        );
        assert_eq!(cfg.saved_links.len(), 1);
        assert_eq!(cfg.saved_links[0].id, "tcp:!0000a1b2");
        assert_eq!(cfg.saved_links[0].target, tcp("10.0.0.9:4403"));
        assert_eq!(cfg.saved_links[0].last_connected, 2);
    }

    #[test]
    fn newest_first_and_capped() {
        let mut cfg = AppConfig::default();
        for n in 0..(MAX_SAVED_LINKS as u32 + 3) {
            remember_link(
                &mut cfg,
                &tcp(&format!("10.0.0.{n}:4403")),
                None,
                Some(n),
                n as i64,
            );
        }
        assert_eq!(cfg.saved_links.len(), MAX_SAVED_LINKS);
        assert_eq!(
            cfg.saved_links[0].node_num,
            Some(MAX_SAVED_LINKS as u32 + 2)
        );
    }

    #[test]
    fn same_radio_over_two_transports_is_two_entries() {
        let mut cfg = AppConfig::default();
        let ble = LinkTarget::Ble {
            name: "Base".into(),
            address: Some("AA".into()),
        };
        remember_link(&mut cfg, &ble, Some("Base"), Some(7), 1);
        remember_link(&mut cfg, &tcp("10.0.0.5:4403"), Some("Base"), Some(7), 2);
        assert_eq!(cfg.saved_links.len(), 2);
    }

    #[test]
    fn seeded_entry_is_replaced_once_node_is_known() {
        let mut cfg: AppConfig = serde_json::from_str(
            r#"{"last_link":{"kind":"tcp","address":"10.0.0.5:4403"},"last_device_name":"Base"}"#,
        )
        .unwrap();
        cfg.seed_saved_links();
        assert_eq!(cfg.saved_links[0].id, "tcp:10.0.0.5:4403");
        assert_eq!(cfg.saved_links[0].name, "Base");
        remember_link(&mut cfg, &tcp("10.0.0.5:4403"), Some("Base"), Some(9), 5);
        assert_eq!(cfg.saved_links.len(), 1);
        assert_eq!(cfg.saved_links[0].id, "tcp:!00000009");
    }

    #[test]
    fn forgetting_the_auto_connect_radio_clears_it() {
        let mut cfg = AppConfig {
            last_link: Some(tcp("10.0.0.5:4403")),
            ..Default::default()
        };
        remember_link(&mut cfg, &tcp("10.0.0.5:4403"), None, Some(9), 1);
        assert!(forget_link(&mut cfg, "tcp:!00000009"));
        assert!(cfg.last_link.is_none());
        assert!(cfg.saved_links.is_empty());
        assert!(!forget_link(&mut cfg, "tcp:!00000009"));
    }

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
            features: 0,
            confirmed: false,
        };
        let json = serde_json::to_string(&peer).unwrap();
        assert!(json.contains(r#""protocol":"v2""#), "{json}");
    }
}
