//! The engine: the state the app and the Android service share (ADR-001
//! option D), behind one lock.
//!
//! Everything a received packet needs is here: chat keys, peers and
//! settings, replay protection and rate limits, pending chat requests, the
//! mesh node list and the history store. The app holds it in a
//! `std::sync::Mutex` and never across an `.await`; radio writes, UI events
//! and notifications are returned as [`crate::receive::Action`]s and carried
//! out by the caller.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use meshguard_core::crypto::SessionKey;
use meshguard_core::dedup::DedupCache;
use meshguard_core::inbound::InboundPolicy;
use meshguard_core::ratelimit::{Cooldown, TokenBucket};
use meshguard_core::status::FEATURE_STATUS;
use meshguard_core::MessageId;
use meshguard_store::{Store, StoreError};

use crate::config::{AppConfig, WireProtocol};
use crate::error::MeshGuardError;
use crate::nodes::MeshNodeInfo;

/// Maximum remembered `(sender, message_id)` pairs.
pub const DEDUP_CAPACITY: usize = 4096;

/// File name of the history store in the data folder.
pub const STORE_FILE: &str = "store.db";

/// Where history goes in this session.
pub enum Slot {
    /// Not tried yet (the vault may still be opening).
    Closed,
    Open(Store),
    /// Memory only; the reason is shown to the user.
    Off(String),
}

/// Receive-side protection: replay cache and airtime/notification limits.
pub struct Protection {
    pub policy: InboundPolicy,
    /// Persisted to `dedup.json` (see `dedup_store`).
    pub dedup: DedupCache,
    /// Global budget for outbound application ACKs (shared LoRa airtime).
    pub ack_budget: TokenBucket,
    /// At most one "chat request" notification per node per interval.
    pub pair_notify_per_node: Cooldown<u32>,
    /// And a global cap across all unknown nodes.
    pub pair_notify_global: TokenBucket,
    /// Outstanding feature probes: peer node -> (probe id, sent at). An ACK
    /// for the id proves the peer understands the newer message types.
    pub caps_probes: HashMap<u32, (MessageId, u64)>,
    /// Answering a peer's request for our `Caps`: once per node per hour.
    pub caps_reply_per_node: Cooldown<u32>,
    /// ACK allowance per message for relay copies (in memory), separate
    /// from the radio's (ADR-006 §6).
    pub relay_acks: DedupCache,
    /// ACK budget of the relay: burst 20, then one per second.
    pub relay_ack_budget: TokenBucket,
}

impl Protection {
    fn new(config_dir: &Path, now: u64) -> Self {
        let policy = InboundPolicy::default();
        Self {
            dedup: crate::dedup_store::load(config_dir, &policy, DEDUP_CAPACITY, now),
            policy,
            // Burst of 5, then one ACK per 10 s.
            ack_budget: TokenBucket::new(5, 10),
            pair_notify_per_node: Cooldown::new(60, 256),
            // Burst of 3, then one per minute.
            pair_notify_global: TokenBucket::new(3, 60),
            caps_probes: HashMap::new(),
            caps_reply_per_node: Cooldown::new(3600, 256),
            relay_acks: policy.new_dedup_cache(DEDUP_CAPACITY),
            relay_ack_budget: TokenBucket::new(20, 1),
        }
    }
}

pub struct Engine {
    pub config_dir: PathBuf,
    /// Peers, saved radios and settings (`meshguard.json`).
    pub config: AppConfig,
    /// Chat keys per peer node. Remembered across restarts only through
    /// the platform-protected key store (ADR-002).
    pub keys: HashMap<u32, SessionKey>,
    /// Local radio (node number) that `keys` belong to.
    pub keys_owner: Option<u32>,
    /// Undecryptable frames (node -> payload) waiting for the user to enter
    /// a passphrase.
    pub pending_pair_requests: HashMap<u32, Vec<u8>>,
    pub protection: Protection,
    /// Nodes on the mesh (without our own radio).
    pub nodes: HashMap<u32, MeshNodeInfo>,
    /// Our radio, once its configuration was read.
    pub my_node_num: Option<u32>,
    pub my_device_name: Option<String>,
    /// Conversation history and outbox (ADR-005).
    pub history: Slot,
    /// Outgoing messages being transmitted right now.
    pub(crate) in_flight: crate::outbox::InFlight,
    /// Attempts per route of messages being sent (in memory).
    pub(crate) route_counts: HashMap<MessageId, crate::outbox::RouteCounts>,
    /// Private MQTT relay state (sealed in `relay_secrets.json`).
    pub relay: crate::relay::RelayBook,
    /// The vault key, once known (seals the relay book).
    pub(crate) vault: Option<SessionKey>,
}

