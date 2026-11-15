//! Remembered chat keys (ADR-002).
//!
//! Session keys are sealed with a random local *vault key* and written to
//! `session_keys.json` next to `meshguard.json`. The vault key is never
//! stored in the clear: on Android it is wrapped by a non-exportable AES key
//! in the Android Keystore (`vault.key` holds only the wrapped bytes). On
//! platforms without a supported key store the vault is unavailable and keys
//! stay in memory only, as before.
//!
//! The passphrase itself is never stored.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use meshguard_core::keystore::{open_session_key, seal_session_key};
use serde::{Deserialize, Serialize};

use crate::crypto::SessionKey;
use crate::error::MeshGuardError;

const KEYS_FILE: &str = "session_keys.json";
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const VAULT_FILE: &str = "vault.key";
const FORMAT_VERSION: u32 = 1;

/// Serialises every read-modify-write of `session_keys.json`, so concurrent
/// pairings or a removal racing a save cannot lose or resurrect entries.
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn file_lock() -> std::sync::MutexGuard<'static, ()> {
    FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    /// Our radio's node number when the chat was set up.
    my_node: u32,
    peer_node: u32,
    /// base64 of nonce || AES-GCM(vault, key bytes).
    blob: String,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn read(dir: &Path) -> Stored {
    match std::fs::read_to_string(dir.join(KEYS_FILE)) {
        Ok(data) => match serde_json::from_str::<Stored>(&data) {
            Ok(s) if s.version == FORMAT_VERSION => s,
            Ok(_) | Err(_) => {
                tracing::warn!("Ignoring unreadable {KEYS_FILE}");
                Stored::default()
            }
        },
        Err(_) => Stored::default(),
    }
}

fn write_atomic(dir: &Path, name: &str, data: &[u8]) -> Result<(), MeshGuardError> {
    std::fs::create_dir_all(dir).map_err(|e| MeshGuardError::Io(e.to_string()))?;
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, data).map_err(|e| MeshGuardError::Io(e.to_string()))?;
    std::fs::rename(&tmp, dir.join(name)).map_err(|e| MeshGuardError::Io(e.to_string()))
}

fn write(dir: &Path, stored: &Stored) -> Result<(), MeshGuardError> {
    let data =
        serde_json::to_vec(stored).map_err(|e| MeshGuardError::Serialization(e.to_string()))?;
    write_atomic(dir, KEYS_FILE, &data)
}

/// Store (or replace) the key for `peer_node` used from radio `my_node`.
pub fn save(
    dir: &Path,
    vault: &SessionKey,
    my_node: u32,
    peer_node: u32,
    key: &SessionKey,
) -> Result<(), MeshGuardError> {
    let blob = seal_session_key(vault, my_node, peer_node, key)
        .map_err(|e| MeshGuardError::Encryption(e.to_string()))?;
    let _guard = file_lock();
    let mut stored = read(dir);
    stored.version = FORMAT_VERSION;
    stored
        .entries
        .retain(|e| !(e.my_node == my_node && e.peer_node == peer_node));
    stored.entries.push(Entry {
        my_node,
        peer_node,
        blob: b64().encode(blob),
    });
    write(dir, &stored)
}

/// Forget every stored key for `peer_node` (any local radio).
pub fn remove_peer(dir: &Path, peer_node: u32) -> Result<(), MeshGuardError> {
    let _guard = file_lock();
    let mut stored = read(dir);
    let before = stored.entries.len();
    stored.entries.retain(|e| e.peer_node != peer_node);
    if stored.entries.len() == before {
        return Ok(());
    }
    stored.version = FORMAT_VERSION;
    write(dir, &stored)
}

/// Keys usable from radio `my_node`, for peers that are still saved.
/// Entries that fail to open are skipped (logged), never fatal.
pub fn load_for(
    dir: &Path,
    vault: &SessionKey,
    my_node: u32,
    peers: &[u32],
) -> Vec<(u32, SessionKey)> {
    let stored = {
        let _guard = file_lock();
        read(dir)
    };
    stored
        .entries
        .into_iter()
        .filter(|e| e.my_node == my_node && peers.contains(&e.peer_node))
        .filter_map(|e| {
            let blob = b64().decode(&e.blob).ok()?;
            match open_session_key(vault, my_node, e.peer_node, &blob) {
                Ok(k) => Some((e.peer_node, k)),
                Err(_) => {
                    tracing::warn!("Stored key for node {} could not be opened", e.peer_node);
                    None
                }
            }
        })
        .collect()
}

