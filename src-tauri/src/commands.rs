use serde::{Deserialize, Serialize};
use tauri::State;

use meshguard_core::codec::{decode_frame, DecodedFrame};
use meshguard_core::dedup::Seen;
use meshguard_core::envelope::{aad_context, MessageType, DEFAULT_FRAME_BUDGET};
use meshguard_core::outbound::{self, OutPayload};
use meshguard_core::v1::V1Payload;
use meshguard_core::PeerIdentity;

use crate::crypto;
use crate::device_config::{BlockedNode, PeerConfig, StatusPreset, WireProtocol};
use crate::error::MeshGuardError;
use crate::link::{self, LinkTarget};
use crate::mesh_radio::{ScannedBleDevice, SerialPortInfo};
use crate::state::{AppState, MeshNodeInfo};
use meshguard_core::status::{GeoFix, StatusBody, StatusKind};

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

/// Look for Meshtastic radios on the current Wi-Fi network (mDNS).
#[tauri::command]
pub async fn scan_wifi_radios(
    app_handle: tauri::AppHandle,
) -> Result<Vec<crate::discovery::WifiRadio>, MeshGuardError> {
    crate::discovery::scan(&app_handle, crate::discovery::SCAN_TIME).await
}

/// Radios connected to before, newest first.
#[tauri::command]
pub async fn get_saved_links(
    state: State<'_, AppState>,
) -> Result<Vec<crate::device_config::SavedLink>, MeshGuardError> {
    Ok(state.engine().config.saved_links.clone())
}

/// Forget a saved radio (and stop auto-connecting to it).
#[tauri::command]
pub async fn forget_saved_link(
    id: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let mut engine = state.engine();
    let config = &mut engine.config;
    if crate::device_config::forget_link(config, &id) {
        config.save(&state.config_dir)?;
    }
    Ok(())
}

