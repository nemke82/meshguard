use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use meshtastic::api::state::Configured;
use meshtastic::api::{ConnectedStreamApi, StreamApi};
use meshtastic::packet::{PacketDestination, PacketRouter};
use meshtastic::protobufs;
use meshtastic::protobufs::from_radio::PayloadVariant;
use meshtastic::types::{EncodedMeshPacketData, MeshChannel, NodeId};
use meshtastic::utils;
use meshtastic::utils::stream::BleId;
use tauri::Emitter;
use tokio::sync::Mutex;

use crate::error::MeshGuardError;
use crate::state::MeshNodeInfo;

/// Serializable BLE device info for the frontend.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScannedBleDevice {
    pub name: String,
    pub address: String,
}

/// Get the first BLE adapter and ensure no scan is running on it.
async fn stop_any_active_scan() -> Result<(), MeshGuardError> {
    let manager = Manager::new()
        .await
        .map_err(|e| MeshGuardError::Ble(e.to_string()))?;
    let adapters = manager
        .adapters()
        .await
        .map_err(|e| MeshGuardError::Ble(e.to_string()))?;
    if let Some(adapter) = adapters.into_iter().next() {
        let _ = adapter.stop_scan().await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}

/// Scan for nearby Meshtastic BLE devices.
///
/// Uses btleplug directly (rather than the meshtastic crate's
/// `available_ble_devices`) so we can:
///   1. Filter results to only Meshtastic devices
///   2. Explicitly stop the scan afterward (avoids BlueZ
///      "Operation already in progress" when connecting later)
pub async fn scan_ble_devices(timeout_secs: u64) -> Result<Vec<ScannedBleDevice>, MeshGuardError> {
    let manager = Manager::new()
        .await
        .map_err(|e| MeshGuardError::Ble(e.to_string()))?;
    let adapters = manager
        .adapters()
        .await
        .map_err(|e| MeshGuardError::Ble(e.to_string()))?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| MeshGuardError::Ble("No Bluetooth adapter found".into()))?;

    // Stop any lingering scan from a previous invocation
    let _ = adapter.stop_scan().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| MeshGuardError::Ble(format!("BLE scan start failed: {e}")))?;

    tokio::time::sleep(Duration::from_secs(timeout_secs)).await;

    let peripherals = adapter
        .peripherals()
        .await
        .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

    // Always stop the scan so the adapter is free for the next operation
    let _ = adapter.stop_scan().await;

    let mut devices = Vec::new();
    for peripheral in peripherals {
        if let Ok(Some(props)) = peripheral.properties().await {
            let name = match props.local_name {
                Some(ref n) if !n.is_empty() => n.clone(),
                _ => continue,
            };
            // Only include Meshtastic devices
            if !name.to_lowercase().contains("meshtastic") {
                continue;
            }
            devices.push(ScannedBleDevice {
                name,
                address: peripheral.address().to_string(),
            });
        }
    }

    Ok(devices)
}

/// Router that the meshtastic crate needs when sending packets.
pub struct MeshGuardRouter {
    node_id: u32,
}

impl MeshGuardRouter {
    pub fn new(node_id: u32) -> Self {
        Self { node_id }
    }
}

#[derive(Debug)]
pub struct RouterError(String);

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RouterError {}

impl PacketRouter<(), RouterError> for MeshGuardRouter {
    fn handle_packet_from_radio(
        &mut self,
        _packet: protobufs::FromRadio,
    ) -> Result<(), RouterError> {
        Ok(())
    }

    fn handle_mesh_packet(
        &mut self,
        _packet: protobufs::MeshPacket,
    ) -> Result<(), RouterError> {
        Ok(())
    }

    fn source_node_id(&self) -> NodeId {
        self.node_id.into()
    }
}

/// Wraps the meshtastic crate's ConnectedStreamApi to provide
/// a simpler interface for MeshGuard.
pub struct MeshRadio {
    api: ConnectedStreamApi<Configured>,
    router: MeshGuardRouter,
}

