use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
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

use std::path::PathBuf;

use meshguard_core::status::GeoFix;
use meshguard_engine::nodes;
use meshguard_engine::receive::{Action, Event};

use crate::error::MeshGuardError;
use crate::state::{lock_engine, MeshNodeInfo, SharedEngine};

pub use meshguard_engine::receive::{
    IncomingMessageEvent, IncomingStatusEvent, MessageDeliveredEvent, PairRequestEvent,
    PeerProtocolEvent,
};

/// The BLE service UUID that all Meshtastic firmware advertises.
const MESHTASTIC_SERVICE_UUID: uuid::Uuid = uuid::Uuid::from_bytes([
    0x6b, 0xa1, 0xb2, 0x18, 0x15, 0xa8, 0x46, 0x1f, 0x9f, 0xa8, 0x5d, 0xca, 0xe2, 0x73, 0xea, 0xfd,
]);

const FROMRADIO_UUID: uuid::Uuid = uuid::Uuid::from_bytes([
    0x2c, 0x55, 0xe6, 0x9e, 0x49, 0x93, 0x11, 0xed, 0xb8, 0x78, 0x02, 0x42, 0xac, 0x12, 0x00, 0x02,
]);

const TORADIO_UUID: uuid::Uuid = uuid::Uuid::from_bytes([
    0xf7, 0x5c, 0x76, 0xd2, 0x12, 0x9e, 0x4d, 0xad, 0xa1, 0xdd, 0x78, 0x66, 0x12, 0x44, 0x01, 0xe7,
]);

