//! Tauri plugin for BLE scanning.
//!
//! - **Desktop**: uses btleplug (Rust).
//! - **Android**: Rust commands call `run_mobile_plugin()` → Kotlin `BlePlugin`
//!   via JNI. The Kotlin side uses Android's native `BluetoothLeScanner`.
//!
//! The Kotlin `BlePlugin.kt` is injected by `scripts/patch-android.sh`.

use serde::{Deserialize, Serialize};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

use crate::error::MeshGuardError;

// ── Response types (shared between desktop and Android) ─────────

/// Bluetooth adapter status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BluetoothStatusResponse {
    pub adapter_found: bool,
    pub powered_on: bool,
    pub message: String,
}

/// A discovered BLE device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedDeviceInfo {
    pub name: String,
    pub address: String,
    pub rssi: Option<i32>,
    pub is_meshtastic: bool,
}

/// Wrapper so both Rust and Kotlin return `{ devices: [...] }`.
#[derive(Serialize, Deserialize)]
pub struct ScanResponse {
    pub devices: Vec<ScannedDeviceInfo>,
}

// ── Android plugin handle ───────────────────────────────────────

/// Holds the PluginHandle used to call Kotlin via JNI on Android.
#[cfg(target_os = "android")]
struct BlePluginState<R: Runtime>(tauri::plugin::PluginHandle<R>);

// ── Commands ────────────────────────────────────────────────────

#[tauri::command]
async fn check_bluetooth<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<BluetoothStatusResponse, MeshGuardError> {
    do_check_bluetooth(app).await
}

#[tauri::command]
async fn scan_devices<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<ScanResponse, MeshGuardError> {
    do_scan_devices(app).await
}

// ── Android: delegate to Kotlin via run_mobile_plugin ───────────

#[cfg(target_os = "android")]
async fn do_check_bluetooth<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<BluetoothStatusResponse, MeshGuardError> {
    use tauri::Manager;
    app.state::<BlePluginState<R>>()
        .0
        .run_mobile_plugin("checkBluetooth", ())
        .map_err(|e| MeshGuardError::Ble(format!("Android BLE check failed: {e}")))
}

#[cfg(target_os = "android")]
async fn do_scan_devices<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<ScanResponse, MeshGuardError> {
    use tauri::Manager;
    app.state::<BlePluginState<R>>()
        .0
        .run_mobile_plugin("scanDevices", ())
        .map_err(|e| MeshGuardError::Ble(format!("Android BLE scan failed: {e}")))
}

// ── Desktop: use btleplug ───────────────────────────────────────

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

#[cfg(not(target_os = "android"))]
async fn do_scan_devices<R: Runtime>(
    _app: tauri::AppHandle<R>,
) -> Result<ScanResponse, MeshGuardError> {
    let ble = crate::ble::BleManager::new().await?;
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

// ── Plugin init ─────────────────────────────────────────────────

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("ble-scanner")
        .invoke_handler(tauri::generate_handler![check_bluetooth, scan_devices])
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            {
                use tauri::Manager;
                let handle =
                    api.register_android_plugin("com.meshguard.app", "BlePlugin")?;
                app.manage(BlePluginState(handle));
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = (app, api);
            }
            Ok(())
        })
        .build()
}
