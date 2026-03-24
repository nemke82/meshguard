use tauri::State;

use crate::ble::BleManager;
use crate::crypto;
use crate::device_config::{DeviceConfig, PeerConfig, RadioConfig};
use crate::error::MeshGuardError;
use crate::state::AppState;

// ── Device Setup ──────────────────────────────────────────────

/// Input for save_device_config command.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceConfigInput {
    pub device_name: String,
    pub device_serial: String,
    pub ble_address: String,
    pub region: String,
    pub modem_preset: String,
    pub tx_power: u8,
    pub hop_limit: u8,
}

/// Save local device configuration (name, serial, BLE address, radio settings).
#[tauri::command]
pub async fn save_device_config(
    input: DeviceConfigInput,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let region = parse_region(&input.region)?;
    let modem = parse_modem(&input.modem_preset)?;

    let mut config = state.config.lock().await;
    let device = config.device.get_or_insert_with(|| DeviceConfig {
        ble_address: String::new(),
        device_name: String::new(),
        device_serial: String::new(),
        radio: RadioConfig::default(),
        channel: crate::device_config::ChannelConfig::default(),
    });

    device.ble_address = input.ble_address;
    device.device_name = input.device_name;
    device.device_serial = input.device_serial;
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

// ── P2P Pairing ───────────────────────────────────────────────

/// Set up P2P pairing — enter peer device name, serial, and shared passphrase.
/// This derives the encryption key and channel PSK. Both sides must enter
/// the same info to get the same keys.
#[tauri::command]
pub async fn setup_peer(
    peer_device_name: String,
    peer_device_serial: String,
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

    // Derive the channel PSK from pairing info
    let psk = crypto::derive_channel_psk(
        &device.device_name,
        &device.device_serial,
        &peer_device_name,
        &peer_device_serial,
        &shared_passphrase,
    )?;

    // Derive the session encryption key
    let session_key = crypto::derive_p2p_key(
        &device.device_name,
        &device.device_serial,
        &peer_device_name,
        &peer_device_serial,
        &shared_passphrase,
    )?;

    // Update channel config with derived PSK
    if let Some(ref mut dev) = config.device {
        dev.channel.psk = psk;
        dev.channel.name = format!("MG-{}", &peer_device_name.chars().take(6).collect::<String>());
        dev.channel.uplink = false;
        dev.channel.downlink = false;
    }

    // Save peer config (passphrase is NOT saved to disk)
    config.peer = Some(PeerConfig {
        device_name: peer_device_name,
        device_serial: peer_device_serial,
        shared_passphrase: String::new(), // Never persist the passphrase
    });

    config.save(&state.config_dir)?;

    // Store session key in memory
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

    let ble_manager = BleManager::new().await?;
    ble_manager.connect_to_address(&address).await?;

    *state.ble.lock().await = Some(ble_manager);
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

    // Build Meshtastic config protobuf (simplified — real impl uses prost-generated types)
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
