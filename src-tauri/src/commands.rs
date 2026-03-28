use serde::Serialize;
use tauri::State;

use crate::crypto;
use crate::device_config::PeerConfig;
use crate::error::MeshGuardError;
use crate::mesh_radio::{MeshRadio, ScannedBleDevice};
use crate::protocol::MeshMessage;
use crate::state::{AppState, MeshNodeInfo};

// ── BLE Scanning ──────────────────────────────────────────────

#[derive(Serialize)]
pub struct ScanResponse {
    pub devices: Vec<ScannedBleDevice>,
}

/// Scan for nearby Meshtastic BLE devices.
#[tauri::command]
pub async fn scan_ble_devices() -> Result<ScanResponse, MeshGuardError> {
    let devices = crate::mesh_radio::scan_ble_devices(5).await?;
    Ok(ScanResponse { devices })
}

// ── Connection ────────────────────────────────────────────────

/// Connect to a Meshtastic device by BLE name, run config handshake,
/// and start background listener.
#[tauri::command]
pub async fn connect_device(
    ble_name: String,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    // Disconnect existing connection if any
    let mut radio_guard = state.radio.lock().await;
    if radio_guard.is_some() {
        *radio_guard = None;
    }
    drop(radio_guard);

    let radio = MeshRadio::connect_ble(
        &ble_name,
        app_handle,
        state.mesh_nodes.clone(),
        state.my_node_num.clone(),
        state.my_device_name.clone(),
        state.session_keys.clone(),
        state.pending_pair_requests.clone(),
    )
    .await?;

    *state.radio.lock().await = Some(radio);

    // Persist the BLE name for future auto-reconnect
    let mut config = state.config.lock().await;
    config.last_ble_address = Some(ble_name);
    if let Some(name) = state.my_device_name.lock().await.as_ref() {
        config.last_device_name = Some(name.clone());
    }
    config.save(&state.config_dir)?;

    tracing::info!("Connected and configured");
    Ok(())
}

/// Disconnect from the Meshtastic device.
#[tauri::command]
pub async fn disconnect_device(state: State<'_, AppState>) -> Result<(), MeshGuardError> {
    *state.radio.lock().await = None;
    *state.my_node_num.lock().await = None;
    state.mesh_nodes.lock().await.clear();
    tracing::info!("Disconnected");
    Ok(())
}

/// Check connection status.
#[tauri::command]
pub async fn is_connected(state: State<'_, AppState>) -> Result<bool, MeshGuardError> {
    Ok(state.radio.lock().await.is_some())
}

// ── Mesh Nodes ────────────────────────────────────────────────

/// Get all known mesh nodes.
#[tauri::command]
pub async fn get_mesh_nodes(
    state: State<'_, AppState>,
) -> Result<Vec<MeshNodeInfo>, MeshGuardError> {
    let nodes = state.mesh_nodes.lock().await;
    Ok(nodes.values().cloned().collect())
}

/// Get our own device info.
#[derive(Serialize)]
pub struct MyDeviceInfo {
    pub node_num: u32,
    pub device_name: String,
}

#[tauri::command]
pub async fn get_my_device_info(
    state: State<'_, AppState>,
) -> Result<Option<MyDeviceInfo>, MeshGuardError> {
    let num = *state.my_node_num.lock().await;
    let name = state.my_device_name.lock().await.clone();
    match (num, name) {
        (Some(n), Some(name)) => Ok(Some(MyDeviceInfo {
            node_num: n,
            device_name: name,
        })),
        _ => Ok(None),
    }
}

// ── Chat / Pairing ────────────────────────────────────────────

/// Start a chat with a mesh node. Derives key from passphrase,
/// sends an encrypted PairRequest, and stores the session key.
#[tauri::command]
pub async fn start_chat(
    peer_node_num: u32,
    passphrase: String,
    state: State<'_, AppState>,
) -> Result<PeerConfig, MeshGuardError> {
    let my_name = state
        .my_device_name
        .lock()
        .await
        .clone()
        .ok_or(MeshGuardError::NotConnected)?;

    let peer_name = {
        let nodes = state.mesh_nodes.lock().await;
        nodes
            .get(&peer_node_num)
            .map(|n| n.long_name.clone())
            .ok_or(MeshGuardError::NodeNotFound(peer_node_num))?
    };

    let session_key = crypto::derive_p2p_key(&my_name, &peer_name, &passphrase)?;

    // Build and encrypt a PairRequest
    let pair_request = MeshMessage::new_pair_request(&my_name);
    let encrypted = pair_request.encrypt_envelope(&session_key)?;

    // Send it via the mesh
    {
        let mut radio_guard = state.radio.lock().await;
        let radio = radio_guard
            .as_mut()
            .ok_or(MeshGuardError::NotConnected)?;
        radio.send_private_app(encrypted, peer_node_num).await?;
    }

    // Store session key
    state
        .session_keys
        .lock()
        .await
        .insert(peer_node_num, session_key);

    // Persist peer config
    let peer_id = uuid::Uuid::new_v4().to_string();
    let peer = PeerConfig {
        id: peer_id,
        device_name: peer_name,
        node_num: peer_node_num,
    };
    let mut config = state.config.lock().await;
    config.peers.retain(|p| p.node_num != peer_node_num);
    config.peers.push(peer.clone());
    config.save(&state.config_dir)?;

    tracing::info!("Chat initiated with node {peer_node_num}");
    Ok(peer)
}

