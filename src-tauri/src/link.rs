//! Radio link lifecycle: connecting, noticing a lost link, and reconnecting.
//!
//! The last successful connection target is saved in the config. When the
//! link drops without the user asking (radio battery empty, out of range,
//! radio rebooted), a background loop retries it with back-off until it
//! works or the user connects elsewhere / disconnects. On app start the UI
//! asks for the same loop, so the app comes back to its radio on its own.
//!
//! Each connection gets a generation number. A listener that ends reports
//! its generation, so the end of a link we already replaced is ignored.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;

use crate::error::MeshGuardError;
use crate::mesh_radio::{ConnectParams, MeshRadio};
use crate::state::AppState;

/// Where to find the radio again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LinkTarget {
    Ble {
        name: String,
        /// Required on Android; desktop scans by name.
        #[serde(default)]
        address: Option<String>,
    },
    Tcp {
        address: String,
    },
    Serial {
        port: String,
    },
}

impl LinkTarget {
    pub fn label(&self) -> String {
        match self {
            Self::Ble { name, .. } => name.clone(),
            Self::Tcp { address } => address.clone(),
            Self::Serial { port } => port.clone(),
        }
    }
}

/// Default Meshtastic BLE PIN. A reconnect reuses the existing bond, so the
/// PIN is only consulted if the phone lost the bond.
const DEFAULT_BLE_PIN: u32 = 123_456;
const FIRST_RETRY: Duration = Duration::from_secs(3);
const MAX_RETRY: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct LinkManager {
    /// Generation of the current link; bumped by every connect/disconnect.
    link_gen: AtomicU64,
    /// Bumped to cancel a running reconnect loop.
    reconnect_gen: AtomicU64,
    /// Serialises connection attempts (manual and automatic).
    connect_lock: Mutex<()>,
}

#[derive(Clone, Serialize)]
pub struct ReconnectEvent {
    pub target: String,
    pub attempt: u32,
    pub retry_in_secs: u64,
}

fn connect_params(app: &tauri::AppHandle, state: &AppState, link_gen: u64) -> ConnectParams {
    ConnectParams {
        app_handle: app.clone(),
        mesh_nodes: state.mesh_nodes.clone(),
        my_node_num: state.my_node_num.clone(),
        my_device_name: state.my_device_name.clone(),
        session_keys: state.session_keys.clone(),
        pending_pair_requests: state.pending_pair_requests.clone(),
        radio: state.radio.clone(),
        config: state.config.clone(),
        config_dir: state.config_dir.clone(),
        inbound: state.inbound.clone(),
        link_gen,
    }
}