/// Connect to a saved radio (see `link::connect_saved`).
#[tauri::command]
pub async fn connect_saved_link(
    id: String,
    pin: Option<u32>,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let (target, node_num) = state
        .engine()
        .config
        .saved_links
        .iter()
        .find(|l| l.id == id)
        .map(|l| (l.target.clone(), l.node_num))
        .ok_or_else(|| MeshGuardError::Io("This saved radio no longer exists".into()))?;
    link::connect_saved(target, node_num, pin, &app_handle, &state).await
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
    let engine = state.engine();
    let nodes = &engine.nodes;
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
    let num = state.engine().my_node_num;
    let name = state.engine().my_device_name.clone();
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

pub(crate) use meshguard_engine::outbox::seal_err;

async fn my_identity(state: &AppState) -> Result<(u32, String), MeshGuardError> {
    let num = state
        .engine()
        .my_node_num
        .ok_or(MeshGuardError::NotConnected)?;
    let name = state
        .engine()
        .my_device_name
        .clone()
        .ok_or(MeshGuardError::NotConnected)?;
    Ok((num, name))
}

/// The peer's Meshtastic long name, which is part of the v1/v2 key
/// derivation. Refuses when it is not known yet: deriving with a placeholder
/// would silently produce the wrong key and look like a passphrase mismatch.
async fn peer_long_name(state: &AppState, peer_node_num: u32) -> Result<String, MeshGuardError> {
    let engine = state.engine();
    let nodes = &engine.nodes;
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
    if state.engine().my_node_num != Some(my_num) {
        return Err(MeshGuardError::Protocol(
            "The radio changed while sending — please try again".into(),
        ));
    }
    radio.send_private_app(frame, peer_node_num).await
}

/// Transmit an outbox message. The radio lock is the gate shared with
/// cancellation (block, remove): the generation is checked under it and
/// held through the hand-over, so nothing cancelled is sent afterwards.
pub(crate) async fn send_transmit(
    state: &AppState,
    t: &meshguard_engine::outbox::Transmit,
) -> Result<(), MeshGuardError> {
    let mut radio_guard = state.radio.lock().await;
    let radio = radio_guard.as_mut().ok_or(MeshGuardError::NotConnected)?;
    {
        let engine = state.engine();
        if !engine.still_sendable(t) {
            // Cancelled meanwhile; the row is gone, nothing to record.
            return Err(MeshGuardError::NotConnected);
        }
        if engine.my_node_num != Some(t.my_node) {
            return Err(MeshGuardError::Protocol(
                "The radio changed while sending — please try again".into(),
            ));
        }
    }
    radio.send_private_app(t.frame.clone(), t.to).await
}

/// Transmit a relay control message over the radio. Like
/// [`send_transmit`], under the radio gate: the control is checked to be
/// still pending (not cancelled by a block or removal) and its attempt is
/// reserved and saved before the hand-over.
pub(crate) async fn send_control(
    state: &AppState,
    c: &meshguard_engine::relay::ControlTransmit,
    now: u64,
) -> Result<(), MeshGuardError> {
    let mut radio_guard = state.radio.lock().await;
    let radio = radio_guard.as_mut().ok_or(MeshGuardError::NotConnected)?;
    {
        let mut engine = state.engine();
        if engine.my_node_num != Some(c.my_node) {
            return Err(MeshGuardError::NotConnected);
        }
        if !engine.relay_reserve_control(c, now) {
            return Ok(());
        }
    }
    radio.send_private_app(c.frame.clone(), c.peer).await
}

fn key_copy(key: &crate::crypto::SessionKey) -> crate::crypto::SessionKey {
    crate::crypto::SessionKey::from_bytes(*key.to_bytes())
}

/// Install a chat key before transmitting, so the peer's immediate reply can
/// be decrypted. Returns the key it replaced, for rollback. Refuses if the
/// in-memory keys now belong to another radio than `my_num`.
fn install_key(
    state: &AppState,
    my_num: u32,
    peer_node_num: u32,
    key: crate::crypto::SessionKey,
) -> Result<Option<crate::crypto::SessionKey>, MeshGuardError> {
    state.engine().install_key(my_num, peer_node_num, key)
}

/// Undo [`install_key`], unless the keys meanwhile belong to another radio.
fn rollback_key(
    state: &AppState,
    my_num: u32,
    peer_node_num: u32,
    previous: Option<crate::crypto::SessionKey>,
) {
    state.engine().rollback_key(my_num, peer_node_num, previous);
}

/// Serialises pairing operations (passphrase, QR scan, QR accept), so two
/// attempts for the same peer cannot interleave key installs and rollbacks.
static PAIRING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let mut engine = state.engine();
    let config = &mut engine.config;
    let before = config.peers.clone();
    config.peers.retain(|p| p.node_num != peer_node_num);
    config.peers.push(peer.clone());
    if let Err(e) = config.save(&state.config_dir) {
        // Transactional: an unsaved change must not linger in memory.
        config.peers = before;
        return Err(e);
    }
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
    let _pairing = PAIRING.lock().await;
    refuse_blocked(&state, peer_node_num)?;
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

    let previous = install_key(&state, my_num, peer_node_num, key_copy(&session_key))?;
    if let Err(e) = send_as(&state, my_num, peer_node_num, sealed.frame).await {
        rollback_key(&state, my_num, peer_node_num, previous);
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
    let _pairing = PAIRING.lock().await;
    refuse_blocked(&state, peer_node_num)?;
    let (my_num, my_name) = my_identity(&state).await?;

    let peer_name = peer_long_name(&state, peer_node_num).await?;

    let session_key = crypto::derive_p2p_key(&my_name, &peer_name, &passphrase)?;
    let me = PeerIdentity::meshtastic_node(my_num);
    let peer_id = PeerIdentity::meshtastic_node(peer_node_num);

    // Keep the request until the accept is actually transmitted, so a failed
    // attempt can be retried without falling back to another version.
    let payload = state
        .engine()
        .pending_pair_requests
        .get(&peer_node_num)
        .cloned()
        .ok_or_else(|| MeshGuardError::Protocol("No pending chat request from this node".into()))?;

    let policy = state.engine().protection.policy;
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
                state.engine().pending_pair_requests.remove(&peer_node_num);
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
        .engine()
        .config
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
        let mut engine = state.engine();
        match engine.protection.dedup.observe(&peer_id, id, now) {
            Seen::New => {}
            Seen::Duplicate { .. } => {
                engine.pending_pair_requests.remove(&peer_node_num);
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
        if let Err(e) = engine.save_dedup() {
            engine.protection.dedup.forget(&peer_id, &id);
            return Err(e);
        }
    }

    let previous = match install_key(&state, my_num, peer_node_num, key_copy(&session_key)) {
        Ok(p) => p,
        Err(e) => {
            // Not accepted after all: un-record the request so it can be retried.
            if let Some(id) = accepted_v2_request {
                forget_dedup(&state, &peer_id, &id);
            }
            return Err(e);
        }
    };
    let sent = send_as(&state, my_num, peer_node_num, sealed.frame).await;
    if let Err(e) = sent {
        rollback_key(&state, my_num, peer_node_num, previous);
        // Not accepted after all: let the user retry the same request.
        if let Some(id) = accepted_v2_request {
            forget_dedup(&state, &peer_id, &id);
        }
        return Err(e);
    }

    state.engine().pending_pair_requests.remove(&peer_node_num);
    let peer = save_peer(&state, peer_node_num, peer_name, protocol, true).await?;
    remember_key(&app_handle, &state, my_num, peer_node_num, &session_key).await;
    if protocol == WireProtocol::V2 {
        crate::status_tx::probe_peers(&state).await;
    }
    tracing::info!("Chat accepted with node {peer_node_num} ({protocol:?})");
    Ok(peer)
}

// ── Messaging ─────────────────────────────────────────────────

/// Result of sending a message. Even `submitted` is not delivery: for v2
/// peers a later `message-state` / `message-delivered` event with the same
/// `message_id` confirms receipt by the peer app.
#[derive(Serialize)]
pub struct SendReceipt {
    /// Stable id: on the wire for v2 peers, local only for v1 peers.
    pub message_id: Option<String>,
    pub protocol: WireProtocol,
    /// `queued` (waiting for the radio), `submitted`, `retrying`, ...
    pub state: String,
    /// Saved in the history store (and retried from the outbox).
    pub stored: bool,
    /// "relay" when it went to the MQTT broker (sent to broker).
    pub via: Option<&'static str>,
}

/// Send an encrypted text message to a peer. With history on, it is saved
/// first and waits in the outbox while the radio is away.
#[tauri::command]
pub async fn send_message(
    peer_node_num: u32,
    text: String,
    app_handle: tauri::AppHandle,
) -> Result<SendReceipt, MeshGuardError> {
    let r = crate::history::send(
        &app_handle,
        peer_node_num,
        crate::history::Content::Text(text),
    )
    .await?;
    Ok(SendReceipt {
        message_id: Some(r.message_id.to_hex()),
        protocol: r.protocol,
        state: r.state.to_string(),
        stored: r.stored,
        via: r.via,
    })
}

/// Check if we have a session key for a peer.
#[tauri::command]
pub async fn has_session(
    peer_node_num: u32,
    state: State<'_, AppState>,
) -> Result<bool, MeshGuardError> {
    Ok(state.engine().keys.contains_key(&peer_node_num))
}

/// List saved peers from config.
#[tauri::command]
pub async fn list_peers(state: State<'_, AppState>) -> Result<Vec<PeerConfig>, MeshGuardError> {
    let engine = state.engine();
    let config = &engine.config;
    Ok(config.peers.clone())
}

/// Remove a saved peer.
#[tauri::command]
pub async fn remove_peer(
    peer_node_num: u32,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), MeshGuardError> {
    crate::history::ensure_open(&app_handle).await;
    // The radio lock and the relay send gate are shared with outbox sends:
    // nothing sealed before this point is handed over afterwards.
    let _gate = state.radio.lock().await;
    let _relay_gate = state.relay.send_gate.lock().await;
    crate::key_store::remove_peer(&state.config_dir, peer_node_num)?;
    // One engine critical section: no message can be queued for the peer
    // between the cancellation and the removal of its key.
    let cancelled = {
        let mut engine = state.engine();
        let prev_config = engine.config.clone();
        let prev_key = engine.keys.remove(&peer_node_num);
        // Nothing queued for a removed contact is sent later (e.g. to
        // whoever pairs as this node next). The raised send generation is
        // saved with the removal.
        let cancelled = engine.cancel_sends_to(peer_node_num);
        // The relay with a removed contact ends too, without telling it.
        engine.relay_forget(peer_node_num, unix_now().into());
        engine.config.peers.retain(|p| p.node_num != peer_node_num);
        if let Err(e) = engine.config.save(&state.config_dir) {
            engine.config = prev_config;
            if let Some(k) = prev_key {
                engine.keys.insert(peer_node_num, k);
            }
            return Err(e);
        }
        cancelled
    };
    for e in cancelled {
        crate::history::emit_state(&app_handle, Some(e));
    }
    Ok(())
}

// ── Blocking and terms of use (Play UGC policy) ───────────────

fn refuse_blocked(state: &AppState, node: u32) -> Result<(), MeshGuardError> {
    if state.engine().config.is_blocked(node) {
        return Err(MeshGuardError::Protocol(
            "This node is blocked. Unblock it under Settings → Blocked nodes first.".into(),
        ));
    }
    Ok(())
}

/// Block a node: remove the contact and its remembered key, and ignore its
/// messages, chat requests and check-ins. Message history stays until the
/// user deletes it. Returns the blocked list.
#[tauri::command]
pub async fn block_node(
    node_num: u32,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<Vec<BlockedNode>, MeshGuardError> {
    // Serialized with pairing: a pairing in progress cannot re-add the
    // contact or its key after the block.
    let _pairing = PAIRING.lock().await;
    crate::history::ensure_open(&app_handle).await;
    // Gates shared with outbox sends (see send_transmit, relay::publish).
    let _gate = state.radio.lock().await;
    let _relay_gate = state.relay.send_gate.lock().await;
    let (blocked, cancelled) = {
        let mut engine = state.engine();
        let name = engine
            .peer_title(node_num)
            .or_else(|| engine.node_name(node_num))
            .unwrap_or_else(|| format!("!{node_num:08x}"));
        // Restored if the block cannot be saved.
        let prev_config = engine.config.clone();
        let prev_key = engine.keys.get(&node_num).map(key_copy);
        let prev_request = engine.pending_pair_requests.get(&node_num).cloned();
        engine.block_node(node_num, name, chrono::Utc::now().timestamp());
        // Same critical section: nothing queued for the node is sent any
        // more; the raised send generation is saved with the block.
        let cancelled = engine.cancel_sends_to(node_num);
        engine.relay_forget(node_num, unix_now().into());
        if let Err(e) = engine.config.save(&state.config_dir) {
            engine.config = prev_config;
            if let Some(key) = prev_key {
                engine.keys.insert(node_num, key);
            }
            if let Some(req) = prev_request {
                engine.pending_pair_requests.insert(node_num, req);
            }
            return Err(e);
        }
        (engine.config.blocked.clone(), cancelled)
    };
    for e in cancelled {
        crate::history::emit_state(&app_handle, Some(e));
    }
    // The block is saved; a remembered key left behind is never used for a
    // blocked node, so this is not an error for the user.
    if let Err(e) = crate::key_store::remove_peer(&state.config_dir, node_num) {
        tracing::warn!("Blocked node {node_num}, but its remembered key was not removed: {e}");
    }
    tracing::info!("Blocked node {node_num}");
    Ok(blocked)
}

/// Unblock a node. Returns the blocked list.
#[tauri::command]
pub async fn unblock_node(
    node_num: u32,
    state: State<'_, AppState>,
) -> Result<Vec<BlockedNode>, MeshGuardError> {
    let mut engine = state.engine();
    let prev = engine.config.blocked.clone();
    if engine.unblock_node(node_num) {
        if let Err(e) = engine.config.save(&state.config_dir) {
            engine.config.blocked = prev; // still blocked, as the UI shows
            return Err(e);
        }
    }
    Ok(engine.config.blocked.clone())
}

#[tauri::command]
pub async fn list_blocked(state: State<'_, AppState>) -> Result<Vec<BlockedNode>, MeshGuardError> {
    Ok(state.engine().config.blocked.clone())
}

/// Whether the current terms of use were accepted.
#[tauri::command]
pub async fn terms_accepted(state: State<'_, AppState>) -> Result<bool, MeshGuardError> {
    Ok(state.engine().config.terms_current())
}

#[tauri::command]
pub async fn accept_terms(state: State<'_, AppState>) -> Result<(), MeshGuardError> {
    let mut engine = state.engine();
    let prev = engine.config.terms_accepted;
    engine.config.terms_accepted = crate::device_config::TERMS_VERSION;
    if let Err(e) = engine.config.save(&state.config_dir) {
        engine.config.terms_accepted = prev;
        return Err(e);
    }
    Ok(())
}

/// Get the last connected BLE device name (for auto-reconnect).
#[tauri::command]
pub async fn get_last_connection(
    state: State<'_, AppState>,
) -> Result<Option<String>, MeshGuardError> {
    let engine = state.engine();
    let config = &engine.config;
    Ok(config.last_ble_address.clone())
}

/// The saved radio to reconnect to, if any.
#[tauri::command]
pub async fn get_last_link(
    state: State<'_, AppState>,
) -> Result<Option<LinkTarget>, MeshGuardError> {
    Ok(state.engine().config.last_link.clone())
}

/// Connect to the saved radio in the background, retrying until it is
/// reachable or the user disconnects. Returns false when nothing is saved.
#[tauri::command]
pub async fn auto_connect(app_handle: tauri::AppHandle) -> Result<bool, MeshGuardError> {
    Ok(link::start_reconnect_loop(app_handle).await)
}

/// Android Back on the app's first screen: send the app to the background
/// like the Home button. The activity stays alive, so returning is instant
/// and keeps the radio link. (Without a Back handler Tauri finishes the
/// activity, which looked like the app exiting.) No-op on desktop.
#[tauri::command]
pub async fn move_to_background(
    #[allow(unused_variables)] app_handle: tauri::AppHandle,
) -> Result<(), MeshGuardError> {
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let state = app_handle
            .try_state::<crate::ble_plugin::BlePluginState<tauri::Wry>>()
            .ok_or_else(|| MeshGuardError::Ble("Android plugin not initialized".into()))?;
        state
            .0
            .run_mobile_plugin::<serde_json::Value>("moveToBackground", ())
            .map_err(|e| MeshGuardError::Protocol(e.to_string()))?;
    }
    Ok(())
}

/// Start or stop the Android background service (ADR-001 §6 gate test).
#[tauri::command]
pub async fn set_background_service(
    on: bool,
    #[allow(unused_variables)] app_handle: tauri::AppHandle,
) -> Result<(), MeshGuardError> {
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let state = app_handle
            .try_state::<crate::ble_plugin::BlePluginState<tauri::Wry>>()
            .ok_or_else(|| MeshGuardError::Ble("Android plugin not initialized".into()))?;
        let cmd = if on {
            "startBackgroundService"
        } else {
            "stopBackgroundService"
        };
        state
            .0
            .run_mobile_plugin::<serde_json::Value>(cmd, ())
            .map_err(|e| MeshGuardError::Protocol(e.to_string()))?;
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = on;
        Err(MeshGuardError::Protocol(
            "The background service is only available on Android".into(),
        ))
    }
}

// ── Check-in, SOS and location ────────────────────────────────

const MAX_STATUS_PRESETS: usize = 8;

#[derive(Serialize)]
pub struct StatusSendResult {
    pub peer_node: u32,
    /// Stable id for v2 peers (matches their delivery ACK).
    pub message_id: Option<String>,
    pub protocol: WireProtocol,
    /// Delivery state when sent or queued (see `SendReceipt::state`).
    pub state: Option<String>,
    pub error: Option<String>,
    /// "relay" when it went to the MQTT broker.
    pub via: Option<&'static str>,
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
    Ok(state.engine().config.status_presets.clone())
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
    let mut engine = state.engine();
    let config = &mut engine.config;
    config.status_presets = cleaned;
    config.save(&state.config_dir)?;
    Ok(config.status_presets.clone())
}

/// Restore the built-in English check-in buttons.
#[tauri::command]
pub async fn reset_status_presets(
    state: State<'_, AppState>,
) -> Result<Vec<StatusPreset>, MeshGuardError> {
    let mut engine = state.engine();
    let config = &mut engine.config;
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
    app_handle: tauri::AppHandle,
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

    // No radio check here: with history on, a check-in or SOS waits in the
    // outbox until the radio is back (history::send decides).
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
        let protocol = state
            .engine()
            .config
            .peers
            .iter()
            .find(|p| p.node_num == peer)
            .map(|p| p.protocol)
            .unwrap_or_default();
        let outcome = if tx.admit(urgent).await {
            let r = crate::history::send(
                &app_handle,
                peer,
                crate::history::Content::Status(body.clone()),
            )
            .await;
            if r.as_ref().is_ok_and(|o| o.state == "submitted") {
                tx.mark_sent();
            }
            r
        } else {
            Err(MeshGuardError::Protocol(
                "Airtime budget reached — wait a minute and try again".into(),
            ))
        };
        tracing::info!(
            "Status {kind:?} to {peer}: {}",
            match &outcome {
                Ok(o) => o.state,
                Err(_) => "failed",
            }
        );
        results.push(match outcome {
            Ok(o) => StatusSendResult {
                peer_node: peer,
                message_id: Some(o.message_id.to_hex()),
                protocol: o.protocol,
                state: Some(o.state.to_string()),
                via: o.via,
                error: None,
            },
            Err(e) => StatusSendResult {
                via: None,
                peer_node: peer,
                message_id: None,
                protocol,
                state: None,
                error: Some(e.to_string()),
            },
        });
    }
    Ok(StatusSendReport { location, results })
}

