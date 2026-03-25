use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::ble::BleManager;
use crate::crypto::SessionKey;
use crate::device_config::AppConfig;

/// Shared application state managed by Tauri.
pub struct AppState {
    pub ble: Arc<Mutex<Option<BleManager>>>,
    /// Session keys per peer ID — derived in memory, never persisted.
    pub session_keys: Arc<Mutex<HashMap<String, SessionKey>>>,
    /// Currently active peer ID for messaging.
    pub active_peer_id: Arc<Mutex<Option<String>>>,
    pub config: Arc<Mutex<AppConfig>>,
    pub config_dir: PathBuf,
}

impl AppState {
    pub fn new(config_dir: PathBuf) -> Self {
        let config = AppConfig::load(&config_dir);
        Self {
            ble: Arc::new(Mutex::new(None)),
            session_keys: Arc::new(Mutex::new(HashMap::new())),
            active_peer_id: Arc::new(Mutex::new(None)),
            config: Arc::new(Mutex::new(config)),
            config_dir,
        }
    }
}