/// Accept a pending pair request. Tries to decrypt the stored payload
/// with the derived key; if decryption succeeds, the passphrase matches.
#[tauri::command]
pub async fn accept_chat(
    peer_node_num: u32,
    passphrase: String,
    state: State<'_, AppState>,
) -> Result<PeerConfig, MeshGuardError> {
    let my_name = state
        .my_device_name
        .lock()
        .await
        .clone()
        .ok_or(MeshGuardError::NotConnected)?;

    let peer_name = {
        let nodes = state.mesh_nodes.lock().await;
        nodes
            .get(&peer_node_num)
            .map(|n| n.long_name.clone())
            .unwrap_or_else(|| format!("Node-{peer_node_num}"))
    };

    let session_key = crypto::derive_p2p_key(&my_name, &peer_name, &passphrase)?;

    // Try to decrypt the pending pair request to verify passphrase
    let pending = state
        .pending_pair_requests
        .lock()
        .await
        .remove(&peer_node_num);

    if let Some(payload) = pending {
        MeshMessage::decrypt_envelope(&payload, &session_key)
            .map_err(|_| MeshGuardError::PassphraseMismatch)?;
    }

    // Send PairAccept back
    let pair_accept = MeshMessage::new_pair_accept(&my_name);
    let encrypted = pair_accept.encrypt_envelope(&session_key)?;

    {
        let mut radio_guard = state.radio.lock().await;
        let radio = radio_guard
            .as_mut()
            .ok_or(MeshGuardError::NotConnected)?;
        radio.send_private_app(encrypted, peer_node_num).await?;
    }

    // Store session key
    state
        .session_keys
        .lock()
        .await
        .insert(peer_node_num, session_key);

    // Persist peer config
    let peer_id = uuid::Uuid::new_v4().to_string();
    let peer = PeerConfig {
        id: peer_id,
        device_name: peer_name,
        node_num: peer_node_num,
    };
    let mut config = state.config.lock().await;
    config.peers.retain(|p| p.node_num != peer_node_num);
    config.peers.push(peer.clone());
    config.save(&state.config_dir)?;

    tracing::info!("Chat accepted with node {peer_node_num}");
    Ok(peer)
}

// ── Messaging ─────────────────────────────────────────────────

/// Send an encrypted text message to a peer.
#[tauri::command]
pub async fn send_message(
    peer_node_num: u32,
    text: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let keys = state.session_keys.lock().await;
    let session_key = keys
        .get(&peer_node_num)
        .ok_or(MeshGuardError::NoSession)?;

    let msg = MeshMessage::new_text(&text, session_key)?;
    let encrypted = msg.encrypt_envelope(session_key)?;
    drop(keys);

    let mut radio_guard = state.radio.lock().await;
    let radio = radio_guard
        .as_mut()
        .ok_or(MeshGuardError::NotConnected)?;

    radio.send_private_app(encrypted, peer_node_num).await?;
    Ok(())
}

/// Check if we have a session key for a peer.
#[tauri::command]
pub async fn has_session(
    peer_node_num: u32,
    state: State<'_, AppState>,
) -> Result<bool, MeshGuardError> {
    let keys = state.session_keys.lock().await;
    Ok(keys.contains_key(&peer_node_num))
}

/// List saved peers from config.
#[tauri::command]
pub async fn list_peers(
    state: State<'_, AppState>,
) -> Result<Vec<PeerConfig>, MeshGuardError> {
    let config = state.config.lock().await;
    Ok(config.peers.clone())
}

/// Remove a saved peer.
#[tauri::command]
pub async fn remove_peer(
    peer_node_num: u32,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    state.session_keys.lock().await.remove(&peer_node_num);

    let mut config = state.config.lock().await;
    config.peers.retain(|p| p.node_num != peer_node_num);
    config.save(&state.config_dir)?;
    Ok(())
}

/// Get the last connected BLE device name (for auto-reconnect).
#[tauri::command]
pub async fn get_last_connection(
    state: State<'_, AppState>,
) -> Result<Option<String>, MeshGuardError> {
    let config = state.config.lock().await;
    Ok(config.last_ble_address.clone())
}