// ── Offline QR pairing (ADR-004) ──────────────────────────────

/// A QR pairing offer is valid this long (and can be scanned by several
/// family members meanwhile; each gets an independent key).
const PAIR_OFFER_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The offer this device is currently showing.
pub struct PairOffer {
    offer: meshguard_core::qrpair::PairOffer,
    created: std::time::Instant,
}

#[derive(Serialize)]
pub struct PairOfferView {
    /// The text in the QR code; can also be typed on the other device.
    pub uri: String,
    /// The QR code as SVG markup (generated locally).
    pub svg: String,
    pub expires_in_secs: u64,
}

/// Show a pairing QR code. Devices that scan it within 10 minutes and send
/// the resulting pair request are accepted automatically.
#[tauri::command]
pub async fn create_pair_offer(
    state: State<'_, AppState>,
) -> Result<PairOfferView, MeshGuardError> {
    let my_num = state
        .engine()
        .my_node_num
        .ok_or(MeshGuardError::NotConnected)?;
    let offer = meshguard_core::qrpair::PairOffer::generate(my_num);
    let uri = offer.code().to_uri();
    let svg = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)
        .map_err(|e| MeshGuardError::Protocol(format!("QR code: {e}")))?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    *state.pair_offer.lock().await = Some(PairOffer {
        offer,
        created: std::time::Instant::now(),
    });
    tracing::info!("Showing a QR pairing offer");
    Ok(PairOfferView {
        uri,
        svg,
        expires_in_secs: PAIR_OFFER_TTL.as_secs(),
    })
}