impl Engine {
    /// Load settings and the replay cache from `config_dir`. `now`: unix
    /// seconds.
    pub fn new(config_dir: PathBuf, now: u64) -> Self {
        Self {
            config: AppConfig::load(&config_dir),
            protection: Protection::new(&config_dir, now),
            config_dir,
            keys: HashMap::new(),
            keys_owner: None,
            pending_pair_requests: HashMap::new(),
            nodes: HashMap::new(),
            my_node_num: None,
            my_device_name: None,
            history: Slot::Closed,
            in_flight: Default::default(),
            route_counts: HashMap::new(),
            relay: Default::default(),
            vault: None,
        }
    }

    /// Block a node: forget the contact and its chat key, drop its pending
    /// chat request, and ignore its MeshGuard frames from now on. The caller
    /// saves the config and removes the remembered key. Returns false when
    /// the node was already blocked.
    pub fn block_node(&mut self, node: u32, name: String, now: i64) -> bool {
        self.keys.remove(&node);
        self.pending_pair_requests.remove(&node);
        self.config.peers.retain(|p| p.node_num != node);
        if self.config.is_blocked(node) {
            return false;
        }
        self.config.blocked.push(crate::config::BlockedNode {
            node_num: node,
            name,
            blocked_at: now,
        });
        true
    }

    /// Unblock a node. Returns false when it was not blocked.
    pub fn unblock_node(&mut self, node: u32) -> bool {
        let before = self.config.blocked.len();
        self.config.blocked.retain(|b| b.node_num != node);
        self.config.blocked.len() != before
    }

    /// Write the replay cache to disk.
    pub fn save_dedup(&self) -> Result<(), MeshGuardError> {
        crate::dedup_store::save(&self.config_dir, &self.protection.dedup)
    }

    /// Wire protocol pin and check-in support of a peer.
    pub fn peer_wire(&self, peer: u32) -> (WireProtocol, bool) {
        self.config
            .peers
            .iter()
            .find(|p| p.node_num == peer)
            .map(|p| (p.protocol, p.features & FEATURE_STATUS != 0))
            .unwrap_or_default()
    }

    /// Mesh long name of a node, if it announced one.
    pub fn node_name(&self, node: u32) -> Option<String> {
        self.nodes
            .get(&node)
            .map(|n| n.long_name.clone())
            .filter(|n| !n.trim().is_empty())
    }

    /// Display name of a peer: the name it paired with, else the mesh name.
    pub fn peer_title(&self, peer: u32) -> Option<String> {
        self.config
            .peers
            .iter()
            .find(|p| p.node_num == peer)
            .map(|p| p.device_name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| self.node_name(peer))
    }

    /// Install a chat key for radio `my_num`. Returns the key it replaced,
    /// for rollback. Refuses when the keys now belong to another radio.
    pub fn install_key(
        &mut self,
        my_num: u32,
        peer: u32,
        key: SessionKey,
    ) -> Result<Option<SessionKey>, MeshGuardError> {
        if self.keys_owner != Some(my_num) {
            return Err(MeshGuardError::Protocol(
                "The radio changed — please try again".into(),
            ));
        }
        Ok(self.keys.insert(peer, key))
    }

    /// Undo [`Self::install_key`], unless the keys meanwhile belong to
    /// another radio.
    pub fn rollback_key(&mut self, my_num: u32, peer: u32, previous: Option<SessionKey>) {
        if self.keys_owner != Some(my_num) {
            return;
        }
        match previous {
            Some(k) => {
                self.keys.insert(peer, k);
            }
            None => {
                self.keys.remove(&peer);
            }
        }
    }

    /// Make the in-memory keys belong to `my_node` and add its remembered
    /// keys (a key set up in this session wins). Returns how many were
    /// restored and whether the radio changed (callers then clear what
    /// described the previous radio).
    pub fn adopt_keys(&mut self, my_node: u32, loaded: Vec<(u32, SessionKey)>) -> (usize, bool) {
        let changed = self.keys_owner != Some(my_node);
        if changed {
            // Keys are tied to one local radio (identity is in the key and AAD).
            self.keys.clear();
            self.keys_owner = Some(my_node);
        }
        let mut restored = 0;
        for (peer, key) in loaded {
            if let std::collections::hash_map::Entry::Vacant(e) = self.keys.entry(peer) {
                e.insert(key);
                restored += 1;
            }
        }
        (restored, changed)
    }

    /// The radio connected to most recently, if its node number is known
    /// (from the saved radios).
    pub fn last_radio(&self) -> Option<u32> {
        let target = self.config.last_link.as_ref()?;
        self.config
            .saved_links
            .iter()
            .find(|l| l.target == *target)
            .and_then(|l| l.node_num)
    }

    // ── History store ─────────────────────────────────────────

    /// Open the history store with the vault key (`None`: no key store on
    /// this device, history stays in memory). Only the first call counts.
    pub fn open_history(&mut self, vault: Option<&SessionKey>) {
        if !matches!(self.history, Slot::Closed) {
            return;
        }
        if let Some(v) = vault {
            self.vault = Some(SessionKey::from_bytes(*v.to_bytes()));
            self.load_relay();
        }
        self.history = match vault {
            Some(v) => open_store(&self.config_dir, v),
            None => Slot::Off(
                "This device has no secure key store, so history is kept only while the app is open"
                    .into(),
            ),
        };
    }

