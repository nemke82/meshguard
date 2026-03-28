use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

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
    pub snr: f32,
    pub rssi: i32,
    pub last_heard: i64,
    pub is_online: bool,
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
}

impl AppState {
    pub fn new(config_dir: PathBuf) -> Self {
        let config = AppConfig::load(&config_dir);
        Self {
            radio: Arc::new(Mutex::new(None)),
            mesh_nodes: Arc::new(Mutex::new(HashMap::new())),
            my_node_num: Arc::new(Mutex::new(None)),
            my_device_name: Arc::new(Mutex::new(None)),
            session_keys: Arc::new(Mutex::new(HashMap::new())),
            pending_pair_requests: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(Mutex::new(config)),
            config_dir,
        }
    }
}
