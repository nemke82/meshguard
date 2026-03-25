use serde::{Deserialize, Serialize};
use tauri::{Runtime, State};

use crate::ble::BleManager;
use crate::crypto;
use crate::device_config::{DeviceConfig, PeerConfig, RadioConfig};
use crate::error::MeshGuardError;
use crate::state::AppState;

// ── BLE response types ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BluetoothStatusResponse {
    pub adapter_found: bool,
    pub powered_on: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedDeviceInfo {
    pub name: String,
    pub address: String,
    pub rssi: Option<i32>,
    pub is_meshtastic: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ScanResponse {
    pub devices: Vec<ScannedDeviceInfo>,
}

// ── Bluetooth Status ──────────────────────────────────────────

#[tauri::command]
pub async fn check_bluetooth<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<BluetoothStatusResponse, MeshGuardError> {
    do_check_bluetooth(app).await
}

#[cfg(target_os = "android")]
async fn do_check_bluetooth<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<BluetoothStatusResponse, MeshGuardError> {
    use tauri::Manager;
    app.state::<crate::ble_plugin::BlePluginState<R>>()
        .0
        .run_mobile_plugin("checkBluetooth", ())
        .map_err(|e| MeshGuardError::Ble(format!("Android BLE check: {e}")))
}

#[cfg(not(target_os = "android"))]
async fn do_check_bluetooth<R: Runtime>(
    _app: tauri::AppHandle<R>,
) -> Result<BluetoothStatusResponse, MeshGuardError> {
    let s = crate::ble::check_bluetooth().await;
    Ok(BluetoothStatusResponse {
        adapter_found: s.adapter_found,
        powered_on: s.powered_on,
        message: s.message,
    })
}

// ── BLE Scanning ──────────────────────────────────────────────

#[tauri::command]
pub async fn scan_devices<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<ScanResponse, MeshGuardError> {
    do_scan_devices(app).await
}

#[cfg(target_os = "android")]
async fn do_scan_devices<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<ScanResponse, MeshGuardError> {
    use tauri::Manager;
    app.state::<crate::ble_plugin::BlePluginState<R>>()
        .0
        .run_mobile_plugin("scanDevices", ())
        .map_err(|e| MeshGuardError::Ble(format!("Android BLE scan: {e}")))
}

#[cfg(not(target_os = "android"))]
async fn do_scan_devices<R: Runtime>(
    _app: tauri::AppHandle<R>,
) -> Result<ScanResponse, MeshGuardError> {
    let ble = BleManager::new().await?;
    let devices = ble.scan(5).await?;
    Ok(ScanResponse {
        devices: devices
            .into_iter()
            .map(|d| ScannedDeviceInfo {
                name: d.name,
                address: d.address,
                rssi: d.rssi.map(|r| r as i32),
                is_meshtastic: d.is_meshtastic,
            })
            .collect(),
    })
}

// ── Device Setup ──────────────────────────────────────────────

/// Input for save_device_config command.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceConfigInput {
    pub device_name: String,
    pub ble_address: String,
    pub region: String,
    pub modem_preset: String,
    pub tx_power: u8,
    pub hop_limit: u8,
}

/// Save local device configuration (name, serial, BLE address, radio settings).
/// Rejects if a device is already configured — only one device allowed.
#[tauri::command]
pub async fn save_device_config(
    input: DeviceConfigInput,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let region = parse_region(&input.region)?;
    let modem = parse_modem(&input.modem_preset)?;

    let mut config = state.config.lock().await;

    // Block adding a second device — only one allowed
    if let Some(existing) = &config.device {
        if existing.ble_address != input.ble_address && !existing.ble_address.is_empty() {
            return Err(MeshGuardError::InvalidConfig(
                "A device is already configured. Remove it first before adding a new one.".into(),
            ));
        }
    }

    let device = config.device.get_or_insert_with(|| DeviceConfig {
        ble_address: String::new(),
        device_name: String::new(),
        radio: RadioConfig::default(),
        channel: crate::device_config::ChannelConfig::default(),
    });

    device.ble_address = input.ble_address;
    device.device_name = input.device_name;
    device.radio.region = region;
    device.radio.modem_preset = modem;
    device.radio.tx_power = input.tx_power;
    device.radio.hop_limit = input.hop_limit;

    config.save(&state.config_dir)?;
    Ok(())
}

/// Get current device config.
#[tauri::command]
pub async fn get_device_config(
    state: State<'_, AppState>,
) -> Result<Option<DeviceConfig>, MeshGuardError> {
    let config = state.config.lock().await;
    Ok(config.device.clone())
}

/// Check if a device is already configured.
#[tauri::command]
pub async fn has_device(state: State<'_, AppState>) -> Result<bool, MeshGuardError> {
    let config = state.config.lock().await;
    Ok(config.device.is_some())
}

/// Remove the configured device (allows adding a new one).
#[tauri::command]
pub async fn remove_device(state: State<'_, AppState>) -> Result<(), MeshGuardError> {
    // Disconnect first if connected
    let mut ble = state.ble.lock().await;
    if let Some(manager) = ble.as_ref() {
        let _ = manager.disconnect().await;
    }
    *ble = None;
    drop(ble);

    // Clear session key
    *state.session_key.lock().await = None;

    // Remove device and peer from config
    let mut config = state.config.lock().await;
    config.device = None;
    config.peer = None;
    config.save(&state.config_dir)?;

    tracing::info!("Device removed — ready for new device setup");
    Ok(())
}

// ── P2P Pairing ───────────────────────────────────────────────