    /// Run `f` on the store. `None` when history is memory-only (or not
    /// open yet) or `f` failed (logged without content).
    pub fn with_store<T>(
        &mut self,
        f: impl FnOnce(&mut Store) -> meshguard_store::Result<T>,
    ) -> Option<T> {
        match &mut self.history {
            Slot::Open(store) => match f(store) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("History store: {e}");
                    None
                }
            },
            _ => None,
        }
    }
}

/// Move an unreadable store aside (kept, never deleted) and start a new one.
fn set_aside(dir: &Path) {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    for suffix in ["", "-wal", "-shm"] {
        let from = dir.join(format!("{STORE_FILE}{suffix}"));
        if from.exists() {
            let to = dir.join(format!("{STORE_FILE}.unreadable-{stamp}{suffix}"));
            if let Err(e) = std::fs::rename(&from, &to) {
                tracing::warn!("Could not move {} aside: {e}", from.display());
            }
        }
    }
}

/// Open (or create) the history store in `dir`.
pub fn open_store(dir: &Path, vault: &SessionKey) -> Slot {
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Slot::Off(format!("Could not create the data folder: {e}"));
    }
    let path = dir.join(STORE_FILE);
    match Store::open(&path, vault) {
        Ok(store) => {
            tracing::info!("Message history store opened");
            Slot::Open(store)
        }
        Err(StoreError::WrongKey) => {
            // The vault key changed (keyring entry lost, app data restored
            // elsewhere). The old history cannot be read with this key.
            tracing::warn!("Message history was saved with another key; starting a new one");
            set_aside(dir);
            match Store::open(&path, vault) {
                Ok(store) => Slot::Open(store),
                Err(e) => Slot::Off(format!("History could not be opened: {e}")),
            }
        }
        Err(StoreError::TooNew { .. }) => {
            Slot::Off("History was saved by a newer MeshGuard; update the app to see it".into())
        }
        Err(e) => Slot::Off(format!("History could not be opened: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshguard_store::MessageKind;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mg-engine-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_store_from_another_key_is_set_aside_not_deleted() {
        let dir = tmp_dir("wrong-key");
        let a = SessionKey::from_bytes([1; 32]);
        let Slot::Open(mut s) = open_store(&dir, &a) else {
            panic!("store should open");
        };
        let c = s.upsert_conversation(1, 2, None).unwrap();
        s.insert_incoming(&c, MessageId::random(), MessageKind::Text, b"x", 1, 1)
            .unwrap();
        drop(s);

        let b = SessionKey::from_bytes([2; 32]);
        let Slot::Open(fresh) = open_store(&dir, &b) else {
            panic!("a new store should open");
        };
        assert!(fresh.conversations().unwrap().is_empty());
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                n.starts_with("store.db.unreadable-")
                    && !n.ends_with("-wal")
                    && !n.ends_with("-shm")
            })
            .collect();
        assert_eq!(kept.len(), 1, "old store kept aside: {kept:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_store_turns_history_off_and_stays() {
        let dir = tmp_dir("too-new");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let conn = rusqlite::Connection::open(dir.join(STORE_FILE)).unwrap();
            conn.pragma_update(None, "user_version", meshguard_store::SCHEMA_VERSION + 1)
                .unwrap();
        }
        assert!(matches!(
            open_store(&dir, &SessionKey::from_bytes([1; 32])),
            Slot::Off(_)
        ));
        assert!(dir.join(STORE_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_follow_the_radio() {
        let dir = tmp_dir("keys");
        let mut e = Engine::new(dir.clone(), 1);
        let k = |b| SessionKey::from_bytes([b; 32]);
        assert_eq!(e.adopt_keys(1, vec![(2, k(1))]), (1, true));
        // A key set up in this session wins over a remembered one.
        e.install_key(1, 3, k(9)).unwrap();
        assert_eq!(e.adopt_keys(1, vec![(3, k(1))]), (0, false));
        assert_eq!(e.keys[&3].to_bytes(), k(9).to_bytes());
        // Another radio: nothing carries over, and old installs are refused.
        assert_eq!(e.adopt_keys(5, vec![]), (0, true));
        assert!(e.keys.is_empty());
        assert!(e.install_key(1, 3, k(9)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_radio_comes_from_the_saved_links() {
        let dir = tmp_dir("last-radio");
        let mut e = Engine::new(dir.clone(), 1);
        assert_eq!(e.last_radio(), None);
        let target = crate::link_target::LinkTarget::Tcp {
            address: "10.0.0.5:4403".into(),
        };
        crate::config::remember_link(&mut e.config, &target, Some("Base"), Some(0x11), 1);
        e.config.last_link = Some(target);
        assert_eq!(e.last_radio(), Some(0x11));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