/// Make the in-memory chat keys match the radio we just connected to, then
/// restore its remembered keys and open the history. Runs before the
/// listener starts, so the first packets can already be decrypted and saved.
pub async fn prepare_for_radio(p: &crate::mesh_radio::ConnectParams, my_node: u32) {
    let peers: Vec<u32> = crate::state::lock_engine(&p.engine)
        .config
        .peers
        .iter()
        .map(|c| c.node_num)
        .collect();
    let loaded = match &p.vault {
        Some(vault) => {
            let (dir, vault, peers) = (p.config_dir.clone(), vault.clone(), peers);
            tokio::task::spawn_blocking(move || load_for(&dir, &vault, my_node, &peers))
                .await
                .unwrap_or_default()
        }
        None => Vec::new(),
    };

    let (restored, changed) = {
        let mut engine = crate::state::lock_engine(&p.engine);
        // Before the listener starts: incoming messages must find the
        // history open (or definitely off), never "not decided yet".
        engine.open_history(p.vault.as_deref());
        engine.adopt_keys(my_node, loaded)
    };
    if changed {
        // Signal history describes what *this* radio heard.
        p.node_history.lock().await.clear();
    }
    if restored > 0 {
        tracing::info!("Restored {restored} remembered chat key(s)");
    }
}

/// The local vault key, created on first use. `None` when this platform has
/// no supported key store (keys then stay in memory only).
#[cfg(target_os = "android")]
pub async fn open_vault(app: &tauri::AppHandle, dir: &Path) -> Option<Arc<SessionKey>> {
    let app = app.clone();
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || android::open_or_create(&app, &dir))
        .await
        .ok()
        .flatten()
        .map(Arc::new)
}

#[cfg(target_os = "ios")]
pub async fn open_vault(_app: &tauri::AppHandle, _dir: &Path) -> Option<Arc<SessionKey>> {
    tracing::debug!("No platform key store here — chat keys are kept in memory only");
    None
}

