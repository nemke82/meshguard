use std::sync::Arc;
use tokio::sync::Mutex;

use crate::ble::BleManager;
use crate::crypto::{Identity, SessionKey};

/// Shared application state managed by Tauri.
pub struct AppState {
    pub ble: Arc<Mutex<BleManager>>,
    pub identity: Arc<Mutex<Option<Identity>>>,
    pub session_key: Arc<Mutex<Option<SessionKey>>>,
    pub device_id: String,
}

impl AppState {
    pub fn new(ble: BleManager, device_id: String) -> Self {
        Self {
            ble: Arc::new(Mutex::new(ble)),
            identity: Arc::new(Mutex::new(None)),
            session_key: Arc::new(Mutex::new(None)),
            device_id,
        }
    }
}
