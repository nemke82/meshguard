use serde::{Deserialize, Serialize};
use tauri::State;

use meshguard_core::codec::{decode_frame, DecodedFrame};
use meshguard_core::dedup::Seen;
use meshguard_core::envelope::{aad_context, MessageType, SealError, DEFAULT_FRAME_BUDGET};
use meshguard_core::outbound::{self, OutPayload};
use meshguard_core::v1::V1Payload;
use meshguard_core::PeerIdentity;

use crate::crypto;
use crate::device_config::{PeerConfig, WireProtocol};
use crate::error::MeshGuardError;
use crate::link::{self, LinkTarget};
use crate::mesh_radio::{ScannedBleDevice, SerialPortInfo};
use crate::state::{AppState, MeshNodeInfo};

// ── BLE Scanning ──────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct ScanResponse {
    pub devices: Vec<ScannedBleDevice>,
}

/// Scan for nearby Meshtastic BLE devices.
/// On Android, delegates to the native Kotlin BlePlugin.
/// On desktop, uses btleplug directly.
#[tauri::command]
pub async fn scan_ble_devices(
    #[allow(unused_variables)] app_handle: tauri::AppHandle,
) -> Result<ScanResponse, MeshGuardError> {
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let state = app_handle
            .try_state::<crate::ble_plugin::BlePluginState<tauri::Wry>>()
            .ok_or_else(|| MeshGuardError::Ble("BLE plugin not initialized".into()))?;
        let response: ScanResponse = state
            .0
            .run_mobile_plugin("scanDevices", ())
            .map_err(|e| MeshGuardError::Ble(format!("Native BLE scan failed: {e}")))?;
        return Ok(response);
    }
    #[cfg(not(target_os = "android"))]
    {
        let devices = crate::mesh_radio::scan_ble_devices(5).await?;
        Ok(ScanResponse { devices })
    }
}

// ── Serial Ports ──────────────────────────────────────────────

/// List available serial ports (for USB connection).
#[tauri::command]
pub async fn get_serial_ports() -> Result<Vec<SerialPortInfo>, MeshGuardError> {
    crate::mesh_radio::list_serial_ports()
}

// ── Connection ────────────────────────────────────────────────

/// Connect via Bluetooth LE.
/// `pin` is the BLE pairing PIN — defaults to 123456 (Meshtastic standard).
/// `ble_address` is the device MAC address — required on Android.
#[tauri::command]
pub async fn connect_device(
    ble_name: String,
    ble_address: Option<String>,
    pin: Option<u32>,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let target = LinkTarget::Ble {
        name: ble_name,
        address: ble_address,
    };
    link::connect(target, pin, &app_handle, &state).await
}

/// Connect via WiFi / TCP.
#[tauri::command]
pub async fn connect_tcp(
    address: String,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    link::connect(LinkTarget::Tcp { address }, None, &app_handle, &state).await
}

/// Connect via USB serial.
#[tauri::command]
pub async fn connect_serial(
    port_name: String,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    link::connect(
        LinkTarget::Serial { port: port_name },
        None,
        &app_handle,
        &state,
    )
    .await
}