/// Stop accepting the current QR code. Waits for an acceptance in progress.
#[tauri::command]
pub async fn cancel_pair_offer(state: State<'_, AppState>) -> Result<(), MeshGuardError> {
    *state.pair_offer.lock().await = None;
    Ok(())
}

/// Pair using a scanned (or typed) code from another device: run our half
/// of the handshake and send the pair request. The other device accepts it
/// automatically while it shows the code.
#[tauri::command]
pub async fn pair_with_code(
    code: String,
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<PeerConfig, MeshGuardError> {
    let _pairing = PAIRING.lock().await;
    let code = meshguard_core::qrpair::PairCode::parse(&code)?;
    refuse_blocked(&state, code.node)?;
    let (my_num, my_name) = my_identity(&state).await?;
    if code.node == my_num {
        return Err(MeshGuardError::Protocol(
            "That is this radio's own code — scan it with the other phone".into(),
        ));
    }
    let peer = code.node;
    // The showing device refuses known contacts (ADR-004), so a rescan would
    // only replace our working key with one the other side never accepts.
    if state
        .engine()
        .config
        .peers
        .iter()
        .any(|p| p.node_num == peer)
    {
        return Err(MeshGuardError::Protocol(
            "This contact is already paired. Remove it on both phones first to pair again with a code."
                .into(),
        ));
    }
    let hs = meshguard_core::qrpair::scan(&code, my_num, &my_name)?;
    let sealed = outbound::seal(
        WireProtocol::V2.into(),
        OutPayload::PairRequest(&hs.request_body),
        &hs.request_key,
        &PeerIdentity::meshtastic_node(my_num),
        &PeerIdentity::meshtastic_node(peer),
        unix_now(),
        DEFAULT_FRAME_BUDGET,
    )
    .map_err(seal_err)?;
    // The accept is sealed with the chat key: install it before sending.
    let previous = install_key(&state, my_num, peer, key_copy(&hs.chat_key))?;
    let previous_copy = previous.as_ref().map(key_copy);
    if let Err(e) = send_as(&state, my_num, peer, sealed.frame).await {
        rollback_key(&state, my_num, peer, previous);
        return Err(e);
    }
    let name = state
        .engine()
        .nodes
        .get(&peer)
        .map(|n| n.long_name.clone())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| format!("!{peer:08x}"));
    // install_key's `previous` is consumed by the send-failure path above,
    // so keep a copy of what was there for a save failure here.
    let saved = match save_peer(&state, peer, name, WireProtocol::V2, false).await {
        Ok(p) => p,
        Err(e) => {
            rollback_key(&state, my_num, peer, previous_copy);
            return Err(e);
        }
    };
    remember_key(&app_handle, &state, my_num, peer, &hs.chat_key).await;
    tracing::info!("QR pair request sent to {peer}");
    Ok(saved)
}

