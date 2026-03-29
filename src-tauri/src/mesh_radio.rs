use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::Manager;
use meshtastic::api::state::Configured;
use meshtastic::api::{ConnectedStreamApi, StreamApi, StreamHandle};
use meshtastic::packet::{PacketDestination, PacketRouter};
use meshtastic::protobufs;
use meshtastic::protobufs::from_radio::PayloadVariant;
use meshtastic::types::{EncodedMeshPacketData, MeshChannel, NodeId};
use meshtastic::utils;
use tauri::Emitter;
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::Mutex;

use crate::error::MeshGuardError;
use crate::state::MeshNodeInfo;

/// The BLE service UUID that all Meshtastic firmware advertises.
const MESHTASTIC_SERVICE_UUID: uuid::Uuid =
    uuid::Uuid::from_bytes([0x6b, 0xa1, 0xb2, 0x18, 0x15, 0xa8, 0x46, 0x1f,
                            0x9f, 0xa8, 0x5d, 0xca, 0xe2, 0x73, 0xea, 0xfd]);

const FROMRADIO_UUID: uuid::Uuid =
    uuid::Uuid::from_bytes([0x2c, 0x55, 0xe6, 0x9e, 0x49, 0x93, 0x11, 0xed,
                            0xb8, 0x78, 0x02, 0x42, 0xac, 0x12, 0x00, 0x02]);

const TORADIO_UUID: uuid::Uuid =
    uuid::Uuid::from_bytes([0xf7, 0x5c, 0x76, 0xd2, 0x12, 0x9e, 0x4d, 0xad,
                            0xa1, 0xdd, 0x78, 0x66, 0x12, 0x44, 0x01, 0xe7]);

/// Known name prefixes for Meshtastic-firmware devices.
const MESHTASTIC_NAME_HINTS: &[&str] = &[
    "meshtastic", "sensecap", "t1000", "rak", "heltec",
    "tbeam", "t-beam", "tlora", "t-lora", "station-g",
    "nano-g", "wio-tracker", "trackerd", "meshcore",
];

/// Meshtastic packet header: magic bytes + 2-byte length (big-endian).
fn format_packet(data: &[u8]) -> Vec<u8> {
    let len = data.len() as u16;
    let [lsb, msb] = len.to_le_bytes();
    let mut buf = vec![0x94, 0xc3, msb, lsb];
    buf.extend_from_slice(data);
    buf
}

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