/// Connect to `target`; caller holds `connect_lock`.
async fn connect_locked(
    target: &LinkTarget,
    pin: u32,
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<(), MeshGuardError> {
    // Drop the old link first; its listener's end is then stale.
    let link_gen = state.link.link_gen.fetch_add(1, Ordering::SeqCst) + 1;
    *state.radio.lock().await = None;

    let previous_node = *state.my_node_num.lock().await;
    let p = connect_params(app, state, link_gen);
    let radio = match target {
        LinkTarget::Ble { name, address } => {
            MeshRadio::connect_ble(name, address.clone(), pin, p).await?
        }
        LinkTarget::Tcp { address } => MeshRadio::connect_tcp(address, p).await?,
        LinkTarget::Serial { port } => MeshRadio::connect_serial(port, p).await?,
    };
    *state.radio.lock().await = Some(radio);

    let my_node = *state.my_node_num.lock().await;
    {
        let mut config = state.config.lock().await;
        config.last_link = Some(target.clone());
        config.last_ble_address = Some(format!(
            "{}:{}",
            match target {
                LinkTarget::Ble { .. } => "ble",
                LinkTarget::Tcp { .. } => "tcp",
                LinkTarget::Serial { .. } => "serial",
            },
            target.label()
        ));
        if let Some(name) = state.my_device_name.lock().await.as_ref() {
            config.last_device_name = Some(name.clone());
        }
        config.save(&state.config_dir)?;
    }

    // Keys belong to one local radio: drop in-memory keys from another one,
    // then restore the remembered keys for this radio.
    if previous_node.is_some() && previous_node != my_node {
        state.session_keys.lock().await.clear();
    }
    if let Some(my_node) = my_node {
        restore_keys(app, state, my_node).await;
    }
    tracing::info!("Connected to {}", target.label());
    Ok(())
}

/// Load remembered chat keys for `my_node` into memory (ADR-002).
async fn restore_keys(app: &tauri::AppHandle, state: &AppState, my_node: u32) {
    let Some(vault) = state.vault(app).await else {
        return;
    };
    let peers: Vec<u32> = state
        .config
        .lock()
        .await
        .peers
        .iter()
        .map(|p| p.node_num)
        .collect();
    let loaded = crate::key_store::load_for(&state.config_dir, &vault, my_node, &peers);
    if loaded.is_empty() {
        return;
    }
    let mut keys = state.session_keys.lock().await;
    let mut restored = 0;
    for (peer, key) in loaded {
        // A key set up in this session wins over the stored one.
        if let std::collections::hash_map::Entry::Vacant(e) = keys.entry(peer) {
            e.insert(key);
            restored += 1;
        }
    }
    tracing::info!("Restored {restored} remembered chat key(s)");
}

/// User-initiated connect: cancels any reconnect loop first.
pub async fn connect(
    target: LinkTarget,
    pin: Option<u32>,
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<(), MeshGuardError> {
    state.link.reconnect_gen.fetch_add(1, Ordering::SeqCst);
    let _guard = state.link.connect_lock.lock().await;
    connect_locked(&target, pin.unwrap_or(DEFAULT_BLE_PIN), app, state).await
}

/// User-initiated disconnect: stops reconnecting and forgets nothing else.
pub async fn disconnect(state: &AppState) {
    state.link.reconnect_gen.fetch_add(1, Ordering::SeqCst);
    let _guard = state.link.connect_lock.lock().await;
    state.link.link_gen.fetch_add(1, Ordering::SeqCst);
    *state.radio.lock().await = None;
    *state.my_node_num.lock().await = None;
    state.mesh_nodes.lock().await.clear();
}

/// Start (or restart) the background loop that reconnects to the last radio.
/// Returns false when there is no saved target.
pub async fn start_reconnect_loop(app: tauri::AppHandle) -> bool {
    let state = app.state::<AppState>();
    let Some(target) = state.config.lock().await.last_link.clone() else {
        return false;
    };
    let my_loop = state.link.reconnect_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let app_for_loop = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app_for_loop.state::<AppState>();
        let mut delay = FIRST_RETRY;
        let mut attempt: u32 = 0;
        loop {
            if state.link.reconnect_gen.load(Ordering::SeqCst) != my_loop {
                return;
            }
            attempt += 1;
            let _ = app_for_loop.emit(
                "reconnecting",
                ReconnectEvent {
                    target: target.label(),
                    attempt,
                    retry_in_secs: delay.as_secs(),
                },
            );
            {
                let _guard = state.link.connect_lock.lock().await;
                if state.link.reconnect_gen.load(Ordering::SeqCst) != my_loop {
                    return;
                }
                match connect_locked(&target, DEFAULT_BLE_PIN, &app_for_loop, &state).await {
                    Ok(()) => {
                        tracing::info!("Reconnected to {} (attempt {attempt})", target.label());
                        let _ = app_for_loop.emit("reconnected", target.label());
                        return;
                    }
                    Err(e) => {
                        tracing::info!(
                            "Reconnect to {} failed (attempt {attempt}): {e}",
                            target.label()
                        );
                    }
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_RETRY);
        }
    });
    true
}

/// Called by a radio listener when its link ends.
pub fn on_link_lost(app: tauri::AppHandle, link_gen: u64) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if state.link.link_gen.load(Ordering::SeqCst) != link_gen {
            return; // an old link we already replaced or closed
        }
        *state.radio.lock().await = None;
        tracing::warn!("Radio link lost — reconnecting automatically");
        if !start_reconnect_loop(app.clone()).await {
            let _ = app.emit("connection-state", "disconnected");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_json_is_stable() {
        let t = LinkTarget::Ble {
            name: "T1000-E".into(),
            address: Some("AA:BB".into()),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, r#"{"kind":"ble","name":"T1000-E","address":"AA:BB"}"#);
        let back: LinkTarget =
            serde_json::from_str(r#"{"kind":"tcp","address":"10.0.0.2"}"#).unwrap();
        assert_eq!(back.label(), "10.0.0.2");
    }
}