impl MeshRadio {
    /// Connect to a Meshtastic device by BLE name and run the config handshake.
    /// Returns the MeshRadio and a background listener handle.
    pub async fn connect_ble(
        ble_name: &str,
        app_handle: tauri::AppHandle,
        mesh_nodes: Arc<Mutex<HashMap<u32, MeshNodeInfo>>>,
        my_node_num: Arc<Mutex<Option<u32>>>,
        my_device_name: Arc<Mutex<Option<String>>>,
        session_keys: Arc<Mutex<HashMap<u32, crate::crypto::SessionKey>>>,
        pending_pair_requests: Arc<Mutex<HashMap<u32, Vec<u8>>>>,
    ) -> Result<Self, MeshGuardError> {
        emit_connection_state(&app_handle, "connecting");

        // Stop any lingering BlueZ scan left over from the device-scan step.
        // build_ble_stream starts its own scan internally; BlueZ only allows
        // one active scan per adapter, so a stale scan causes
        // "Operation already in progress" errors.
        stop_any_active_scan().await?;

        let stream_api = StreamApi::new();

        let ble_stream =
            utils::stream::build_ble_stream(BleId::from_name(ble_name), Duration::from_secs(15))
                .await
                .map_err(|e| MeshGuardError::Ble(format!("BLE connect failed: {e}")))?;

        let (mut decoded_listener, connected_api) = stream_api.connect(ble_stream).await;

        emit_connection_state(&app_handle, "configuring");

        let config_id = utils::generate_rand_id();
        let configured_api = connected_api
            .configure(config_id)
            .await
            .map_err(|e| MeshGuardError::MeshRadio(format!("Config handshake failed: {e}")))?;

        // Drain the initial config/node dump from the radio.
        // The meshtastic crate streams FromRadio packets through decoded_listener;
        // we collect NodeInfo and MyNodeInfo during this phase.
        let mut found_node_num: Option<u32> = None;
        let mut found_device_name: Option<String> = None;
        let mut nodes: HashMap<u32, MeshNodeInfo> = HashMap::new();

        // Read packets with a timeout — once the stream goes quiet, config is done.
        loop {
            match tokio::time::timeout(Duration::from_secs(5), decoded_listener.recv()).await {
                Ok(Some(from_radio)) => {
                    if let Some(variant) = from_radio.payload_variant {
                        match variant {
                            PayloadVariant::MyInfo(my_info) => {
                                found_node_num = Some(my_info.my_node_num);
                                tracing::info!("My node num: {}", my_info.my_node_num);
                            }
                            PayloadVariant::NodeInfo(node_info) => {
                                let node = node_info_to_mesh_node(&node_info);
                                if Some(node.node_num) == found_node_num {
                                    if let Some(ref user) = node_info.user {
                                        found_device_name = Some(user.long_name.clone());
                                    }
                                }
                                nodes.insert(node.node_num, node);
                            }
                            PayloadVariant::ConfigCompleteId(id) => {
                                tracing::info!("Config complete (id={})", id);
                            }
                            _ => {}
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break, // timeout — config dump is complete
            }
        }

        let node_num = found_node_num
            .ok_or_else(|| MeshGuardError::MeshRadio("Radio did not report MyNodeInfo".into()))?;

        // Remove ourselves from the mesh nodes list
        nodes.remove(&node_num);

        // Store results
        *mesh_nodes.lock().await = nodes.clone();
        *my_node_num.lock().await = Some(node_num);
        *my_device_name.lock().await = found_device_name;

        // Spawn background listener for ongoing packets
        spawn_listener(
            decoded_listener,
            app_handle.clone(),
            mesh_nodes,
            node_num,
            session_keys,
            pending_pair_requests,
        );

        emit_connection_state(&app_handle, "connected");

        let _ = app_handle.emit("mesh-nodes-updated", ());

        Ok(Self {
            api: configured_api,
            router: MeshGuardRouter::new(node_num),
        })
    }

    /// Send raw bytes on PortNum::PrivateApp to a specific node.
    pub async fn send_private_app(
        &mut self,
        data: Vec<u8>,
        destination_node: u32,
    ) -> Result<(), MeshGuardError> {
        let encoded = EncodedMeshPacketData::new(data);
        self.api
            .send_mesh_packet(
                &mut self.router,
                encoded,
                protobufs::PortNum::PrivateApp,
                PacketDestination::Node(destination_node.into()),
                MeshChannel::new(0).unwrap(),
                true,  // want_ack
                false, // want_response
                true,  // echo_response
                None,  // reply_id
                None,  // emoji
            )
            .await
            .map_err(|e| MeshGuardError::MeshRadio(format!("Send failed: {e}")))
    }
}

fn node_info_to_mesh_node(ni: &protobufs::NodeInfo) -> MeshNodeInfo {
    let (user_name, long_name, short_name, hw_model) = ni
        .user
        .as_ref()
        .map(|u| {
            (
                u.long_name.clone(),
                u.long_name.clone(),
                u.short_name.clone(),
                format!("{:?}", protobufs::HardwareModel::try_from(u.hw_model).unwrap_or(protobufs::HardwareModel::Unset)),
            )
        })
        .unwrap_or_default();

    MeshNodeInfo {
        node_num: ni.num,
        user_name,
        long_name,
        short_name,
        hw_model,
        snr: ni.snr,
        rssi: 0,
        last_heard: ni.last_heard as i64,
        is_online: ni.last_heard > 0
            && (chrono::Utc::now().timestamp() - ni.last_heard as i64) < 7200,
    }
}

fn spawn_listener(
    mut listener: meshtastic::packet::PacketReceiver,
    app_handle: tauri::AppHandle,
    mesh_nodes: Arc<Mutex<HashMap<u32, MeshNodeInfo>>>,
    my_node_num: u32,
    session_keys: Arc<Mutex<HashMap<u32, crate::crypto::SessionKey>>>,
    pending_pair_requests: Arc<Mutex<HashMap<u32, Vec<u8>>>>,
) {
    tokio::spawn(async move {
        while let Some(from_radio) = listener.recv().await {
            if let Some(variant) = from_radio.payload_variant {
                match variant {
                    PayloadVariant::NodeInfo(node_info) => {
                        let node = node_info_to_mesh_node(&node_info);
                        if node.node_num != my_node_num {
                            mesh_nodes.lock().await.insert(node.node_num, node);
                            let _ = app_handle.emit("mesh-nodes-updated", ());
                        }
                    }
                    PayloadVariant::Packet(mesh_packet) => {
                        handle_incoming_packet(
                            &mesh_packet,
                            &app_handle,
                            &session_keys,
                            &pending_pair_requests,
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        }

        tracing::warn!("Mesh radio listener ended — radio disconnected");
        emit_connection_state(&app_handle, "disconnected");
    });
}

/// Event payload for incoming messages sent to the frontend.
#[derive(Clone, serde::Serialize)]
pub struct IncomingMessageEvent {
    pub from_node: u32,
    pub from_name: String,
    pub text: String,
    pub timestamp: i64,
    pub message_id: String,
}

/// Event payload for pair requests sent to the frontend.
#[derive(Clone, serde::Serialize)]
pub struct PairRequestEvent {
    pub from_node: u32,
    pub from_name: String,
    pub timestamp: i64,
}

async fn handle_incoming_packet(
    packet: &protobufs::MeshPacket,
    app_handle: &tauri::AppHandle,
    session_keys: &Arc<Mutex<HashMap<u32, crate::crypto::SessionKey>>>,
    pending_pair_requests: &Arc<Mutex<HashMap<u32, Vec<u8>>>>,
) {
    let decoded = match &packet.payload_variant {
        Some(protobufs::mesh_packet::PayloadVariant::Decoded(d)) => d,
        _ => return,
    };

    if decoded.portnum != protobufs::PortNum::PrivateApp as i32 {
        return;
    }

    let from_node = packet.from;
    let payload = &decoded.payload;

    if payload.is_empty() {
        return;
    }

    // Try to decrypt with a known session key
    let keys = session_keys.lock().await;
    if let Some(key) = keys.get(&from_node) {
        match crate::protocol::MeshMessage::decrypt_envelope(payload, key) {
            Ok(msg) => {
                match msg {
                    crate::protocol::MeshMessage::Text {
                        id, ciphertext, timestamp, ..
                    } => {
                        match key.decrypt(&ciphertext) {
                            Ok(plaintext_bytes) => {
                                let text = String::from_utf8_lossy(&plaintext_bytes).to_string();
                                let _ = app_handle.emit(
                                    "incoming-message",
                                    IncomingMessageEvent {
                                        from_node,
                                        from_name: String::new(),
                                        text,
                                        timestamp,
                                        message_id: id,
                                    },
                                );
                            }
                            Err(e) => {
                                tracing::warn!("Failed to decrypt inner text from {from_node}: {e}");
                            }
                        }
                    }
                    crate::protocol::MeshMessage::PairAccept { responder_name, .. } => {
                        tracing::info!("Pair accepted by {responder_name} (node {from_node})");
                        let _ = app_handle.emit(
                            "pair-accepted",
                            PairRequestEvent {
                                from_node,
                                from_name: responder_name,
                                timestamp: chrono::Utc::now().timestamp(),
                            },
                        );
                    }
                    crate::protocol::MeshMessage::PairRequest { sender_name, .. } => {
                        tracing::info!("Pair request from {sender_name} (node {from_node}) — already have key");
                    }
                    _ => {}
                }
                return;
            }
            Err(_) => {
                tracing::debug!("Could not decrypt from known peer {from_node} — key mismatch?");
            }
        }
    }
    drop(keys);

    // Unknown sender or decryption failed — store as pending pair request
    pending_pair_requests
        .lock()
        .await
        .insert(from_node, payload.clone());

    let _ = app_handle.emit(
        "pair-request",
        PairRequestEvent {
            from_node,
            from_name: String::new(),
            timestamp: chrono::Utc::now().timestamp(),
        },
    );
}

fn emit_connection_state(app_handle: &tauri::AppHandle, state: &str) {
    let _ = app_handle.emit("connection-state", state);
}