/// Known name prefixes for Meshtastic-firmware devices.
const MESHTASTIC_NAME_HINTS: &[&str] = &[
    "meshtastic",
    "sensecap",
    "t1000",
    "rak",
    "heltec",
    "tbeam",
    "t-beam",
    "tlora",
    "t-lora",
    "station-g",
    "nano-g",
    "wio-tracker",
    "trackerd",
    "meshcore",
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
    MESHTASTIC_NAME_HINTS
        .iter()
        .any(|hint| lower.contains(hint))
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
                let disc = adapter
                    .discover_devices()
                    .await
                    .map_err(|e| MeshGuardError::Ble(format!("Re-scan failed: {e}")))?;
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

fn find_char(
    chars: &std::collections::BTreeSet<Characteristic>,
    uuid: uuid::Uuid,
) -> Result<Characteristic, MeshGuardError> {
    chars
        .iter()
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

    let radio = target.ok_or_else(|| {
        MeshGuardError::Ble(format!("Device '{device_name}' not found during BLE scan"))
    })?;

    let mac_address = radio.address().to_string();

    // On Linux: pair via BlueZ BEFORE connecting with btleplug.
    // bluer's device.pair() connects + pairs in one step, creating an
    // encrypted link. btleplug's connect() then reuses that connection.
    pair_ble_device(&mac_address, pin).await?;

    tracing::info!("Connecting to BLE device: {device_name} ({mac_address})");
    radio
        .connect()
        .await
        .map_err(|e| MeshGuardError::Ble(format!("BLE connect failed: {e}")))?;

    tracing::info!("Discovering GATT services...");
    radio
        .discover_services()
        .await
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
            radio
                .connect()
                .await
                .map_err(|e| MeshGuardError::Ble(format!("BLE reconnect failed: {e}")))?;
            radio
                .discover_services()
                .await
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
            match tokio::time::timeout(Duration::from_millis(50), server.read(&mut write_buf)).await
            {
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
                        match radio
                            .write(&toradio_char, payload, WriteType::WithResponse)
                            .await
                        {
                            Ok(()) => {
                                ok = true;
                                break;
                            }
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
    allow_bond: bool,
) -> Result<(StreamHandle<DuplexStream>, i64), MeshGuardError> {
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
                    serde_json::json!({ "address": addr, "pin": pin, "allowBond": allow_bond }),
                )
                .map_err(|e| MeshGuardError::Ble(format!("Android BLE connect failed: {e}")))
        }
    })
    .await
    .map_err(|e| MeshGuardError::Ble(format!("BLE connect task panicked: {e}")))?;

    // The plugin is a singleton; every GATT connection gets a session id so
    // this link's I/O thread can never read from, write to or close a newer
    // connection.
    let session = connect_result?
        .get("session")
        .and_then(|s| s.as_i64())
        .unwrap_or(-1);
    tracing::info!("Android BLE connected — GATT ready for {addr_for_log} (session {session})");

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

        'io: loop {
            // Write any pending data to toradio
            loop {
                match proto_rx.try_recv() {
                    Ok(data) => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                        if let Err(e) = plugin.run_mobile_plugin::<serde_json::Value>(
                            "writeToRadio",
                            serde_json::json!({ "data": b64, "session": session }),
                        ) {
                            tracing::error!("Android BLE write failed: {e}");
                            break 'io;
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    // The transport was shut down (reconnect/disconnect).
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'io,
                }
            }

            // Read from fromradio
            match plugin.run_mobile_plugin::<serde_json::Value>(
                "readFromRadio",
                serde_json::json!({ "session": session }),
            ) {
                Ok(response) => {
                    if let Some(data_str) = response.get("data").and_then(|d| d.as_str()) {
                        if !data_str.is_empty() {
                            if let Ok(data) =
                                base64::engine::general_purpose::STANDARD.decode(data_str)
                            {
                                if !data.is_empty() && ble_tx.send(data).is_err() {
                                    break 'io;
                                }
                                continue;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(75));
                }
                Err(e) => {
                    tracing::error!("Android BLE read failed: {e}");
                    break 'io;
                }
            }
        }

        tracing::warn!("Android BLE I/O thread ended (session {session})");
        // Only closes the GATT connection if it is still this session's.
        let _ = plugin.run_mobile_plugin::<serde_json::Value>(
            "disconnectBleDevice",
            serde_json::json!({ "session": session }),
        );
    });

    // Bridge task: channels ↔ duplex stream
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut server = server;
        let mut write_buf = [0u8; 512];

        loop {
            tokio::select! {
                msg = ble_rx.recv() => {
                    // None: the BLE I/O thread ended (link lost).
                    let Some(data) = msg else { break };
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

    Ok((
        StreamHandle {
            stream: client,
            join_handle: Some(handle),
        },
        session,
    ))
}

/// The config download is done: leave the fast BLE connection interval
/// requested by `connectDevice`, which costs the radio's battery. Only
/// touches the GATT connection of `session`.
#[cfg(target_os = "android")]
async fn relax_android_ble_link(app_handle: &tauri::AppHandle, session: i64) {
    let app = app_handle.clone();
    let res = tokio::task::spawn_blocking(move || {
        use tauri::Manager;
        let state = app.state::<crate::ble_plugin::BlePluginState<tauri::Wry>>();
        state.0.run_mobile_plugin::<serde_json::Value>(
            "setLinkPriority",
            serde_json::json!({ "high": false, "session": session }),
        )
    })
    .await;
    if let Ok(Err(e)) = res {
        tracing::debug!("BLE link priority not changed: {e}");
    }
}

// ── PacketRouter ──────────────────────────────────────────────

pub struct MeshGuardRouter {
    node_id: u32,
    /// Id of the last packet sent with echo_response (see send_traceroute).
    last_sent_id: Option<u32>,
}

impl MeshGuardRouter {
    pub fn new(node_id: u32) -> Self {
        Self {
            node_id,
            last_sent_id: None,
        }
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

    fn handle_mesh_packet(&mut self, packet: protobufs::MeshPacket) -> Result<(), RouterError> {
        // Only called for packets we send with echo_response: remember the
        // id so a reply (Data.request_id) can be matched to its request.
        self.last_sent_id = Some(packet.id);
        Ok(())
    }

    fn source_node_id(&self) -> NodeId {
        self.node_id.into()
    }
}

// ── MeshRadio ─────────────────────────────────────────────────

pub struct ConnectParams {
    pub app_handle: tauri::AppHandle,
    /// Keys, peers, nodes, replay protection and history.
    pub engine: SharedEngine,
    /// The radio slot in `AppState`, so the listener can transmit ACKs.
    pub radio: Arc<Mutex<Option<MeshRadio>>>,
    pub config_dir: PathBuf,
    /// Generation of this link (see `link.rs`).
    pub link_gen: u64,
    pub link: Arc<crate::link::LinkManager>,
    /// Vault key for restoring remembered chat keys (ADR-002), if available.
    pub vault: Option<Arc<crate::crypto::SessionKey>>,
    /// Latest GPS fix of our own radio.
    pub my_position: Arc<Mutex<Option<GeoFix>>>,
    pub node_history: Arc<Mutex<crate::diagnostics::History>>,
    pub path_tests: Arc<Mutex<crate::diagnostics::PathTests>>,
    /// Refuse the link unless this node answers (a saved radio's address
    /// may now belong to another device). Checked before any state changes.
    pub expect_node: Option<u32>,
}

/// Everything the background listener needs; cheap to clone.
#[derive(Clone)]
struct ListenerCtx {
    app_handle: tauri::AppHandle,
    engine: SharedEngine,
    my_node_num: u32,
    radio: Arc<Mutex<Option<MeshRadio>>>,
    link_gen: u64,
    link: Arc<crate::link::LinkManager>,
    my_position: Arc<Mutex<Option<GeoFix>>>,
    node_history: Arc<Mutex<crate::diagnostics::History>>,
    path_tests: Arc<Mutex<crate::diagnostics::PathTests>>,
}

impl ListenerCtx {
    fn new(p: &ConnectParams, my_node_num: u32) -> Self {
        Self {
            app_handle: p.app_handle.clone(),
            engine: p.engine.clone(),
            my_node_num,
            radio: p.radio.clone(),
            link_gen: p.link_gen,
            link: p.link.clone(),
            my_position: p.my_position.clone(),
            node_history: p.node_history.clone(),
            path_tests: p.path_tests.clone(),
        }
    }
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
    let mut positions: HashMap<u32, protobufs::Position> = HashMap::new();
    let mut nodes: HashMap<u32, MeshNodeInfo> = HashMap::new();
    let mut packet_count: u32 = 0;

    loop {
        let timeout_secs = if packet_count == 0 { 30 } else { 10 };

        match tokio::time::timeout(Duration::from_secs(timeout_secs), decoded_listener.recv()).await
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
                            let node =
                                nodes::from_node_info(node_info, chrono::Utc::now().timestamp());
                            if Some(node.node_num) == found_node_num {
                                if let Some(ref user) = node_info.user {
                                    found_device_name = Some(user.long_name.clone());
                                }
                            }
                            if let Some(pos) = &node_info.position {
                                positions.insert(node.node_num, *pos);
                            }
                            nodes.insert(node.node_num, node);
                        }
                        PayloadVariant::ConfigCompleteId(id) => {
                            tracing::info!(
                                "Config complete (id={id}), received {packet_count} packets"
                            );
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
                tracing::warn!(
                    "Config timeout after {timeout_secs}s ({packet_count} packets received)"
                );
                break;
            }
        }
    }

    let node_num = found_node_num
        .ok_or_else(|| MeshGuardError::MeshRadio("Radio did not report MyNodeInfo".into()))?;

    // Before touching keys, nodes or the connection state: a different radio
    // must not replace the one we meant to reach.
    if let Some(want) = p.expect_node {
        if node_num != want {
            if let Err(e) = configured_api.disconnect().await {
                tracing::debug!("Wrong radio's transport shutdown: {e}");
            }
            return Err(MeshGuardError::MeshRadio(format!(
                "A different radio (!{node_num:08x}) answered instead of !{want:08x}"
            )));
        }
    }

    nodes.remove(&node_num);

    // Our radio's last known GPS fix (a new radio starts from scratch).
    *p.my_position.lock().await = None;
    if let Some(pos) = positions.get(&node_num) {
        update_my_position(&p.my_position, pos).await;
    }

    {
        let mut engine = lock_engine(&p.engine);
        engine.nodes = nodes;
        engine.my_node_num = Some(node_num);
        engine.my_device_name = found_device_name;
    }

    emit_connection_state(&p.app_handle, "connected");
    let _ = p.app_handle.emit("mesh-nodes-updated", ());

    Ok((node_num, configured_api))
}

impl MeshRadio {
    /// Stop the transport's worker tasks (reader, writer, processor).
    pub async fn shutdown(self) {
        if let Err(e) = self.api.disconnect().await {
            tracing::debug!("Radio transport shutdown: {e}");
        }
    }

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
        allow_bond: bool,
        p: ConnectParams,
    ) -> Result<Self, MeshGuardError> {
        emit_connection_state(&p.app_handle, "connecting");

        // `android_session`: the GATT session, to relax the link after config.
        let (ble_stream, android_session) = {
            #[cfg(target_os = "android")]
            {
                let address = ble_address.ok_or_else(|| {
                    MeshGuardError::Ble("Device address required for Android BLE".into())
                })?;
                let (stream, session) =
                    build_android_ble_stream(&p.app_handle, &address, pin, allow_bond).await?;
                (stream, Some(session))
            }
            #[cfg(not(target_os = "android"))]
            {
                // Desktop pairing uses the PIN agent, never an interactive dialog.
                let _ = (ble_address, allow_bond);
                stop_any_active_scan().await?;
                (build_polling_ble_stream(ble_name, pin).await?, None::<i64>)
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

        #[cfg(target_os = "android")]
        if let Some(session) = android_session {
            relax_android_ble_link(&p.app_handle, session).await;
        }
        #[cfg(not(target_os = "android"))]
        let _ = android_session;

        crate::key_store::prepare_for_radio(&p, node_num).await;
        spawn_listener(decoded_listener, ListenerCtx::new(&p, node_num));

        Ok(Self::from_configured(configured_api, node_num))
    }

    /// Connect via TCP / WiFi.
    pub async fn connect_tcp(address: &str, p: ConnectParams) -> Result<Self, MeshGuardError> {
        emit_connection_state(&p.app_handle, "connecting");

        let stream_api = StreamApi::new();
        let tcp_stream = utils::stream::build_tcp_stream(address.to_string())
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

        crate::key_store::prepare_for_radio(&p, node_num).await;
        spawn_listener(decoded_listener, ListenerCtx::new(&p, node_num));

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

        crate::key_store::prepare_for_radio(&p, node_num).await;
        spawn_listener(decoded_listener, ListenerCtx::new(&p, node_num));

        Ok(Self::from_configured(configured_api, node_num))
    }

    /// Send a Meshtastic traceroute request to `destination_node`; the reply
    /// arrives as a TRACEROUTE_APP packet from that node.
    /// Returns the request's packet id.
    pub async fn send_traceroute(&mut self, destination_node: u32) -> Result<u32, MeshGuardError> {
        use meshtastic::Message as _;
        let body = protobufs::RouteDiscovery::default().encode_to_vec();
        self.router.last_sent_id = None;
        self.api
            .send_mesh_packet(
                &mut self.router,
                EncodedMeshPacketData::new(body),
                protobufs::PortNum::TracerouteApp,
                PacketDestination::Node(destination_node.into()),
                MeshChannel::new(0).unwrap(),
                false,
                true,
                // Echo the sent packet to our router to learn its id.
                true,
                None,
                None,
            )
            .await
            .map_err(|e| MeshGuardError::MeshRadio(format!("Path test failed: {e}")))?;
        self.router
            .last_sent_id
            .take()
            .ok_or_else(|| MeshGuardError::MeshRadio("Path test id unknown".into()))
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

async fn update_my_position(slot: &Mutex<Option<GeoFix>>, pos: &protobufs::Position) {
    let mut current = slot.lock().await;
    *current = nodes::newest_fix(*current, pos);
}

/// A traceroute reply: report the route if we have a test outstanding.
async fn handle_path_test_reply(ctx: &ListenerCtx, from: u32, data: &protobufs::Data) {
    use meshtastic::Message as _;
    // Replies carry our request's id; incoming requests and stale replies
    // do not match.
    if !ctx.link.is_current(ctx.link_gen) {
        return;
    }
    let Ok(rd) = protobufs::RouteDiscovery::decode(data.payload.as_slice()) else {
        return;
    };
    let Some(rtt) = ctx.path_tests.lock().await.finish(from, data.request_id) else {
        return; // not ours, late, or already reported
    };
    let result = crate::diagnostics::path_result(
        ctx.my_node_num,
        from,
        &rd.route,
        &rd.snr_towards,
        &rd.route_back,
        &rd.snr_back,
        rtt,
    );
    tracing::info!(
        "Path test to {from}: {} hop(s) out, {} back, {} ms",
        result.towards.len(),
        result.back.len(),
        result.rtt_ms
    );
    let _ = ctx.app_handle.emit("path-test-result", result);
}

fn spawn_listener(mut listener: meshtastic::packet::PacketReceiver, ctx: ListenerCtx) {
    tokio::spawn(async move {
        let mut pkt_counter: u64 = 0;
        while let Some(from_radio) = listener.recv().await {
            if !ctx.link.is_current(ctx.link_gen) {
                tracing::debug!("Listener of a replaced link stopping");
                break;
            }
            let Some(variant) = &from_radio.payload_variant else {
                continue;
            };
            pkt_counter += 1;
            let vname = variant_name(variant);
            if vname != "QueueStatus" {
                tracing::debug!("Listener #{pkt_counter}: {vname}");
            }
            if let PayloadVariant::Packet(mesh_packet) = variant {
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
            }
            let now = chrono::Utc::now().timestamp().max(0) as u64;
            let actions = lock_engine(&ctx.engine).on_from_radio(&from_radio, ctx.my_node_num, now);
            run_actions(&ctx, actions, now).await;
        }
        tracing::warn!("Mesh radio listener ended — radio disconnected");
        crate::link::on_link_lost(ctx.app_handle.clone(), ctx.link_gen);
    });
}

/// Carry out what the engine decided for one packet.
async fn run_actions(ctx: &ListenerCtx, actions: Vec<Action>, now: u64) {
    for action in actions {
        match action {
            Action::SendAck { to, frame } => send_ack(ctx, to, frame).await,
            Action::Emit(event) => emit_event(&ctx.app_handle, event),
            Action::Notify { title, body, node } => {
                #[cfg(target_os = "android")]
                show_android_notification(&ctx.app_handle, &title, &body, node);
                #[cfg(not(target_os = "android"))]
                let _ = (title, body, node);
            }
            Action::MyPosition(pos) => update_my_position(&ctx.my_position, &pos).await,
            Action::PathReply { from, data } => handle_path_test_reply(ctx, from, &data).await,
            Action::Sample { node, sample } => ctx.node_history.lock().await.record(node, sample),
            Action::PeerAck { peer, message_id } => {
                crate::history::record_peer_ack(&ctx.app_handle, ctx.my_node_num, peer, message_id)
                    .await;
                let _ = ctx.app_handle.emit(
                    "message-delivered",
                    MessageDeliveredEvent {
                        peer_node: peer,
                        message_id: message_id.to_hex(),
                    },
                );
            }
            Action::Undecryptable { from, payload } => {
                // A device showing a QR code accepts requests sealed with it.
                if crate::commands::try_accept_qr(&ctx.app_handle, from, &payload).await {
                    continue;
                }
                let more = lock_engine(&ctx.engine).note_pair_request(from, payload, now);
                Box::pin(run_actions(ctx, more, now)).await;
            }
            Action::AnnounceCaps { peer } => {
                let app = ctx.app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    use tauri::Manager;
                    let state = app.state::<crate::state::AppState>();
                    crate::status_tx::announce_caps(&state, peer).await;
                });
            }
            // The relay client (ADR-006 P2e) follows topic changes; control
            // messages go out with the outbox loop.
            Action::RelayTopicsChanged | Action::RelayControlsDue => {}
            Action::ProbePeers => {
                let app = ctx.app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    use tauri::Manager;
                    let state = app.state::<crate::state::AppState>();
                    crate::status_tx::probe_peers(&state).await;
                });
            }
        }
    }
}

