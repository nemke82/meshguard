use tauri::State;

use crate::ble::DeviceInfo;
use crate::error::MeshGuardError;
use crate::state::AppState;

/// Scan for nearby Meshtastic devices.
#[tauri::command]
pub async fn scan_devices(state: State<'_, AppState>) -> Result<Vec<DeviceInfo>, MeshGuardError> {
    let ble = state.ble.lock().await;
    ble.scan(5).await
}

/// Connect to a Meshtastic device by its BLE address.
#[tauri::command]
pub async fn connect_device(
    address: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let ble = state.ble.lock().await;
    ble.connect(&address).await
}

/// Disconnect from the current device.
#[tauri::command]
pub async fn disconnect_device(state: State<'_, AppState>) -> Result<(), MeshGuardError> {
    let ble = state.ble.lock().await;
    ble.disconnect().await
}

/// Check connection status.
#[tauri::command]
pub async fn is_connected(state: State<'_, AppState>) -> Result<bool, MeshGuardError> {
    let ble = state.ble.lock().await;
    Ok(ble.is_connected().await)
}

/// Initiate a secure P2P session with key exchange.
#[tauri::command]
pub async fn start_secure_session(
    state: State<'_, AppState>,
) -> Result<String, MeshGuardError> {
    let identity = state.identity.lock().await;
    let pub_key = identity.as_ref()
        .ok_or(MeshGuardError::Protocol("no identity generated".into()))?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        pub_key.public_key.as_bytes(),
    ))
}

/// Send an encrypted message to the connected peer.
#[tauri::command]
pub async fn send_message(
    text: String,
    state: State<'_, AppState>,
) -> Result<(), MeshGuardError> {
    let session = state.session_key.lock().await;
    let session_key = session
        .as_ref()
        .ok_or(MeshGuardError::Protocol("no active session".into()))?;

    let msg =
        crate::protocol::MeshMessage::new_encrypted_text(&state.device_id, &text, session_key)?;

    let data = msg.to_bytes()?;
    let ble = state.ble.lock().await;
    ble.send_to_radio(&data).await
}

/// Get the local device identity (public key fingerprint).
#[tauri::command]
pub async fn get_device_id(state: State<'_, AppState>) -> Result<String, MeshGuardError> {
    Ok(state.device_id.clone())
}