/// Pair with a BLE device via BlueZ D-Bus, providing the PIN/passkey.
///
/// Important: call this BEFORE connecting with btleplug. `device.pair()`
/// both connects and pairs, creating an encrypted link from the start.
/// After this returns, btleplug's `connect()` will reuse the encrypted
/// connection established by BlueZ.
///
/// Only `request_passkey` is set on the agent to get "KeyboardOnly"
/// capability — paired with the device's "DisplayOnly" this triggers
/// the Passkey Entry method where BlueZ asks us for the PIN.
#[cfg(target_os = "linux")]
async fn pair_ble_device(mac_address: &str, pin: u32) -> Result<(), MeshGuardError> {
    let address: bluer::Address = mac_address
        .parse()
        .map_err(|e| MeshGuardError::Ble(format!("Invalid MAC address '{mac_address}': {e}")))?;

    let session = bluer::Session::new()
        .await
        .map_err(|e| MeshGuardError::Ble(format!("BlueZ D-Bus session failed: {e}")))?;

    let adapter = session
        .default_adapter()
        .await
        .map_err(|e| MeshGuardError::Ble(format!("No Bluetooth adapter via BlueZ: {e}")))?;

    let device = adapter
        .device(address)
        .map_err(|e| MeshGuardError::Ble(format!("Device {mac_address} not in BlueZ: {e}")))?;

    // If already paired, verify the bond works by checking connectivity.
    // If the bond is stale (device was reset), remove it and re-pair.
    if device.is_paired().await.unwrap_or(false) {
        tracing::info!("Device {mac_address} already paired — checking bond validity");
        if device.is_connected().await.unwrap_or(false) {
            tracing::info!("Bond appears valid (device connected)");
            return Ok(());
        }
        // Stale bond — remove and re-pair
        tracing::info!("Removing stale bond for {mac_address}...");
        let _ = adapter.remove_device(address).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        // After removal, the device may no longer be in BlueZ cache.
        // We need it to be re-discovered. Since btleplug already scanned,
        // it should still be there, but let's verify.
        match adapter.device(address) {
            Ok(_) => {}
            Err(_) => {
                tracing::info!("Device not in cache after bond removal — re-scanning...");
                let disc = adapter.discover_devices().await.map_err(|e| {
                    MeshGuardError::Ble(format!("Re-scan failed: {e}"))
                })?;
                // Keep discovery running briefly so BlueZ rediscovers the device
                tokio::time::sleep(Duration::from_secs(3)).await;
                drop(disc);
            }
        }
    }

    let device = adapter
        .device(address)
        .map_err(|e| MeshGuardError::Ble(format!("Device {mac_address} lost from BlueZ: {e}")))?;

    // Only set request_passkey → "KeyboardOnly" capability.
    // With device "DisplayOnly" → Passkey Entry (BlueZ asks us for PIN).
    let agent = bluer::agent::Agent {
        request_default: true,
        request_passkey: Some(Box::new(move |_req| {
            Box::pin(async move {
                tracing::info!("BlueZ agent: providing passkey {pin}");
                Ok(pin)
            })
        })),
        ..Default::default()
    };

    let _agent_handle = session
        .register_agent(agent)
        .await
        .map_err(|e| MeshGuardError::Ble(format!("BlueZ agent registration failed: {e}")))?;

    tracing::info!("Pairing with {mac_address} (PIN {pin})...");
    device.pair().await.map_err(|e| {
        MeshGuardError::Ble(format!(
            "BLE pairing failed: {e} — check the device PIN (default is 123456)"
        ))
    })?;

    let is_paired = device.is_paired().await.unwrap_or(false);
    let is_connected = device.is_connected().await.unwrap_or(false);
    tracing::info!("BLE pairing completed — paired={is_paired} connected={is_connected}");

    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn pair_ble_device(_mac_address: &str, _pin: u32) -> Result<(), MeshGuardError> {
    Ok(())
}

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

pub fn list_serial_ports() -> Result<Vec<SerialPortInfo>, MeshGuardError> {
    let ports = utils::stream::available_serial_ports()
        .map_err(|e| MeshGuardError::Ble(format!("Serial port enumeration failed: {e}")))?;
    Ok(ports
        .into_iter()
        .map(|name| SerialPortInfo { name })
        .collect())
}

// ── Custom BLE Stream (polling-based) ─────────────────────────
//
// The meshtastic crate's build_ble_stream relies on fromnum BLE
// notifications to know when the radio has data. Some devices
// (SenseCAP T1000, etc.) don't fire these notifications reliably,
// causing the connection to hang. This implementation polls the
// fromradio characteristic directly.

fn find_char(chars: &std::collections::BTreeSet<Characteristic>, uuid: uuid::Uuid) -> Result<Characteristic, MeshGuardError> {
    chars.iter()
        .find(|c| c.uuid == uuid)
        .cloned()
        .ok_or_else(|| MeshGuardError::Ble(format!("Characteristic {uuid} not found")))
}

/// Build a BLE stream using direct btleplug polling instead of
/// notification-based reading. Returns a StreamHandle compatible
/// with the meshtastic crate's StreamApi.
///
/// On Linux, uses `bluer` to pair BEFORE connecting with btleplug.
/// `device.pair()` establishes an encrypted connection with the PIN,
/// and btleplug's `connect()` then reuses that encrypted link.
async fn build_polling_ble_stream(
    device_name: &str,
    pin: u32,
) -> Result<StreamHandle<DuplexStream>, MeshGuardError> {
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
        .map_err(|e| MeshGuardError::Ble(format!("BLE scan failed: {e}")))?;

    tokio::time::sleep(Duration::from_secs(5)).await;

    let peripherals = adapter
        .peripherals()
        .await
        .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

    let _ = adapter.stop_scan().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut target = None;
    for p in peripherals {
        if let Ok(Some(props)) = p.properties().await {
            if props.local_name.as_deref() == Some(device_name) {
                target = Some(p);
                break;
            }
        }
    }

    let radio = target
        .ok_or_else(|| MeshGuardError::Ble(format!("Device '{device_name}' not found during BLE scan")))?;

    let mac_address = radio.address().to_string();

    // On Linux: pair via BlueZ BEFORE connecting with btleplug.
    // bluer's device.pair() connects + pairs in one step, creating an
    // encrypted link. btleplug's connect() then reuses that connection.
    pair_ble_device(&mac_address, pin).await?;

    tracing::info!("Connecting to BLE device: {device_name} ({mac_address})");
    radio.connect().await
        .map_err(|e| MeshGuardError::Ble(format!("BLE connect failed: {e}")))?;

    tracing::info!("Discovering GATT services...");
    radio.discover_services().await
        .map_err(|e| MeshGuardError::Ble(format!("GATT discovery failed: {e}")))?;

    let chars = radio.characteristics();
    let fromradio_char = find_char(&chars, FROMRADIO_UUID)?;
    find_char(&chars, TORADIO_UUID)?;
    tracing::info!("Found Meshtastic GATT characteristics");

    // Verify GATT access works on the encrypted connection.
    // If it fails, disconnect and reconnect — the reconnection uses the
    // bond and negotiates encryption from scratch.
    match radio.read(&fromradio_char).await {
        Ok(_) => tracing::info!("GATT read verified — connection is encrypted"),
        Err(e) => {
            tracing::warn!("GATT read failed ({e}) — reconnecting to apply bond encryption...");
            let _ = radio.disconnect().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            radio.connect().await
                .map_err(|e| MeshGuardError::Ble(format!("BLE reconnect failed: {e}")))?;
            radio.discover_services().await
                .map_err(|e| MeshGuardError::Ble(format!("GATT re-discovery failed: {e}")))?;
            // Re-find characteristics after re-discovery
            let chars = radio.characteristics();
            let new_from = find_char(&chars, FROMRADIO_UUID)?;
            match radio.read(&new_from).await {
                Ok(_) => tracing::info!("GATT read verified after reconnect"),
                Err(e) => {
                    return Err(MeshGuardError::Ble(format!(
                        "GATT still fails after reconnect: {e} — try removing the device from \
                         system Bluetooth settings and reconnecting"
                    )));
                }
            }
        }
    }

    // Re-fetch characteristics after possible reconnect
    let chars = radio.characteristics();
    let fromradio_char = find_char(&chars, FROMRADIO_UUID)?;
    let toradio_char = find_char(&chars, TORADIO_UUID)?;

    let (client, mut server) = tokio::io::duplex(4096);

    let handle = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let mut write_buf = [0u8; 512];

        loop {
            match tokio::time::timeout(Duration::from_millis(50), server.read(&mut write_buf)).await {
                Ok(Ok(0)) => {
                    tracing::debug!("BLE client stream closed");
                    break;
                }
                Ok(Ok(len)) => {
                    let payload = if len > 4 && write_buf[0] == 0x94 && write_buf[1] == 0xc3 {
                        &write_buf[4..len]
                    } else {
                        &write_buf[..len]
                    };
                    let mut ok = false;
                    for retry in 0..3 {
                        match radio.write(&toradio_char, payload, WriteType::WithResponse).await {
                            Ok(()) => { ok = true; break; }
                            Err(e) => {
                                tracing::warn!("BLE write retry {retry}: {e}");
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                        }
                    }
                    if !ok {
                        tracing::error!("BLE write failed after retries");
                        break;
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!("Server stream read error: {e}");
                    break;
                }
                Err(_) => {}
            }

            match radio.read(&fromradio_char).await {
                Ok(data) if !data.is_empty() => {
                    let framed = format_packet(&data);
                    if let Err(e) = server.write_all(&framed).await {
                        tracing::error!("Server stream write error: {e}");
                        break;
                    }
                }
                Ok(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => {
                    tracing::error!("BLE read failed: {e}");
                    break;
                }
            }
        }

        tracing::warn!("BLE polling loop ended");
        let _ = radio.disconnect().await;
        Ok::<(), meshtastic::errors::Error>(())
    });

    Ok(StreamHandle {
        stream: client,
        join_handle: Some(handle),
    })
}

// ── Android BLE Stream (native Kotlin plugin) ────────────────
//
// btleplug doesn't support Android. Instead, we use the native
// Kotlin BlePlugin for GATT operations: connectDevice, readFromRadio,
// writeToRadio. A background std::thread makes blocking plugin calls,
// and channels bridge data to a tokio task that drives the DuplexStream.

#[cfg(target_os = "android")]
async fn build_android_ble_stream(
    app_handle: &tauri::AppHandle,
    address: &str,
    pin: u32,
) -> Result<StreamHandle<DuplexStream>, MeshGuardError> {
    use base64::Engine;
    use tauri::Manager;

    let app = app_handle.clone();
    let addr = address.to_string();
    let addr_for_log = addr.clone();

    tracing::info!("Connecting to BLE device via Android native: {addr_for_log}");

    // Connect + bond + discover GATT via native Kotlin plugin (blocking)
    let connect_result = tokio::task::spawn_blocking({
        let app = app.clone();
        move || {
            let state = app.state::<crate::ble_plugin::BlePluginState<tauri::Wry>>();
            state
                .0
                .run_mobile_plugin::<serde_json::Value>(
                    "connectDevice",
                    serde_json::json!({ "address": addr, "pin": pin }),
                )
                .map_err(|e| MeshGuardError::Ble(format!("Android BLE connect failed: {e}")))
        }
    })
    .await
    .map_err(|e| MeshGuardError::Ble(format!("BLE connect task panicked: {e}")))?;

    connect_result?;
    tracing::info!("Android BLE connected — GATT ready for {addr_for_log}");

    let (client, server) = tokio::io::duplex(4096);

    // Channel: BLE thread → bridge task (fromradio data)
    let (ble_tx, mut ble_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // Channel: bridge task → BLE thread (toradio data)
    let (proto_tx, proto_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Background thread: blocking native plugin calls for BLE I/O
    let app_for_thread = app.clone();
    std::thread::spawn(move || {
        use tauri::Manager;

        let state = app_for_thread.state::<crate::ble_plugin::BlePluginState<tauri::Wry>>();
        let plugin = &state.0;

        loop {
            // Write any pending data to toradio
            while let Ok(data) = proto_rx.try_recv() {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                match plugin.run_mobile_plugin::<serde_json::Value>(
                    "writeToRadio",
                    serde_json::json!({ "data": b64 }),
                ) {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("Android BLE write failed: {e}");
                        return;
                    }
                }
            }

            // Read from fromradio
            match plugin.run_mobile_plugin::<serde_json::Value>(
                "readFromRadio",
                serde_json::json!({}),
            ) {
                Ok(response) => {
                    if let Some(data_str) = response.get("data").and_then(|d| d.as_str()) {
                        if !data_str.is_empty() {
                            if let Ok(data) =
                                base64::engine::general_purpose::STANDARD.decode(data_str)
                            {
                                if !data.is_empty() && ble_tx.send(data).is_err() {
                                    break;
                                }
                                continue;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(75));
                }
                Err(e) => {
                    tracing::error!("Android BLE read failed: {e}");
                    break;
                }
            }
        }

        tracing::warn!("Android BLE I/O thread ended");
        let _ = plugin.run_mobile_plugin::<serde_json::Value>(
            "disconnectBleDevice",
            serde_json::json!({}),
        );
    });

    // Bridge task: channels ↔ duplex stream
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut server = server;
        let mut write_buf = [0u8; 512];

        loop {
            tokio::select! {
                Some(data) = ble_rx.recv() => {
                    let framed = format_packet(&data);
                    if server.write_all(&framed).await.is_err() {
                        break;
                    }
                }
                result = server.read(&mut write_buf) => {
                    match result {
                        Ok(0) => break,
                        Ok(len) => {
                            let payload = if len > 4
                                && write_buf[0] == 0x94
                                && write_buf[1] == 0xc3
                            {
                                write_buf[4..len].to_vec()
                            } else {
                                write_buf[..len].to_vec()
                            };
                            if proto_tx.send(payload).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        tracing::warn!("Android BLE bridge task ended");
        Ok::<(), meshtastic::errors::Error>(())
    });

    Ok(StreamHandle {
        stream: client,
        join_handle: Some(handle),
    })
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

async fn run_config_and_listen(
    configured_api: ConnectedStreamApi<Configured>,
    decoded_listener: &mut meshtastic::packet::PacketReceiver,
    p: &ConnectParams,
) -> Result<(u32, ConnectedStreamApi<Configured>), MeshGuardError> {
    emit_connection_state(&p.app_handle, "configuring");

    let mut found_node_num: Option<u32> = None;
    let mut found_device_name: Option<String> = None;
    let mut nodes: HashMap<u32, MeshNodeInfo> = HashMap::new();
    let mut packet_count: u32 = 0;

    loop {
        let timeout_secs = if packet_count == 0 { 30 } else { 10 };

        match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            decoded_listener.recv(),
        )
        .await
        {
            Ok(Some(from_radio)) => {
                packet_count += 1;
                if let Some(ref variant) = from_radio.payload_variant {
                    tracing::debug!("Config packet #{packet_count}: {}", variant_name(variant));

                    match variant {
                        PayloadVariant::MyInfo(my_info) => {
                            found_node_num = Some(my_info.my_node_num);
                            tracing::info!("My node num: {}", my_info.my_node_num);
                        }
                        PayloadVariant::NodeInfo(node_info) => {
                            let node = node_info_to_mesh_node(node_info);
                            if Some(node.node_num) == found_node_num {
                                if let Some(ref user) = node_info.user {
                                    found_device_name = Some(user.long_name.clone());
                                }
                            }
                            nodes.insert(node.node_num, node);
                        }
                        PayloadVariant::ConfigCompleteId(id) => {
                            tracing::info!("Config complete (id={id}), received {packet_count} packets");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Ok(None) => {
                tracing::warn!("Config stream closed after {packet_count} packets");
                break;
            }
            Err(_) => {
                tracing::warn!("Config timeout after {timeout_secs}s ({packet_count} packets received)");
                break;
            }
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
    /// `pin` is the BLE pairing PIN (default 123456 for Meshtastic).
    /// `ble_address` is the MAC address — required on Android, optional on desktop.
    pub async fn connect_ble(
        ble_name: &str,
        ble_address: Option<String>,
        pin: u32,
        p: ConnectParams,
    ) -> Result<Self, MeshGuardError> {
        emit_connection_state(&p.app_handle, "connecting");

        let ble_stream = {
            #[cfg(target_os = "android")]
            {
                let address = ble_address.ok_or_else(|| {
                    MeshGuardError::Ble("Device address required for Android BLE".into())
                })?;
                build_android_ble_stream(&p.app_handle, &address, pin).await?
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = ble_address;
                stop_any_active_scan().await?;
                build_polling_ble_stream(ble_name, pin).await?
            }
        };

        let stream_api = StreamApi::new();
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
        tracing::info!(
            "Sending PrivateApp to node {destination_node} ({} bytes)",
            data.len()
        );
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

fn variant_name(v: &PayloadVariant) -> &'static str {
    match v {
        PayloadVariant::Packet(_) => "Packet",
        PayloadVariant::MyInfo(_) => "MyInfo",
        PayloadVariant::NodeInfo(_) => "NodeInfo",
        PayloadVariant::Config(_) => "Config",
        PayloadVariant::ModuleConfig(_) => "ModuleConfig",
        PayloadVariant::Channel(_) => "Channel",
        PayloadVariant::ConfigCompleteId(_) => "ConfigCompleteId",
        PayloadVariant::Rebooted(_) => "Rebooted",
        PayloadVariant::LogRecord(_) => "LogRecord",
        PayloadVariant::QueueStatus(_) => "QueueStatus",
        PayloadVariant::XmodemPacket(_) => "XmodemPacket",
        PayloadVariant::Metadata(_) => "Metadata",
        PayloadVariant::MqttClientProxyMessage(_) => "MqttClientProxyMessage",
        PayloadVariant::FileInfo(_) => "FileInfo",
        PayloadVariant::ClientNotification(_) => "ClientNotification",
        _ => "Unknown",
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
        let mut pkt_counter: u64 = 0;
        while let Some(from_radio) = listener.recv().await {
            if let Some(ref variant) = from_radio.payload_variant {
                pkt_counter += 1;
                let vname = variant_name(variant);
                if vname != "QueueStatus" {
                    tracing::debug!("Listener #{pkt_counter}: {vname}");
                }

                match variant {
                    PayloadVariant::NodeInfo(node_info) => {
                        let node = node_info_to_mesh_node(node_info);
                        if node.node_num != my_node_num {
                            tracing::info!(
                                "Mesh node update: {} ({})",
                                node.long_name,
                                node.node_num
                            );
                            mesh_nodes.lock().await.insert(node.node_num, node);
                            let _ = app_handle.emit("mesh-nodes-updated", ());
                        }
                    }
                    PayloadVariant::Packet(mesh_packet) => {
                        let pv = match &mesh_packet.payload_variant {
                            Some(protobufs::mesh_packet::PayloadVariant::Decoded(d)) => {
                                format!("Decoded(port={})", d.portnum)
                            }
                            Some(protobufs::mesh_packet::PayloadVariant::Encrypted(e)) => {
                                format!("Encrypted({} bytes)", e.len())
                            }
                            None => "None".to_string(),
                        };
                        tracing::info!(
                            "Mesh packet from={} to={} channel={} payload={pv}",
                            mesh_packet.from,
                            mesh_packet.to,
                            mesh_packet.channel
                        );
                        handle_incoming_packet(
                            mesh_packet,
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
        Some(protobufs::mesh_packet::PayloadVariant::Encrypted(e)) => {
            tracing::warn!(
                "Received encrypted packet from {} ({} bytes) — cannot process. \
                 Both devices must share the same Meshtastic channel PSK.",
                packet.from,
                e.len()
            );
            return;
        }
        None => return,
    };

    let portnum = decoded.portnum;
    let private_app = protobufs::PortNum::PrivateApp as i32;

    if portnum != private_app {
        tracing::debug!(
            "Ignoring packet from {} with portnum={portnum} (want {private_app}=PrivateApp)",
            packet.from
        );
        return;
    }

    let from_node = packet.from;
    let payload = &decoded.payload;

    if payload.is_empty() {
        tracing::debug!("Empty PrivateApp payload from {from_node}");
        return;
    }

    tracing::info!(
        "PrivateApp message from node {from_node} ({} bytes)",
        payload.len()
    );

    let keys = session_keys.lock().await;
    if let Some(key) = keys.get(&from_node) {
        match crate::protocol::MeshMessage::decrypt_envelope(payload, key) {
            Ok(msg) => {
                tracing::info!("Decrypted envelope from {from_node}: {:?}", msg.id());
                match msg {
                    crate::protocol::MeshMessage::Text {
                        id, text, timestamp,
                    } => {
                        tracing::info!(
                            "Incoming text from {from_node}: \"{}\" (id={id})",
                            text.chars().take(40).collect::<String>()
                        );
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
                        tracing::info!(
                            "Pair request from {sender_name} (node {from_node}) — already have key"
                        );
                    }
                }
                return;
            }
            Err(e) => {
                tracing::warn!(
                    "Envelope decryption failed from known peer {from_node}: {e} — key mismatch?"
                );
            }
        }
    } else {
        tracing::info!(
            "No session key for node {from_node} — treating as pair request"
        );
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