/// The UI refreshes its node list on "mesh-nodes-updated". On a busy mesh
/// almost every packet changes a node, so send it at most this often.
const NODES_UPDATED_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

struct NodesThrottle {
    last: Option<std::time::Instant>,
    /// A delayed emit is already scheduled; it will carry this change too.
    scheduled: bool,
}

static NODES_THROTTLE: std::sync::Mutex<NodesThrottle> = std::sync::Mutex::new(NodesThrottle {
    last: None,
    scheduled: false,
});

/// Emit "mesh-nodes-updated" now, or once at the end of the current interval
/// if one was sent less than `NODES_UPDATED_INTERVAL` ago. The last change is
/// never dropped.
fn emit_nodes_updated(app: &tauri::AppHandle) {
    let now = std::time::Instant::now();
    let wait = {
        let mut t = NODES_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
        if t.scheduled {
            return;
        }
        match t.last {
            Some(last) if now.duration_since(last) < NODES_UPDATED_INTERVAL => {
                t.scheduled = true;
                NODES_UPDATED_INTERVAL - now.duration_since(last)
            }
            _ => {
                t.last = Some(now);
                std::time::Duration::ZERO
            }
        }
    };
    if wait.is_zero() {
        let _ = app.emit("mesh-nodes-updated", ());
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(wait).await;
        {
            let mut t = NODES_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
            t.scheduled = false;
            t.last = Some(std::time::Instant::now());
        }
        let _ = app.emit("mesh-nodes-updated", ());
    });
}

