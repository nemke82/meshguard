use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use meshguard_engine::Engine;

use crate::crypto::SessionKey;
use crate::mesh_radio::MeshRadio;

pub use meshguard_engine::engine::DEDUP_CAPACITY;
pub use meshguard_engine::nodes::{MeshNodeInfo, ONLINE_WINDOW_SECS};

/// The engine, shared by the commands, the radio listener and the outbox.
/// A `std` mutex: never hold it across an `.await`.
pub type SharedEngine = Arc<std::sync::Mutex<Engine>>;

/// Lock the engine (a panic while it was held does not make it unusable).
pub fn lock_engine(engine: &SharedEngine) -> std::sync::MutexGuard<'_, Engine> {
    engine.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared application state managed by Tauri.
pub struct AppState {
    /// The mesh radio connection (wraps meshtastic crate API).
    pub radio: Arc<Mutex<Option<MeshRadio>>>,

    /// Keys, peers and settings, nodes, replay protection and the history
    /// store (`meshguard-engine`).
    pub engine: SharedEngine,
    pub config_dir: PathBuf,

    /// Radio link lifecycle (auto-reconnect).
    pub link: Arc<crate::link::LinkManager>,

    /// Serialises check-in/SOS transmissions and enforces their airtime
    /// budget across concurrent requests.
    pub status_tx: Mutex<crate::status_tx::StatusTx>,

    /// The QR pairing offer this device is showing, if any.
    pub pair_offer: Mutex<Option<crate::commands::PairOffer>>,

    /// Signal history per node (diagnostics), in memory only.
    pub node_history: Arc<Mutex<crate::diagnostics::History>>,
    /// Serialises outbox work (send, resend, ACK, delete), so each step
    /// reads the current stored state and a row is never sent twice.
    pub outbox_lock: Mutex<()>,
    /// Outstanding path tests (traceroute).
    pub path_tests: Arc<Mutex<crate::diagnostics::PathTests>>,

    /// Latest GPS fix reported by our radio (for check-ins and SOS).
    pub my_position: Arc<Mutex<Option<meshguard_core::status::GeoFix>>>,

    /// Local vault key protecting remembered chat keys; opened lazily and
    /// retried after a failure.
    vault: Mutex<Option<Arc<SessionKey>>>,
}

impl AppState {
    /// Lock the engine; never hold the guard across an `.await`.
    pub fn engine(&self) -> std::sync::MutexGuard<'_, Engine> {
        lock_engine(&self.engine)
    }

    /// The vault key, or `None` when this platform has no key store.
    pub async fn vault(&self, app: &tauri::AppHandle) -> Option<Arc<SessionKey>> {
        let mut vault = self.vault.lock().await;
        if vault.is_none() {
            *vault = crate::key_store::open_vault(app, &self.config_dir).await;
        }
        vault.clone()
    }

    pub fn new(config_dir: PathBuf) -> Self {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        Self {
            radio: Arc::new(Mutex::new(None)),
            engine: Arc::new(std::sync::Mutex::new(Engine::new(config_dir.clone(), now))),
            config_dir,
            link: Arc::new(crate::link::LinkManager::default()),
            my_position: Arc::new(Mutex::new(None)),
            pair_offer: Mutex::new(None),
            node_history: Arc::new(Mutex::new(crate::diagnostics::History::default())),
            outbox_lock: Mutex::new(()),
            path_tests: Arc::new(Mutex::new(crate::diagnostics::PathTests::default())),
            status_tx: Mutex::new(crate::status_tx::StatusTx::default()),
            vault: Mutex::new(None),
        }
    }
}
