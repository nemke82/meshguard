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

pub use meshguard_engine::nodes::{MeshNodeInfo, ONLINE_WINDOW_SECS};

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
    /// Outstanding feature probes: peer node -> (probe id, sent at). An ACK
    /// for the id proves the peer understands the newer message types.
    /// At most one per peer; expired entries are pruned.
    pub caps_probes: Mutex<HashMap<u32, (meshguard_core::MessageId, u64)>>,
    /// Ids of new messages already in `dedup` whose message is not saved to
    /// the history yet; left out of every dedup snapshot written to disk.
    pub uncommitted: Mutex<Vec<(meshguard_core::PeerIdentity, meshguard_core::MessageId)>>,
}

impl InboundState {
    /// Write the dedup cache to disk, leaving out ids whose message is not
    /// saved to the history yet. Caller holds the `dedup` lock (lock order:
    /// `dedup`, then `uncommitted`).
    pub async fn save_dedup(
        &self,
        config_dir: &Path,
        dedup: &DedupCache,
    ) -> Result<(), crate::error::MeshGuardError> {
        let pending = self.uncommitted.lock().await;
        if pending.is_empty() {
            return crate::dedup_store::save(config_dir, dedup);
        }
        let mut snapshot = dedup.clone();
        for (sender, id) in pending.iter() {
            snapshot.forget(sender, id);
        }
        crate::dedup_store::save(config_dir, &snapshot)
    }
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
            caps_probes: Mutex::new(HashMap::new()),
            uncommitted: Mutex::new(Vec::new()),
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

    /// Session keys per peer node_num. Remembered across restarts only
    /// through the platform-protected key store (ADR-002).
    pub session_keys: Arc<Mutex<HashMap<u32, SessionKey>>>,

    /// Pending pair requests (node_num -> encrypted payload bytes)
    /// waiting for the user to enter a passphrase.
    pub pending_pair_requests: Arc<Mutex<HashMap<u32, Vec<u8>>>>,

    pub config: Arc<Mutex<AppConfig>>,
    pub config_dir: PathBuf,

    pub inbound: Arc<InboundState>,

    /// Radio link lifecycle (auto-reconnect).
    pub link: Arc<crate::link::LinkManager>,

    /// Local radio (node number) that `session_keys` belong to.
    pub keys_owner: Arc<Mutex<Option<u32>>>,

    /// Serialises check-in/SOS transmissions and enforces their airtime
    /// budget across concurrent requests.
    pub status_tx: Mutex<crate::status_tx::StatusTx>,

    /// The QR pairing offer this device is showing, if any.
    pub pair_offer: Mutex<Option<crate::commands::PairOffer>>,

    /// Signal history per node (diagnostics), in memory only.
    pub node_history: Arc<Mutex<crate::diagnostics::History>>,
    /// Conversation history and outbox (ADR-005); opened on first use.
    pub history: Mutex<crate::history::Slot>,
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
    /// The vault key, or `None` when this platform has no key store.
    pub async fn vault(&self, app: &tauri::AppHandle) -> Option<Arc<SessionKey>> {
        let mut vault = self.vault.lock().await;
        if vault.is_none() {
            *vault = crate::key_store::open_vault(app, &self.config_dir).await;
        }
        vault.clone()
    }

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
            link: Arc::new(crate::link::LinkManager::default()),
            keys_owner: Arc::new(Mutex::new(None)),
            my_position: Arc::new(Mutex::new(None)),
            pair_offer: Mutex::new(None),
            node_history: Arc::new(Mutex::new(crate::diagnostics::History::default())),
            history: Mutex::new(crate::history::Slot::Closed),
            outbox_lock: Mutex::new(()),
            path_tests: Arc::new(Mutex::new(crate::diagnostics::PathTests::default())),
            status_tx: Mutex::new(crate::status_tx::StatusTx::default()),
            vault: Mutex::new(None),
        }
    }
}