/// Set up P2P pairing — enter peer device name and shared passphrase.
#[tauri::command]
pub async fn setup_peer(
    peer_device_name: String,
    shared_passphrase: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let mut config = state.config.lock().await;

    let device = config
        .device
        .as_ref()
        .ok_or(MeshGuardError::InvalidConfig(
            "configure your local device first".into(),
        ))?;

    let psk = crypto::derive_channel_psk(
        &device.device_name,
        &peer_device_name,
        &shared_passphrase,
    )?;

    let session_key = crypto::derive_p2p_key(
        &device.device_name,
        &peer_device_name,
        &shared_passphrase,
    )?;

    if let Some(ref mut dev) = config.device {
        dev.channel.psk = psk;
        dev.channel.name = format!("MG-{}", &peer_device_name.chars().take(6).collect::<String>());
        dev.channel.uplink = false;
        dev.channel.downlink = false;
    }

    config.peer = Some(PeerConfig {
        device_name: peer_device_name,
        shared_passphrase: String::new(),
    });

    config.save(&state.config_dir)?;
    *state.session_key.lock().await = Some(session_key);

    tracing::info!("P2P pairing complete — session key derived");
    Ok(())
}

/// Get current peer config (without passphrase).
#[tauri::command]
pub async fn get_peer_config(
    state: State<'_, AppState>,
) -> Result<Option<PeerConfig>, MeshGuardError> {
    let config = state.config.lock().await;
    Ok(config.peer.clone())
}

// ── Connection ────────────────────────────────────────────────

/// Connect to the local Meshtastic device via BLE.
#[tauri::command]
pub async fn connect_local_device(
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let config = state.config.lock().await;
    let device = config
        .device
        .as_ref()
        .ok_or(MeshGuardError::InvalidConfig("no device configured".into()))?;
    let address = device.ble_address.clone();
    drop(config);

    let mut ble = state.ble.lock().await;
    // Reuse existing manager if available, otherwise create new
    if ble.is_none() {
        *ble = Some(BleManager::new().await?);
    }
    let manager = ble.as_ref().unwrap();
    manager.connect_to_address(&address).await?;

    Ok(())
}

/// Disconnect from local device.
#[tauri::command]
pub async fn disconnect_local_device(
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let mut ble = state.ble.lock().await;
    if let Some(manager) = ble.as_ref() {
        manager.disconnect().await?;
    }
    *ble = None;
    Ok(())
}

/// Check if local device is connected.
#[tauri::command]
pub async fn is_connected(state: State<'_, AppState>) -> Result<bool, MeshGuardError> {
    let ble = state.ble.lock().await;
    match ble.as_ref() {
        Some(manager) => Ok(manager.is_connected().await),
        None => Ok(false),
    }
}

/// Push the current config (radio + channel with PSK) to the Meshtastic device.
#[tauri::command]
pub async fn apply_config_to_device(
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let config = state.config.lock().await;
    let device = config
        .device
        .as_ref()
        .ok_or(MeshGuardError::InvalidConfig("no device configured".into()))?;

    let config_summary = serde_json::to_vec(device)
        .map_err(|e| MeshGuardError::Serialization(e.to_string()))?;

    let ble = state.ble.lock().await;
    let manager = ble.as_ref().ok_or(MeshGuardError::NotConnected)?;
    manager.write_config(&config_summary).await?;

    tracing::info!("Device config applied: region={:?}, channel={}", device.radio.region, device.channel.name);
    Ok(())
}

// ── Messaging ─────────────────────────────────────────────────

/// Send an encrypted message to the peer via the Meshtastic mesh.
#[tauri::command]
pub async fn send_message(
    text: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let session = state.session_key.lock().await;
    let session_key = session.as_ref().ok_or(MeshGuardError::NoSession)?;

    let msg = crate::protocol::MeshMessage::new_text(&text, session_key)?;
    let data = msg.to_bytes()?;

    let ble = state.ble.lock().await;
    let manager = ble.as_ref().ok_or(MeshGuardError::NotConnected)?;
    manager.write_to_radio(&data).await
}

/// Check if a P2P session is active (keys derived).
#[tauri::command]
pub async fn has_session(state: State<'_, AppState>) -> Result<bool, MeshGuardError> {
    let session = state.session_key.lock().await;
    Ok(session.is_some())
}

// ── Helpers ───────────────────────────────────────────────────

fn parse_region(s: &str) -> Result<crate::device_config::LoraRegion, MeshGuardError> {
    use crate::device_config::LoraRegion::*;
    match s {
        "US" => Ok(US),
        "EU868" => Ok(EU868),
        "EU433" => Ok(EU433),
        "CN" => Ok(CN),
        "JP" => Ok(JP),
        "ANZ" => Ok(ANZ),
        "KR" => Ok(KR),
        "TW" => Ok(TW),
        "RU" => Ok(RU),
        "IN" => Ok(IN),
        "NZ865" => Ok(NZ865),
        "TH" => Ok(TH),
        "UA868" => Ok(UA868),
        "UA433" => Ok(UA433),
        _ => Err(MeshGuardError::InvalidConfig(format!("unknown region: {}", s))),
    }
}

fn parse_modem(s: &str) -> Result<crate::device_config::ModemPreset, MeshGuardError> {
    use crate::device_config::ModemPreset::*;
    match s {
        "LongRange" => Ok(LongRange),
        "LongModerate" => Ok(LongModerate),
        "MediumRange" => Ok(MediumRange),
        "ShortRange" => Ok(ShortRange),
        "ShortFast" => Ok(ShortFast),
        _ => Err(MeshGuardError::InvalidConfig(format!("unknown modem preset: {}", s))),
    }
}