/// Desktop: the vault key lives in the OS keyring (Secret Service on
/// Linux, Keychain on macOS, Credential Manager on Windows; ADR-005 §3).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn open_vault(_app: &tauri::AppHandle, _dir: &Path) -> Option<Arc<SessionKey>> {
    // The keyring client talks D-Bus/system APIs synchronously.
    tokio::task::spawn_blocking(desktop::open_or_create)
        .await
        .ok()
        .flatten()
        .map(Arc::new)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod desktop {
    use super::*;
    use keyring::{Entry, Error};

    const SERVICE: &str = "net.meshguard.app";
    const ACCOUNT: &str = "vault-v1";

    pub(super) fn open_or_create() -> Option<SessionKey> {
        open_or_create_in(SERVICE, ACCOUNT)
    }

    pub(super) fn open_or_create_in(service: &str, account: &str) -> Option<SessionKey> {
        let entry = match Entry::new(service, account) {
            Ok(e) => e,
            Err(e) => {
                tracing::info!("No OS keyring ({e}) — chats and history stay in memory only");
                return None;
            }
        };
        match entry.get_secret() {
            Ok(bytes) => match <[u8; 32]>::try_from(bytes.as_slice()) {
                Ok(arr) => Some(SessionKey::from_bytes(arr)),
                Err(_) => {
                    // Never overwrite: an unexpected value may be someone
                    // else's entry or a future format.
                    tracing::warn!("Keyring vault entry has an unexpected size — not using it");
                    None
                }
            },
            Err(Error::NoEntry) => {
                let key = SessionKey::random();
                let bytes = key.to_bytes();
                if let Err(e) = entry.set_secret(bytes.as_slice()) {
                    tracing::warn!("Could not save the vault key in the keyring: {e}");
                    return None;
                }
                // Read back: some backends accept a write they cannot keep.
                match entry.get_secret() {
                    Ok(back) if back.as_slice() == bytes.as_slice() => {
                        tracing::info!("Created the vault key in the OS keyring");
                        Some(key)
                    }
                    _ => {
                        tracing::warn!("Keyring did not keep the vault key — memory only");
                        None
                    }
                }
            }
            Err(e) => {
                // Locked keyring, denied access, D-Bus failure: try again
                // next start, never replace the key.
                tracing::warn!("Keyring unavailable ({e}) — chats and history stay in memory only");
                None
            }
        }
    }

    /// Talks to the real OS keyring with a throw-away account; run with
    /// `cargo test -- --ignored keyring` on a desktop session.
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[ignore = "needs a desktop keyring"]
        fn keyring_vault_is_created_once_and_reused() {
            let account = format!("vault-test-{}", std::process::id());
            let first = open_or_create_in(SERVICE, &account).expect("keyring available");
            let again = open_or_create_in(SERVICE, &account).expect("keyring available");
            assert_eq!(*first.to_bytes(), *again.to_bytes());
            Entry::new(SERVICE, &account)
                .unwrap()
                .delete_credential()
                .unwrap();
        }
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use tauri::Manager;

    fn plugin_call(
        app: &tauri::AppHandle,
        cmd: &str,
        data: &[u8],
    ) -> Result<Vec<u8>, MeshGuardError> {
        let state = app.state::<crate::ble_plugin::BlePluginState<tauri::Wry>>();
        let resp = state
            .0
            .run_mobile_plugin::<serde_json::Value>(
                cmd,
                serde_json::json!({ "data": b64().encode(data) }),
            )
            .map_err(|e| MeshGuardError::Encryption(format!("{cmd}: {e}")))?;
        let out = resp
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| MeshGuardError::Encryption(format!("{cmd}: no data")))?;
        b64()
            .decode(out)
            .map_err(|e| MeshGuardError::Encryption(format!("{cmd}: {e}")))
    }

    /// Errors after which the wrapped vault key can never be opened again.
    fn is_permanent(e: &MeshGuardError) -> bool {
        let msg = e.to_string();
        [
            "no vault key",
            "KeyPermanentlyInvalidatedException",
            "UnrecoverableKeyException",
            "AEADBadTagException",
            "blob too short",
        ]
        .iter()
        .any(|m| msg.contains(m))
    }

    pub(super) fn open_or_create(app: &tauri::AppHandle, dir: &Path) -> Option<SessionKey> {
        let path = dir.join(VAULT_FILE);
        if let Ok(wrapped) = std::fs::read(&path) {
            match plugin_call(app, "unwrapSecret", &wrapped) {
                Ok(raw) => {
                    let raw = zeroize::Zeroizing::new(raw);
                    if let Ok(bytes) = <[u8; 32]>::try_from(raw.as_slice()) {
                        return Some(SessionKey::from_bytes(bytes));
                    }
                    tracing::warn!("Vault key has the wrong length — starting a new vault");
                }
                Err(e) if is_permanent(&e) => {
                    // The Keystore key is gone or invalidated (e.g. app data
                    // restored to a new device): stored keys cannot be opened.
                    tracing::warn!("Vault key is no longer usable ({e}) — starting a new vault");
                }
                Err(e) => {
                    // Possibly temporary (Keystore busy, plugin not ready):
                    // keep everything and try again on the next connect.
                    tracing::warn!("Vault key could not be unwrapped right now ({e})");
                    return None;
                }
            }
            let _guard = file_lock();
            let _ = std::fs::remove_file(dir.join(KEYS_FILE));
        }

        let vault = SessionKey::random();
        let raw = vault.to_bytes();
        match plugin_call(app, "wrapSecret", raw.as_slice()) {
            Ok(wrapped) => match write_atomic(dir, VAULT_FILE, &wrapped) {
                Ok(()) => Some(vault),
                Err(e) => {
                    tracing::warn!("Could not store vault key: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("Android Keystore unavailable ({e}) — keys stay in memory");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_p2p_key;

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("meshguard-keys-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_load_remove() {
        let dir = tempdir();
        let vault = SessionKey::random();
        let k = derive_p2p_key("A", "B", "pw").unwrap();
        save(&dir, &vault, 1, 2, &k).unwrap();
        save(&dir, &vault, 1, 3, &k).unwrap();

        let loaded = load_for(&dir, &vault, 1, &[2, 3]);
        assert_eq!(loaded.len(), 2);
        let ct = k.encrypt(b"x").unwrap();
        assert_eq!(loaded[0].1.decrypt(&ct).unwrap(), b"x");

        assert!(load_for(&dir, &vault, 9, &[2, 3]).is_empty(), "other radio");
        assert_eq!(
            load_for(&dir, &vault, 1, &[3]).len(),
            1,
            "unsaved peers skipped"
        );

        remove_peer(&dir, 2).unwrap();
        assert_eq!(load_for(&dir, &vault, 1, &[2, 3]).len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn file_never_contains_raw_key() {
        let dir = tempdir();
        let vault = SessionKey::random();
        let k = derive_p2p_key("A", "B", "pw").unwrap();
        save(&dir, &vault, 1, 2, &k).unwrap();
        let data = std::fs::read(dir.join(KEYS_FILE)).unwrap();
        let raw = k.to_bytes();
        assert!(!data.windows(32).any(|w| w == raw.as_slice()));
        assert!(!String::from_utf8_lossy(&data).contains(&b64().encode(raw.as_slice())));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempdir();
        std::fs::write(dir.join(KEYS_FILE), b"{nope").unwrap();
        assert!(load_for(&dir, &SessionKey::random(), 1, &[2]).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
