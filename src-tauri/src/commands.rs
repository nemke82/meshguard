use serde::{Deserialize, Serialize};
use tauri::State;

use meshguard_core::codec::{decode_frame, DecodedFrame};
use meshguard_core::dedup::Seen;
use meshguard_core::envelope::{aad_context, MessageType, SealError, DEFAULT_FRAME_BUDGET};
use meshguard_core::outbound::{self, OutPayload};
use meshguard_core::v1::V1Payload;
use meshguard_core::PeerIdentity;

use crate::crypto;
use crate::device_config::{PeerConfig, StatusPreset, WireProtocol};
use crate::error::MeshGuardError;
use crate::link::{self, LinkTarget};
use crate::mesh_radio::{ScannedBleDevice, SerialPortInfo};
use crate::state::{AppState, MeshNodeInfo};
use meshguard_core::status::{GeoFix, StatusBody, StatusKind, FEATURE_STATUS};

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

pub(crate) fn unix_now() -> u32 {
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

/// Transmit `frame` as radio `my_num`. Fails if the radio changed since the
/// frame was sealed for `my_num` (the identity is part of the AAD).
pub(crate) async fn send_as(
    state: &AppState,
    my_num: u32,
    peer_node_num: u32,
    frame: Vec<u8>,
) -> Result<(), MeshGuardError> {
    let mut radio_guard = state.radio.lock().await;
    let radio = radio_guard.as_mut().ok_or(MeshGuardError::NotConnected)?;
    if *state.my_node_num.lock().await != Some(my_num) {
        return Err(MeshGuardError::Protocol(
            "The radio changed while sending — please try again".into(),
        ));
    }
    radio.send_private_app(frame, peer_node_num).await
}

fn key_copy(key: &crate::crypto::SessionKey) -> crate::crypto::SessionKey {
    crate::crypto::SessionKey::from_bytes(*key.to_bytes())
}

/// Install a chat key before transmitting, so the peer's immediate reply can
/// be decrypted. Returns the key it replaced, for rollback. Refuses if the
/// in-memory keys now belong to another radio than `my_num`.
async fn install_key(
    state: &AppState,
    my_num: u32,
    peer_node_num: u32,
    key: crate::crypto::SessionKey,
) -> Result<Option<crate::crypto::SessionKey>, MeshGuardError> {
    let owner = state.keys_owner.lock().await;
    if *owner != Some(my_num) {
        return Err(MeshGuardError::Protocol(
            "The radio changed — please try again".into(),
        ));
    }
    Ok(state.session_keys.lock().await.insert(peer_node_num, key))
}

/// Undo [`install_key`], unless the keys meanwhile belong to another radio.
async fn rollback_key(
    state: &AppState,
    my_num: u32,
    peer_node_num: u32,
    previous: Option<crate::crypto::SessionKey>,
) {
    let owner = state.keys_owner.lock().await;
    if *owner != Some(my_num) {
        return;
    }
    let mut keys = state.session_keys.lock().await;
    match previous {
        Some(k) => {
            keys.insert(peer_node_num, k);
        }
        None => {
            keys.remove(&peer_node_num);
        }
    }
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
    // true when we accepted the peer's request (it already holds the key).
    confirmed: bool,
) -> Result<PeerConfig, MeshGuardError> {
    let peer = PeerConfig {
        id: uuid::Uuid::new_v4().to_string(),
        device_name: peer_name,
        node_num: peer_node_num,
        protocol,
        // Re-pairing may mean a different app version: learn again.
        features: 0,
        confirmed,
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

    let previous = install_key(&state, my_num, peer_node_num, key_copy(&session_key)).await?;
    if let Err(e) = send_as(&state, my_num, peer_node_num, sealed.frame).await {
        rollback_key(&state, my_num, peer_node_num, previous).await;
        return Err(e);
    }

    let peer = save_peer(&state, peer_node_num, peer_name, protocol, false).await?;
    remember_key(&app_handle, &state, my_num, peer_node_num, &session_key).await;
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

    let previous = match install_key(&state, my_num, peer_node_num, key_copy(&session_key)).await {
        Ok(p) => p,
        Err(e) => {
            // Not accepted after all: un-record the request so it can be retried.
            if let Some(id) = accepted_v2_request {
                let mut dedup = state.inbound.dedup.lock().await;
                dedup.forget(&peer_id, &id);
                let _ = crate::dedup_store::save(&state.config_dir, &dedup);
            }
            return Err(e);
        }
    };
    let sent = send_as(&state, my_num, peer_node_num, sealed.frame).await;
    if let Err(e) = sent {
        rollback_key(&state, my_num, peer_node_num, previous).await;
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
    let peer = save_peer(&state, peer_node_num, peer_name, protocol, true).await?;
    remember_key(&app_handle, &state, my_num, peer_node_num, &session_key).await;
    if protocol == WireProtocol::V2 {
        crate::status_tx::probe_peers(&state).await;
    }
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

    send_as(&state, my_num, peer_node_num, sealed.frame).await?;

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

// ── Check-in, SOS and location ────────────────────────────────

const MAX_STATUS_PRESETS: usize = 8;

#[derive(Serialize)]
pub struct StatusSendResult {
    pub peer_node: u32,
    /// Stable id for v2 peers (matches their delivery ACK).
    pub message_id: Option<String>,
    pub protocol: WireProtocol,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct StatusSendReport {
    /// The position attached, if any (also shown in our own chat).
    pub location: Option<GeoFix>,
    pub results: Vec<StatusSendResult>,
}

/// The user's one-tap check-in buttons.
#[tauri::command]
pub async fn get_status_presets(
    state: State<'_, AppState>,
) -> Result<Vec<StatusPreset>, MeshGuardError> {
    Ok(state.config.lock().await.status_presets.clone())
}

/// Replace the check-in buttons (wording is free; length is checked so every
/// preset fits one LoRa frame, with a location attached).
#[tauri::command]
pub async fn set_status_presets(
    presets: Vec<StatusPreset>,
    state: State<'_, AppState>,
) -> Result<Vec<StatusPreset>, MeshGuardError> {
    if presets.is_empty() || presets.len() > MAX_STATUS_PRESETS {
        return Err(MeshGuardError::Protocol(format!(
            "Use between 1 and {MAX_STATUS_PRESETS} quick messages"
        )));
    }
    let max = outbound::max_status_text_bytes(true, DEFAULT_FRAME_BUDGET);
    let mut cleaned = Vec::with_capacity(presets.len());
    for p in presets {
        let text = p.text.trim().to_string();
        if text.is_empty() {
            return Err(MeshGuardError::Protocol(
                "Quick messages cannot be empty".into(),
            ));
        }
        if text.len() > max {
            return Err(MeshGuardError::Protocol(format!(
                "\"{text}\" is too long ({} bytes, limit {max})",
                text.len()
            )));
        }
        cleaned.push(StatusPreset { text, ..p });
    }
    let mut config = state.config.lock().await;
    config.status_presets = cleaned;
    config.save(&state.config_dir)?;
    Ok(config.status_presets.clone())
}

/// Restore the built-in English check-in buttons.
#[tauri::command]
pub async fn reset_status_presets(
    state: State<'_, AppState>,
) -> Result<Vec<StatusPreset>, MeshGuardError> {
    let mut config = state.config.lock().await;
    config.status_presets = crate::device_config::default_status_presets();
    config.save(&state.config_dir)?;
    Ok(config.status_presets.clone())
}

/// Latest GPS fix of our radio, if it has one.
#[tauri::command]
pub async fn get_my_position(state: State<'_, AppState>) -> Result<Option<GeoFix>, MeshGuardError> {
    Ok(*state.my_position.lock().await)
}

/// Send a check-in, SOS or location share to one or more paired peers.
/// SOS always attaches the radio's position when one is known.
#[tauri::command]
pub async fn send_status(
    peer_node_nums: Vec<u32>,
    kind: StatusKind,
    text: String,
    include_location: bool,
    state: State<'_, AppState>,
) -> Result<StatusSendReport, MeshGuardError> {
    let mut peers = peer_node_nums;
    peers.sort_unstable();
    peers.dedup();
    if peers.is_empty() {
        return Err(MeshGuardError::Protocol("No recipients".into()));
    }
    let text = text.trim().to_string();
    if text.is_empty() && kind != StatusKind::Location {
        return Err(MeshGuardError::Protocol("The message is empty".into()));
    }

    let my_num = (*state.my_node_num.lock().await).ok_or(MeshGuardError::NotConnected)?;
    let location = if include_location || kind == StatusKind::Sos {
        *state.my_position.lock().await
    } else {
        None
    };
    if kind == StatusKind::Location && location.is_none() {
        return Err(MeshGuardError::Protocol(
            "Your radio has no GPS position yet — take it outside for a fix".into(),
        ));
    }
    let body = StatusBody {
        kind,
        location,
        text,
    };

    let urgent = kind.is_urgent();

    // One transmit path for all check-ins: spacing + airtime budget across
    // concurrent requests (see status_tx).
    let mut tx = state.status_tx.lock().await;
    let mut results = Vec::with_capacity(peers.len());
    for peer in peers {
        let (protocol, capable) = state
            .config
            .lock()
            .await
            .peers
            .iter()
            .find(|p| p.node_num == peer)
            .map(|p| (p.protocol, p.features & FEATURE_STATUS != 0))
            .unwrap_or_default();
        // v1 peers get the fallback inside `seal`; v2 peers on an older
        // build would drop a Status frame, so they get v2 text until they
        // are known to understand it.
        let payload = if protocol == WireProtocol::V2 && !capable {
            OutPayload::StatusAsText(&body)
        } else {
            OutPayload::Status(&body)
        };
        let sealed = {
            let keys = state.session_keys.lock().await;
            keys.get(&peer)
                .ok_or(MeshGuardError::NoSession)
                .and_then(|key| {
                    outbound::seal(
                        protocol.into(),
                        payload,
                        key,
                        &PeerIdentity::meshtastic_node(my_num),
                        &PeerIdentity::meshtastic_node(peer),
                        unix_now(),
                        DEFAULT_FRAME_BUDGET,
                    )
                    .map_err(seal_err)
                })
        };
        let outcome = match sealed {
            Ok(s) => {
                if tx.admit(urgent).await {
                    let r = send_as(&state, my_num, peer, s.frame)
                        .await
                        .map(|()| s.message_id);
                    if r.is_ok() {
                        tx.mark_sent();
                    }
                    r
                } else {
                    Err(MeshGuardError::Protocol(
                        "Airtime budget reached — wait a minute and try again".into(),
                    ))
                }
            }
            Err(e) => Err(e),
        };
        tracing::info!(
            "Status {kind:?} to {peer} ({}): {}",
            if matches!(payload, OutPayload::Status(_)) {
                "status"
            } else {
                "text fallback"
            },
            if outcome.is_ok() { "sent" } else { "failed" }
        );
        results.push(match outcome {
            Ok(id) => StatusSendResult {
                peer_node: peer,
                message_id: id.map(|id| id.to_hex()),
                protocol,
                error: None,
            },
            Err(e) => StatusSendResult {
                peer_node: peer,
                message_id: None,
                protocol,
                error: Some(e.to_string()),
            },
        });
    }
    Ok(StatusSendReport { location, results })
}

// ── Diagnostics ───────────────────────────────────────────────

/// Signal history of a node (RSSI/SNR of direct packets, hops, battery).
#[tauri::command]
pub async fn get_node_history(
    node_num: u32,
    state: State<'_, AppState>,
) -> Result<Vec<crate::diagnostics::SignalSample>, MeshGuardError> {
    Ok(state.node_history.lock().await.get(node_num))
}

/// Start a path test (Meshtastic traceroute) to `node_num`. The result
/// arrives as a `path-test-result` event; no event means no reply within
/// the timeout (the UI shows that).
#[tauri::command]
pub async fn run_path_test(
    node_num: u32,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let my_num = (*state.my_node_num.lock().await).ok_or(MeshGuardError::NotConnected)?;
    if node_num == my_num {
        return Err(MeshGuardError::Protocol("That is your own radio".into()));
    }
    if let Err(wait) = state.path_tests.lock().await.try_start(node_num) {
        return Err(MeshGuardError::Protocol(format!(
            "Please wait {} s before testing this node again",
            wait.as_secs().max(1)
        )));
    }
    // Shares the check-in transmit path: spacing and airtime budget.
    let mut tx = state.status_tx.lock().await;
    if !tx.admit(false).await {
        state.path_tests.lock().await.forget(node_num);
        return Err(MeshGuardError::Protocol(
            "Airtime budget reached — wait a minute and try again".into(),
        ));
    }
    let sent = {
        let mut radio = state.radio.lock().await;
        match radio.as_mut() {
            Some(r) => r.send_traceroute(node_num).await,
            None => Err(MeshGuardError::NotConnected),
        }
    };
    match sent {
        Ok(request_id) => {
            tx.mark_sent();
            state.path_tests.lock().await.sent(node_num, request_id);
            Ok(())
        }
        Err(e) => {
            state.path_tests.lock().await.forget(node_num);
            Err(e)
        }
    }
}
