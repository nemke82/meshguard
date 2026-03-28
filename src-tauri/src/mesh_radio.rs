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

/// The BLE service UUID that all Meshtastic firmware advertises,
/// regardless of device brand or custom name.
const MESHTASTIC_SERVICE_UUID: uuid::Uuid =
    uuid::Uuid::from_bytes([0x6b, 0xa1, 0xb2, 0x18, 0x15, 0xa8, 0x46, 0x1f,
                            0x9f, 0xa8, 0x5d, 0xca, 0xe2, 0x73, 0xea, 0xfd]);

/// Known name prefixes for Meshtastic-firmware devices.
const MESHTASTIC_NAME_HINTS: &[&str] = &[
    "meshtastic", "sensecap", "t1000", "rak", "heltec",
    "tbeam", "t-beam", "tlora", "t-lora", "station-g",
    "nano-g", "wio-tracker", "trackerd", "meshcore",
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScannedBleDevice {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SerialPortInfo {
    pub name: String,
}

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

fn is_meshtastic_device(name: &str, services: &[uuid::Uuid]) -> bool {
    if services.contains(&MESHTASTIC_SERVICE_UUID) {
        return true;
    }
    let lower = name.to_lowercase();
    MESHTASTIC_NAME_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Scan for nearby Meshtastic BLE devices.
///
/// Detects Meshtastic devices by the BLE service UUID
/// (`6ba1b218-15a8-461f-9fa8-5dcae273eafd`) that all Meshtastic
/// firmware advertises, plus name-pattern fallback for SenseCAP,
/// RAK, Heltec, T-Beam, etc.
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

    let _ = adapter.stop_scan().await;

    let mut devices = Vec::new();
    for peripheral in peripherals {
        if let Ok(Some(props)) = peripheral.properties().await {
            let name = match props.local_name {
                Some(ref n) if !n.is_empty() => n.clone(),
                _ => continue,
            };
            if !is_meshtastic_device(&name, &props.services) {
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

/// List available serial ports.
pub fn list_serial_ports() -> Result<Vec<SerialPortInfo>, MeshGuardError> {
    let ports = utils::stream::available_serial_ports()
        .map_err(|e| MeshGuardError::Ble(format!("Serial port enumeration failed: {e}")))?;
    Ok(ports
        .into_iter()
        .map(|name| SerialPortInfo { name })
        .collect())
}

// ── PacketRouter ──────────────────────────────────────────────

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

// ── MeshRadio ─────────────────────────────────────────────────

/// Shared state refs passed to each connect method.
pub struct ConnectParams {
    pub app_handle: tauri::AppHandle,
    pub mesh_nodes: Arc<Mutex<HashMap<u32, MeshNodeInfo>>>,
    pub my_node_num: Arc<Mutex<Option<u32>>>,
    pub my_device_name: Arc<Mutex<Option<String>>>,
    pub session_keys: Arc<Mutex<HashMap<u32, crate::crypto::SessionKey>>>,
    pub pending_pair_requests: Arc<Mutex<HashMap<u32, Vec<u8>>>>,
}

pub struct MeshRadio {
    api: ConnectedStreamApi<Configured>,
    router: MeshGuardRouter,
}

/// Run config handshake, collect NodeDB, spawn background listener.
/// Called after stream_api.connect() for any transport.
async fn run_config_and_listen(
    configured_api: ConnectedStreamApi<Configured>,
    decoded_listener: &mut meshtastic::packet::PacketReceiver,
    p: &ConnectParams,
) -> Result<(u32, ConnectedStreamApi<Configured>), MeshGuardError> {
    emit_connection_state(&p.app_handle, "configuring");

    let mut found_node_num: Option<u32> = None;
    let mut found_device_name: Option<String> = None;
    let mut nodes: HashMap<u32, MeshNodeInfo> = HashMap::new();

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
            Err(_) => break,
        }
    }

    let node_num = found_node_num
        .ok_or_else(|| MeshGuardError::MeshRadio("Radio did not report MyNodeInfo".into()))?;

    nodes.remove(&node_num);

    *p.mesh_nodes.lock().await = nodes;
    *p.my_node_num.lock().await = Some(node_num);
    *p.my_device_name.lock().await = found_device_name;

    emit_connection_state(&p.app_handle, "connected");
    let _ = p.app_handle.emit("mesh-nodes-updated", ());

    Ok((node_num, configured_api))
}

impl MeshRadio {
    fn from_configured(api: ConnectedStreamApi<Configured>, node_num: u32) -> Self {
        Self {
            api,
            router: MeshGuardRouter::new(node_num),
        }
    }

    /// Connect via Bluetooth LE.
    pub async fn connect_ble(ble_name: &str, p: ConnectParams) -> Result<Self, MeshGuardError> {
        emit_connection_state(&p.app_handle, "connecting");
        stop_any_active_scan().await?;

        let stream_api = StreamApi::new();
        let ble_stream =
            utils::stream::build_ble_stream(BleId::from_name(ble_name), Duration::from_secs(15))
                .await
                .map_err(|e| MeshGuardError::Ble(format!("BLE connect failed: {e}")))?;

        let (mut decoded_listener, connected_api) = stream_api.connect(ble_stream).await;

        let config_id = utils::generate_rand_id();
        let configured_api = connected_api
            .configure(config_id)
            .await
            .map_err(|e| MeshGuardError::MeshRadio(format!("Config handshake failed: {e}")))?;

        let (node_num, configured_api) =
            run_config_and_listen(configured_api, &mut decoded_listener, &p).await?;

        spawn_listener(
            decoded_listener, p.app_handle.clone(), p.mesh_nodes.clone(),
            node_num, p.session_keys.clone(), p.pending_pair_requests.clone(),
        );

        Ok(Self::from_configured(configured_api, node_num))
    }

    /// Connect via TCP / WiFi.
    pub async fn connect_tcp(address: &str, p: ConnectParams) -> Result<Self, MeshGuardError> {
        emit_connection_state(&p.app_handle, "connecting");

        let stream_api = StreamApi::new();
        let tcp_stream =
            utils::stream::build_tcp_stream(address.to_string())
                .await
                .map_err(|e| MeshGuardError::Ble(format!("TCP connect failed: {e}")))?;

        let (mut decoded_listener, connected_api) = stream_api.connect(tcp_stream).await;

        let config_id = utils::generate_rand_id();
        let configured_api = connected_api
            .configure(config_id)
            .await
            .map_err(|e| MeshGuardError::MeshRadio(format!("Config handshake failed: {e}")))?;

        let (node_num, configured_api) =
            run_config_and_listen(configured_api, &mut decoded_listener, &p).await?;

        spawn_listener(
            decoded_listener, p.app_handle.clone(), p.mesh_nodes.clone(),
            node_num, p.session_keys.clone(), p.pending_pair_requests.clone(),
        );

        Ok(Self::from_configured(configured_api, node_num))
    }

    /// Connect via USB serial.
    pub async fn connect_serial(port_name: &str, p: ConnectParams) -> Result<Self, MeshGuardError> {
        emit_connection_state(&p.app_handle, "connecting");

        let stream_api = StreamApi::new();
        let serial_stream =
            utils::stream::build_serial_stream(port_name.to_string(), None, None, None)
                .map_err(|e| MeshGuardError::Ble(format!("Serial connect failed: {e}")))?;

        let (mut decoded_listener, connected_api) = stream_api.connect(serial_stream).await;

        let config_id = utils::generate_rand_id();
        let configured_api = connected_api
            .configure(config_id)
            .await
            .map_err(|e| MeshGuardError::MeshRadio(format!("Config handshake failed: {e}")))?;

        let (node_num, configured_api) =
            run_config_and_listen(configured_api, &mut decoded_listener, &p).await?;

        spawn_listener(
            decoded_listener, p.app_handle.clone(), p.mesh_nodes.clone(),
            node_num, p.session_keys.clone(), p.pending_pair_requests.clone(),
        );

        Ok(Self::from_configured(configured_api, node_num))
    }

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
                true,
                false,
                true,
                None,
                None,
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

#[derive(Clone, serde::Serialize)]
pub struct IncomingMessageEvent {
    pub from_node: u32,
    pub from_name: String,
    pub text: String,
    pub timestamp: i64,
    pub message_id: String,
}

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