/// Disconnect from the Meshtastic device.
#[tauri::command]
pub async fn disconnect_device(state: State<'_, AppState>) -> Result<(), MeshGuardError> {
    // Also stops any automatic reconnect.
    link::disconnect(&state).await;
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

fn unix_now() -> u32 {
    chrono::Utc::now().timestamp().clamp(0, i64::from(u32::MAX)) as u32
}

fn seal_err(e: SealError) -> MeshGuardError {
    match e {
        SealError::TooLarge { len, max } => MeshGuardError::Protocol(format!(
            "Message too long: {len} bytes on the wire, limit is {max}"
        )),
        SealError::InvalidBody => MeshGuardError::Protocol("invalid message body".into()),
        SealError::Crypto => MeshGuardError::Encryption("sealing failed".into()),
    }
}

async fn my_identity(state: &AppState) -> Result<(u32, String), MeshGuardError> {
    let num = (*state.my_node_num.lock().await).ok_or(MeshGuardError::NotConnected)?;
    let name = state
        .my_device_name
        .lock()
        .await
        .clone()
        .ok_or(MeshGuardError::NotConnected)?;
    Ok((num, name))
}

/// The peer's Meshtastic long name, which is part of the v1/v2 key
/// derivation. Refuses when it is not known yet: deriving with a placeholder
/// would silently produce the wrong key and look like a passphrase mismatch.
async fn peer_long_name(state: &AppState, peer_node_num: u32) -> Result<String, MeshGuardError> {
    let nodes = state.mesh_nodes.lock().await;
    let node = nodes
        .get(&peer_node_num)
        .ok_or(MeshGuardError::NodeNotFound(peer_node_num))?;
    if node.long_name.trim().is_empty() {
        return Err(MeshGuardError::Protocol(
            "This node has not announced its name yet, and the name is part of the \
             encryption key. Wait until it shows up with a name in the node list \
             (nodes announce themselves every few minutes), then try again."
                .into(),
        ));
    }
    Ok(node.long_name.clone())
}

/// Remember `key` for `peer_node_num` across restarts (ADR-002). Best
/// effort: without a platform key store, or on a write error, the chat
/// still works for this session.
async fn remember_key(
    app: &tauri::AppHandle,
    state: &AppState,
    my_num: u32,
    peer_node_num: u32,
    key: &crate::crypto::SessionKey,
) {
    let Some(vault) = state.vault(app).await else {
        return;
    };
    if let Err(e) = crate::key_store::save(&state.config_dir, &vault, my_num, peer_node_num, key) {
        tracing::warn!("Could not remember the chat key for {peer_node_num}: {e}");
    }
}

/// Persist (or replace) a peer. Re-pairing resets the protocol pin.
async fn save_peer(
    state: &AppState,
    peer_node_num: u32,
    peer_name: String,
    protocol: WireProtocol,
) -> Result<PeerConfig, MeshGuardError> {
    let peer = PeerConfig {
        id: uuid::Uuid::new_v4().to_string(),
        device_name: peer_name,
        node_num: peer_node_num,
        protocol,
    };
    let mut config = state.config.lock().await;
    config.peers.retain(|p| p.node_num != peer_node_num);
    config.peers.push(peer.clone());
    config.save(&state.config_dir)?;
    Ok(peer)
}

/// Start a chat with a mesh node. Derives key from passphrase,
/// sends an encrypted PairRequest, and stores the session key.
///
/// `protocol` = `"v2"` pairs in MeshGuard 2.0 mode (both peers must run 2.0);
/// omitted or `"v1"` keeps compatibility with older builds (ADR-003 §3).
#[tauri::command]
pub async fn start_chat(
    peer_node_num: u32,
    passphrase: String,
    protocol: Option<WireProtocol>,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<PeerConfig, MeshGuardError> {
    let protocol = protocol.unwrap_or_default();
    let (my_num, my_name) = my_identity(&state).await?;

    let peer_name = peer_long_name(&state, peer_node_num).await?;

    let session_key = crypto::derive_p2p_key(&my_name, &peer_name, &passphrase)?;

    let sealed = outbound::seal(
        protocol.into(),
        OutPayload::PairRequest(&my_name),
        &session_key,
        &PeerIdentity::meshtastic_node(my_num),
        &PeerIdentity::meshtastic_node(peer_node_num),
        unix_now(),
        DEFAULT_FRAME_BUDGET,
    )
    .map_err(seal_err)?;

    {
        let mut radio_guard = state.radio.lock().await;
        let radio = radio_guard.as_mut().ok_or(MeshGuardError::NotConnected)?;
        radio.send_private_app(sealed.frame, peer_node_num).await?;
    }

    let peer = save_peer(&state, peer_node_num, peer_name, protocol).await?;
    remember_key(&app_handle, &state, my_num, peer_node_num, &session_key).await;
    state
        .session_keys
        .lock()
        .await
        .insert(peer_node_num, session_key);
    tracing::info!("Chat initiated with node {peer_node_num} ({protocol:?})");
    Ok(peer)
}

/// Accept a pending pair request. The stored payload must be a PairRequest
/// that authenticates with the derived key (v1, or a fresh v2); its version
/// decides the reply and the pin.
#[tauri::command]
pub async fn accept_chat(
    peer_node_num: u32,
    passphrase: String,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<PeerConfig, MeshGuardError> {
    let (my_num, my_name) = my_identity(&state).await?;

    let peer_name = peer_long_name(&state, peer_node_num).await?;

    let session_key = crypto::derive_p2p_key(&my_name, &peer_name, &passphrase)?;
    let me = PeerIdentity::meshtastic_node(my_num);
    let peer_id = PeerIdentity::meshtastic_node(peer_node_num);

    // Keep the request until the accept is actually transmitted, so a failed
    // attempt can be retried without falling back to another version.
    let payload = state
        .pending_pair_requests
        .lock()
        .await
        .get(&peer_node_num)
        .cloned()
        .ok_or_else(|| MeshGuardError::Protocol("No pending chat request from this node".into()))?;

    let policy = state.inbound.policy;
    let now = u64::from(unix_now());
    let mut accepted_v2_request = None;
    let protocol = match decode_frame(&payload, Some(&session_key), &aad_context(&peer_id, &me)) {
        Ok(DecodedFrame::V1(V1Payload::PairRequest(_))) => WireProtocol::V1,
        Ok(DecodedFrame::V2(env)) if env.message_type == MessageType::PairRequest => {
            accepted_v2_request = Some(env.message_id);
            if env
                .check_freshness(now, policy.max_skew_secs, policy.max_ttl_mins)
                .is_err()
            {
                state
                    .pending_pair_requests
                    .lock()
                    .await
                    .remove(&peer_node_num);
                return Err(MeshGuardError::Protocol(
                    "Chat request expired — ask your peer to send it again".into(),
                ));
            }
            WireProtocol::V2
        }
        Ok(_) => {
            return Err(MeshGuardError::Protocol(
                "The pending message is not a chat request — open the chat and unlock it \
                 with the passphrase instead"
                    .into(),
            ))
        }
        Err(_) => return Err(MeshGuardError::PassphraseMismatch),
    };

    // Anti-downgrade (ADR-003 §3): a v2 pin is only lowered by removing the
    // peer first, never implicitly by a v1 request.
    let current_pin = state
        .config
        .lock()
        .await
        .peers
        .iter()
        .find(|p| p.node_num == peer_node_num)
        .map(|p| p.protocol);
    if current_pin == Some(WireProtocol::V2) && protocol == WireProtocol::V1 {
        return Err(MeshGuardError::Protocol(
            "This peer was paired in MeshGuard 2.0 mode. Remove the peer first to accept \
             a request from an older version"
                .into(),
        ));
    }

    let sealed = outbound::seal(
        protocol.into(),
        OutPayload::PairAccept(&my_name),
        &session_key,
        &me,
        &peer_id,
        unix_now(),
        DEFAULT_FRAME_BUDGET,
    )
    .map_err(seal_err)?;

    // Durably record an accepted v2 request *before* transmitting (and after
    // sealing, which can fail), so a replay
    // of it (also after a restart) can never trigger another acceptance.
    if let Some(id) = accepted_v2_request {
        let mut dedup = state.inbound.dedup.lock().await;
        match dedup.observe(&peer_id, id, now) {
            Seen::New => {}
            Seen::Duplicate { .. } => {
                drop(dedup);
                state
                    .pending_pair_requests
                    .lock()
                    .await
                    .remove(&peer_node_num);
                return Err(MeshGuardError::Protocol(
                    "This chat request was already accepted".into(),
                ));
            }
            Seen::Refused => {
                return Err(MeshGuardError::Protocol(
                    "Replay protection is full — try again later".into(),
                ))
            }
        }
        if let Err(e) = crate::dedup_store::save(&state.config_dir, &dedup) {
            dedup.forget(&peer_id, &id);
            return Err(e);
        }
    }

    let sent = {
        let mut radio_guard = state.radio.lock().await;
        match radio_guard.as_mut() {
            Some(radio) => radio.send_private_app(sealed.frame, peer_node_num).await,
            None => Err(MeshGuardError::NotConnected),
        }
    };
    if let Err(e) = sent {
        // Not accepted after all: let the user retry the same request.
        if let Some(id) = accepted_v2_request {
            let mut dedup = state.inbound.dedup.lock().await;
            dedup.forget(&peer_id, &id);
            let _ = crate::dedup_store::save(&state.config_dir, &dedup);
        }
        return Err(e);
    }

    state
        .pending_pair_requests
        .lock()
        .await
        .remove(&peer_node_num);
    let peer = save_peer(&state, peer_node_num, peer_name, protocol).await?;
    remember_key(&app_handle, &state, my_num, peer_node_num, &session_key).await;
    state
        .session_keys
        .lock()
        .await
        .insert(peer_node_num, session_key);
    tracing::info!("Chat accepted with node {peer_node_num} ({protocol:?})");
    Ok(peer)
}

// ── Messaging ─────────────────────────────────────────────────

/// Result of handing a message to the local radio. This is a *submission*,
/// not delivery: for v2 peers a later `message-delivered` event with the
/// same `message_id` confirms receipt by the peer app.
#[derive(Serialize)]
pub struct SendReceipt {
    /// Stable id (v2 only); `None` for v1 peers, which cannot confirm.
    pub message_id: Option<String>,
    pub protocol: WireProtocol,
}

/// Send an encrypted text message to a peer.
#[tauri::command]
pub async fn send_message(
    peer_node_num: u32,
    text: String,
    state: State<'_, AppState>,
) -> Result<SendReceipt, MeshGuardError> {
    let my_num = (*state.my_node_num.lock().await).ok_or(MeshGuardError::NotConnected)?;
    let protocol = state
        .config
        .lock()
        .await
        .peers
        .iter()
        .find(|p| p.node_num == peer_node_num)
        .map(|p| p.protocol)
        .unwrap_or_default();

    let sealed = {
        let keys = state.session_keys.lock().await;
        let session_key = keys.get(&peer_node_num).ok_or(MeshGuardError::NoSession)?;
        outbound::seal(
            protocol.into(),
            OutPayload::Text(&text),
            session_key,
            &PeerIdentity::meshtastic_node(my_num),
            &PeerIdentity::meshtastic_node(peer_node_num),
            unix_now(),
            DEFAULT_FRAME_BUDGET,
        )
        .map_err(seal_err)?
    };

    let mut radio_guard = state.radio.lock().await;
    let radio = radio_guard.as_mut().ok_or(MeshGuardError::NotConnected)?;
    radio.send_private_app(sealed.frame, peer_node_num).await?;

    Ok(SendReceipt {
        message_id: sealed.message_id.map(|id| id.to_hex()),
        protocol,
    })
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
pub async fn list_peers(state: State<'_, AppState>) -> Result<Vec<PeerConfig>, MeshGuardError> {
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
    crate::key_store::remove_peer(&state.config_dir, peer_node_num)?;

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

/// The saved radio to reconnect to, if any.
#[tauri::command]
pub async fn get_last_link(
    state: State<'_, AppState>,
) -> Result<Option<LinkTarget>, MeshGuardError> {
    Ok(state.config.lock().await.last_link.clone())
}

/// Connect to the saved radio in the background, retrying until it is
/// reachable or the user disconnects. Returns false when nothing is saved.
#[tauri::command]
pub async fn auto_connect(app_handle: tauri::AppHandle) -> Result<bool, MeshGuardError> {
    Ok(link::start_reconnect_loop(app_handle).await)
}