/// Undo a durable replay record for a request that was not accepted after all.
fn forget_dedup(state: &AppState, peer: &PeerIdentity, id: &meshguard_core::MessageId) {
    let mut engine = state.engine();
    engine.protection.dedup.forget(peer, id);
    let _ = engine.save_dedup();
}

#[derive(Clone, Serialize)]
pub struct PeerPairedEvent {
    pub node_num: u32,
    pub name: String,
}

/// Called by the listener for an undecryptable frame: if we are showing a
/// QR code and the frame is a fresh v2 pair request sealed with its request
/// key, finish the handshake and accept automatically. Returns true when
/// the frame belonged to the offer (handled or deliberately ignored).
///
/// Holds the offer lock throughout, so *Done* (cancel) cannot interleave
/// with an acceptance.
pub(crate) async fn try_accept_qr(app: &tauri::AppHandle, from_node: u32, frame: &[u8]) -> bool {
    let _pairing = PAIRING.lock().await;
    use tauri::{Emitter, Manager};
    let state = app.state::<AppState>();
    let mut slot = state.pair_offer.lock().await;
    let Some(offer) = slot.as_ref() else {
        return false;
    };
    if offer.created.elapsed() >= PAIR_OFFER_TTL {
        *slot = None;
        return false;
    }
    let Ok((my_num, my_name)) = my_identity(&state).await else {
        return false;
    };
    if offer.offer.code().node != my_num || from_node == my_num {
        return false; // offer from another radio, or a loop
    }
    // Blocked after this frame was received (the listener filters earlier
    // ones): ignore it.
    if state.engine().config.is_blocked(from_node) {
        return true;
    }
    let Ok(request_key) = offer.offer.code().request_key(from_node) else {
        return false;
    };
    let me = PeerIdentity::meshtastic_node(my_num);
    let peer_id = PeerIdentity::meshtastic_node(from_node);
    let Ok(DecodedFrame::V2(env)) =
        decode_frame(frame, Some(&request_key), &aad_context(&peer_id, &me))
    else {
        return false;
    };
    if env.message_type != MessageType::PairRequest {
        return false;
    }
    let policy = state.engine().protection.policy;
    let now = u64::from(unix_now());
    if env
        .check_freshness(now, policy.max_skew_secs, policy.max_ttl_mins)
        .is_err()
    {
        tracing::info!("Stale QR pair request from {from_node} ignored");
        return true;
    }
    let Some(body) = env.text_body() else {
        return true;
    };
    let Ok((chat_key, announced)) = offer.offer.accept(from_node, body) else {
        tracing::info!("Malformed QR pair request from {from_node}");
        return true;
    };

    // Never silently replace an existing contact (including one paired a
    // minute ago under this offer): anyone who saw the code could otherwise
    // claim a known node number with their own key. Re-pairing means
    // removing the contact first.
    let known = state
        .engine()
        .config
        .peers
        .iter()
        .any(|p| p.node_num == from_node);
    if known {
        tracing::warn!("QR pair request from already-paired node {from_node} refused");
        let _ = app.emit("qr-pair-refused", from_node);
        return true;
    }

    // Replay protection: each request is accepted once, durably.
    {
        let mut engine = state.engine();
        if engine
            .protection
            .dedup
            .observe(&peer_id, env.message_id, now)
            != Seen::New
        {
            return true;
        }
        if engine.save_dedup().is_err() {
            engine.protection.dedup.forget(&peer_id, &env.message_id);
            return true;
        }
    }

    let Ok(sealed) = outbound::seal(
        WireProtocol::V2.into(),
        OutPayload::PairAccept(&my_name),
        &chat_key,
        &me,
        &peer_id,
        unix_now(),
        DEFAULT_FRAME_BUDGET,
    ) else {
        forget_dedup(&state, &peer_id, &env.message_id);
        return true;
    };
    let Ok(previous) = install_key(&state, my_num, from_node, key_copy(&chat_key)) else {
        forget_dedup(&state, &peer_id, &env.message_id);
        return true;
    };
    if let Err(e) = send_as(&state, my_num, from_node, sealed.frame).await {
        rollback_key(&state, my_num, from_node, previous);
        forget_dedup(&state, &peer_id, &env.message_id);
        tracing::warn!("QR pair accept to {from_node} not sent: {e}");
        return true;
    }
    let name = state
        .engine()
        .nodes
        .get(&from_node)
        .map(|n| n.long_name.clone())
        .filter(|n| !n.trim().is_empty())
        .or_else(|| Some(announced).filter(|n| !n.trim().is_empty()))
        .unwrap_or_else(|| format!("!{from_node:08x}"));
    if let Err(e) = save_peer(&state, from_node, name.clone(), WireProtocol::V2, true).await {
        // Keep memory and disk consistent: no half-paired contact, and the
        // request may be retried.
        rollback_key(&state, my_num, from_node, previous);
        forget_dedup(&state, &peer_id, &env.message_id);
        tracing::warn!("Could not save QR peer {from_node}: {e}");
        let _ = app.emit("qr-pair-failed", e.to_string());
        return true;
    }
    remember_key(app, &state, my_num, from_node, &chat_key).await;
    tracing::info!("Paired with {from_node} via QR");
    let _ = app.emit(
        "peer-paired",
        PeerPairedEvent {
            node_num: from_node,
            name,
        },
    );
    true
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
    let my_num = state
        .engine()
        .my_node_num
        .ok_or(MeshGuardError::NotConnected)?;
    if node_num == my_num {
        return Err(MeshGuardError::Protocol("That is your own radio".into()));
    }
    let cooldown = |wait: std::time::Duration| {
        MeshGuardError::Protocol(format!(
            "Please wait {} s before testing this node again",
            wait.as_secs().max(1)
        ))
    };
    state
        .path_tests
        .lock()
        .await
        .check(node_num)
        .map_err(cooldown)?;
    // The link this request was made on; a reconnect while queued voids it.
    let link_gen = state.link.current();

    // Shares the check-in transmit path: spacing and airtime budget.
    let mut tx = state.status_tx.lock().await;
    if !tx.admit(false).await {
        return Err(MeshGuardError::Protocol(
            "Airtime budget reached — wait a minute and try again".into(),
        ));
    }

    // Register and transmit under the path-test lock: the listener takes it
    // to match a reply, so even an instant reply finds the request id.
    // The timeout starts here, at actual submission.
    let mut tests = state.path_tests.lock().await;
    tests.check(node_num).map_err(cooldown)?;
    let sent = {
        let mut radio = state.radio.lock().await;
        if !state.link.is_current(link_gen) {
            return Err(MeshGuardError::Protocol(
                "The radio reconnected — please run the test again".into(),
            ));
        }
        match radio.as_mut() {
            Some(r) => r.send_traceroute(node_num).await,
            None => Err(MeshGuardError::NotConnected),
        }
    };
    let request_id = sent?;
    tx.mark_sent();
    tests.begin(node_num, request_id);
    Ok(())
}