/// Send an engine event to the UI.
pub fn emit_event(app: &tauri::AppHandle, event: Event) {
    let name = event.name();
    let _ = match event {
        Event::NodesUpdated => {
            emit_nodes_updated(app);
            Ok(())
        }
        Event::IncomingMessage(e) => app.emit(name, e),
        Event::IncomingStatus(e) => app.emit(name, e),
        Event::PeerProtocol(e) => app.emit(name, e),
        Event::PairRequest(e) | Event::PairAccepted(e) => app.emit(name, e),
        Event::RelayChanged(e) => app.emit(name, e),
    };
}

#[cfg(target_os = "android")]
fn show_android_notification(
    app_handle: &tauri::AppHandle,
    title: &str,
    body: &str,
    node_num: u32,
) {
    use tauri::Manager;
    if let Some(state) = app_handle.try_state::<crate::ble_plugin::BlePluginState<tauri::Wry>>() {
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "nodeNum": node_num,
        });
        match state
            .0
            .run_mobile_plugin::<serde_json::Value>("showNotification", payload)
        {
            Ok(_) => tracing::debug!("Android notification shown"),
            Err(e) => tracing::warn!("Failed to show Android notification: {e}"),
        }
    }
}

/// Transmit an application ACK (the engine already charged the airtime
/// budget), only through the link the frame it answers came from.
async fn send_ack(ctx: &ListenerCtx, to_node: u32, frame: Vec<u8>) {
    if !ctx.link.is_current(ctx.link_gen) {
        return;
    }
    let mut radio = ctx.radio.lock().await;
    // Re-check under the radio lock: the link may have been replaced while
    // we waited, and an ACK must only leave through the radio it is for.
    if !ctx.link.is_current(ctx.link_gen) {
        return;
    }
    match radio.as_mut() {
        Some(r) => {
            if let Err(e) = r.send_private_app(frame, to_node).await {
                tracing::warn!("ACK to {to_node} failed: {e}");
            }
        }
        None => tracing::debug!("ACK to {to_node} skipped: radio not stored yet"),
    }
}

fn emit_connection_state(app_handle: &tauri::AppHandle, state: &str) {
    let _ = app_handle.emit("connection-state", state);
}
