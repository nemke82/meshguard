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

// ── BLE Bonding ──────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct BondResult {
    pub success: bool,
    pub message: String,
}

/// Bond (pair) with a Meshtastic device via BLE.
/// On Android this triggers the system pairing dialog.
/// On desktop, bonding happens automatically during connect.
#[tauri::command]
pub async fn bond_device<R: Runtime>(
    app: tauri::AppHandle<R>,
    address: String,
    state: State<'_, AppState>,
) -> Result<BondResult, MeshGuardError> {
    do_bond_device(app, address, state).await
}

#[cfg(target_os = "android")]
async fn do_bond_device<R: Runtime>(
    app: tauri::AppHandle<R>,
    address: String,
    state: State<'_, AppState>,
) -> Result<BondResult, MeshGuardError> {
    use tauri::Manager;
    #[derive(Serialize)]
    struct BondArgs {
        address: String,
    }
    let result: BondResult = app
        .state::<crate::ble_plugin::BlePluginState<R>>()
        .0
        .run_mobile_plugin("bondDevice", BondArgs { address: address.clone() })
        .map_err(|e| MeshGuardError::Ble(format!("Android BLE bond: {e}")))?;

    if result.success {
        let mut config = state.config.lock().await;
        if let Some(ref mut dev) = config.device {
            dev.bonded = true;
        }
        config.save(&state.config_dir)?;
    }
    Ok(result)
}

#[cfg(not(target_os = "android"))]
async fn do_bond_device<R: Runtime>(
    _app: tauri::AppHandle<R>,
    address: String,
    state: State<'_, AppState>,
) -> Result<BondResult, MeshGuardError> {
    // On desktop, btleplug handles bonding during connect().
    // We try connecting to verify the device is reachable and bonding works.
    let ble = BleManager::new().await?;
    match ble.connect_to_address(&address).await {
        Ok(_) => {
            let _ = ble.disconnect().await;
            let mut config = state.config.lock().await;
            if let Some(ref mut dev) = config.device {
                dev.bonded = true;
            }
            config.save(&state.config_dir)?;
            Ok(BondResult {
                success: true,
                message: "Device paired successfully.".into(),
            })
        }
        Err(e) => Ok(BondResult {
            success: false,
            message: format!("Pairing failed: {}", e),
        }),
    }
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

/// Save local device configuration.
/// Rejects if a different device is already configured — only one device allowed.
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
        bonded: false,
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

    // Clear all session keys
    state.session_keys.lock().await.clear();
    *state.active_peer_id.lock().await = None;

    // Remove device and all peers from config
    let mut config = state.config.lock().await;
    config.device = None;
    config.peers.clear();
    config.save(&state.config_dir)?;

    tracing::info!("Device removed — ready for new device setup");
    Ok(())
}

// ── Multi-Peer Management ─────────────────────────────────────

/// Add a new peer and derive the session key.
/// Returns the peer's ID for future reference.
#[tauri::command]
pub async fn add_peer(
    peer_device_name: String,
    shared_passphrase: String,
    state: State<'_, AppState>,
) -> Result<PeerConfig, MeshGuardError> {
    let mut config = state.config.lock().await;

    let device = config
        .device
        .as_ref()
        .ok_or(MeshGuardError::InvalidConfig(
            "configure your local device first".into(),
        ))?;

    // Check for duplicate peer name
    if config.peers.iter().any(|p| p.device_name == peer_device_name) {
        return Err(MeshGuardError::InvalidConfig(
            format!("A peer named '{}' already exists.", peer_device_name),
        ));
    }

    let session_key = crypto::derive_p2p_key(
        &device.device_name,
        &peer_device_name,
        &shared_passphrase,
    )?;

    let peer_id = uuid::Uuid::new_v4().to_string();
    let peer = PeerConfig {
        id: peer_id.clone(),
        device_name: peer_device_name.clone(),
    };

    config.peers.push(peer.clone());
    config.save(&state.config_dir)?;

    // Store session key in memory
    state.session_keys.lock().await.insert(peer_id.clone(), session_key);

    // Auto-select as active peer
    *state.active_peer_id.lock().await = Some(peer_id);

    // Update channel PSK for the new active peer
    let psk = crypto::derive_channel_psk(
        &config.device.as_ref().unwrap().device_name,
        &peer_device_name,
        &shared_passphrase,
    )?;
    if let Some(ref mut dev) = config.device {
        dev.channel.psk = psk;
        dev.channel.name = format!("MG-{}", &peer_device_name.chars().take(6).collect::<String>());
        dev.channel.uplink = false;
        dev.channel.downlink = false;
    }
    config.save(&state.config_dir)?;

    tracing::info!("Peer added: {} — session key derived", peer_device_name);
    Ok(peer)
}

