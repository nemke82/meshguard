//! Tauri plugin for BLE scanning.
//!
//! - **Desktop**: uses btleplug (Rust).
//! - **Android**: delegates to native Kotlin via `register_android_plugin`.
//!
//! The Kotlin side (`BlePlugin.kt`) is injected by `scripts/patch-android.sh`.

use serde::Serialize;
use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

use crate::ble::{self, BleManager, BluetoothStatus};
use crate::error::MeshGuardError;

/// Wrapper so both Rust and Kotlin return `{ devices: [...] }`.
#[derive(Serialize)]
struct ScanResponse {
    devices: Vec<crate::ble::ScannedDevice>,
}

#[tauri::command]
async fn check_bluetooth() -> Result<BluetoothStatus, MeshGuardError> {
    // On Android this is never called — Kotlin handles it.
    Ok(ble::check_bluetooth().await)
}

#[tauri::command]
async fn scan_devices() -> Result<ScanResponse, MeshGuardError> {
    // On Android this is never called — Kotlin handles it.
    let ble = BleManager::new().await?;
    let devices = ble.scan(5).await?;
    Ok(ScanResponse { devices })
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("ble-scanner")
        .invoke_handler(tauri::generate_handler![check_bluetooth, scan_devices])
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            _api.register_android_plugin("com.meshguard.app", "BlePlugin")?;
            Ok(())
        })
        .build()
}
