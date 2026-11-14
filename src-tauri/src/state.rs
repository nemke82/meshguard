use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use meshguard_core::dedup::DedupCache;
use meshguard_core::inbound::InboundPolicy;
use meshguard_core::ratelimit::{Cooldown, TokenBucket};

use crate::crypto::SessionKey;
use crate::device_config::AppConfig;
use crate::mesh_radio::MeshRadio;

/// Info about a node seen on the mesh network.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MeshNodeInfo {
    pub node_num: u32,
    pub user_name: String,
    pub long_name: String,
    pub short_name: String,
    pub hw_model: String,
    /// SNR of the last packet heard from this node, in dB.
    pub snr: f32,
    /// RSSI of the last packet heard directly, in dBm (0 = unknown).
    pub rssi: i32,
    /// Unix seconds; refreshed by every packet we hear from the node.
    pub last_heard: i64,
    pub is_online: bool,
    /// Relay hops between us and the node (0 = heard directly).
    pub hops_away: Option<u32>,
    /// Battery percentage; above 100 means powered externally.
    pub battery_level: Option<u32>,
    pub voltage: Option<f32>,
    /// Channel utilisation seen by the node, in percent.
    pub channel_utilization: Option<f32>,
    /// The node's own transmit airtime over the last hour, in percent.
    pub air_util_tx: Option<f32>,
    /// Heard through an MQTT bridge rather than over the air.
    pub via_mqtt: bool,
}

/// A node counts as online when heard within this window (as in the
/// Meshtastic apps).
pub const ONLINE_WINDOW_SECS: i64 = 2 * 60 * 60;

/// Receive-side protection state shared with the radio listener.
///
/// The dedup cache is persisted to `dedup.json` (see `dedup_store`) until
/// the SQLite store lands (ADR-001 §5).
pub struct InboundState {
    pub policy: InboundPolicy,
    pub dedup: Mutex<DedupCache>,
    /// Global budget for outbound application ACKs (shared LoRa airtime).
    pub ack_budget: Mutex<TokenBucket>,
    /// At most one "chat request" notification per node per interval.
    pub pair_notify_per_node: Mutex<Cooldown<u32>>,
    /// And a global cap across all unknown nodes.
    pub pair_notify_global: Mutex<TokenBucket>,
}

/// Maximum remembered `(sender, message_id)` pairs.
pub const DEDUP_CAPACITY: usize = 4096;

impl InboundState {
    pub fn new(config_dir: &Path) -> Self {
        let policy = InboundPolicy::default();
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        Self {
            dedup: Mutex::new(crate::dedup_store::load(
                config_dir,
                &policy,
                DEDUP_CAPACITY,
                now,
            )),
            policy,
            // Burst of 5, then one ACK per 10 s.
            ack_budget: Mutex::new(TokenBucket::new(5, 10)),
            pair_notify_per_node: Mutex::new(Cooldown::new(60, 256)),
            // Burst of 3, then one per minute.
            pair_notify_global: Mutex::new(TokenBucket::new(3, 60)),
        }
    }
}

/// Shared application state managed by Tauri.
pub struct AppState {
    /// The mesh radio connection (wraps meshtastic crate API).
    pub radio: Arc<Mutex<Option<MeshRadio>>>,

    /// Nodes discovered on the mesh network (node_num -> info).
    pub mesh_nodes: Arc<Mutex<HashMap<u32, MeshNodeInfo>>>,

    /// Our own node number (set after connecting).
    pub my_node_num: Arc<Mutex<Option<u32>>>,

    /// Our device's long name (set after connecting).
    pub my_device_name: Arc<Mutex<Option<String>>>,

    /// Session keys per peer node_num — derived in memory, never persisted.
    pub session_keys: Arc<Mutex<HashMap<u32, SessionKey>>>,

    /// Pending pair requests (node_num -> encrypted payload bytes)
    /// waiting for the user to enter a passphrase.
    pub pending_pair_requests: Arc<Mutex<HashMap<u32, Vec<u8>>>>,

    pub config: Arc<Mutex<AppConfig>>,
    pub config_dir: PathBuf,

    pub inbound: Arc<InboundState>,
}

impl AppState {
    pub fn new(config_dir: PathBuf) -> Self {
        let config = AppConfig::load(&config_dir);
        let inbound = Arc::new(InboundState::new(&config_dir));
        Self {
            radio: Arc::new(Mutex::new(None)),
            mesh_nodes: Arc::new(Mutex::new(HashMap::new())),
            my_node_num: Arc::new(Mutex::new(None)),
            my_device_name: Arc::new(Mutex::new(None)),
            session_keys: Arc::new(Mutex::new(HashMap::new())),
            pending_pair_requests: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(Mutex::new(config)),
            config_dir,
            inbound,
        }
    }
}