/// Remove a peer by ID.
#[tauri::command]
pub async fn remove_peer(
    peer_id: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let mut config = state.config.lock().await;
    config.peers.retain(|p| p.id != peer_id);
    config.save(&state.config_dir)?;

    // Remove session key
    state.session_keys.lock().await.remove(&peer_id);

    // Clear active peer if it was the removed one
    let mut active = state.active_peer_id.lock().await;
    if active.as_deref() == Some(&peer_id) {
        *active = None;
    }

    tracing::info!("Peer removed: {}", peer_id);
    Ok(())
}

/// List all configured peers.
#[tauri::command]
pub async fn list_peers(
    state: State<'_, AppState>,
) -> Result<Vec<PeerConfig>, MeshGuardError> {
    let config = state.config.lock().await;
    Ok(config.peers.clone())
}

/// Activate a peer for messaging. Requires passphrase to derive session key
/// if not already in memory.
#[tauri::command]
pub async fn activate_peer(
    peer_id: String,
    shared_passphrase: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let config = state.config.lock().await;

    let device = config
        .device
        .as_ref()
        .ok_or(MeshGuardError::InvalidConfig("no device configured".into()))?;

    let peer = config
        .peers
        .iter()
        .find(|p| p.id == peer_id)
        .ok_or(MeshGuardError::InvalidConfig("peer not found".into()))?;

    let session_key = crypto::derive_p2p_key(
        &device.device_name,
        &peer.device_name,
        &shared_passphrase,
    )?;

    state.session_keys.lock().await.insert(peer_id.clone(), session_key);
    *state.active_peer_id.lock().await = Some(peer_id);

    tracing::info!("Peer activated: {}", peer.device_name);
    Ok(())
}

/// Check if a peer has an active session key in memory.
#[tauri::command]
pub async fn peer_has_session(
    peer_id: String,
    state: State<'_, AppState>,
) -> Result<bool, MeshGuardError> {
    let keys = state.session_keys.lock().await;
    Ok(keys.contains_key(&peer_id))
}

/// Get the currently active peer ID.
#[tauri::command]
pub async fn get_active_peer(
    state: State<'_, AppState>,
) -> Result<Option<String>, MeshGuardError> {
    let active = state.active_peer_id.lock().await;
    Ok(active.clone())
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

/// Send an encrypted message to the active peer via the Meshtastic mesh.
#[tauri::command]
pub async fn send_message(
    text: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let active_peer = state.active_peer_id.lock().await;
    let peer_id = active_peer.as_ref().ok_or(MeshGuardError::NoSession)?;

    let keys = state.session_keys.lock().await;
    let session_key = keys.get(peer_id).ok_or(MeshGuardError::NoSession)?;

    let msg = crate::protocol::MeshMessage::new_text(&text, session_key)?;
    let data = msg.to_bytes()?;

    let ble = state.ble.lock().await;
    let manager = ble.as_ref().ok_or(MeshGuardError::NotConnected)?;
    manager.write_to_radio(&data).await
}

/// Check if any P2P session is active (active peer with derived key).
#[tauri::command]
pub async fn has_session(state: State<'_, AppState>) -> Result<bool, MeshGuardError> {
    let active = state.active_peer_id.lock().await;
    if let Some(peer_id) = active.as_ref() {
        let keys = state.session_keys.lock().await;
        Ok(keys.contains_key(peer_id))
    } else {
        Ok(false)
    }
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
