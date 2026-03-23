pub mod ble;
pub mod commands;
pub mod crypto;
pub mod error;
pub mod protocol;
pub mod state;

use state::AppState;
use uuid::Uuid;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let device_id = Uuid::new_v4().to_string()[..8].to_string();

            // BLE initialization happens async; we spawn it
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match ble::BleManager::new().await {
                    Ok(ble_manager) => {
                        let state = AppState::new(ble_manager, device_id);
                        handle.manage(state);
                        tracing::info!("MeshGuard BLE initialized");
                    }
                    Err(e) => {
                        tracing::error!("Failed to initialize BLE: {}", e);
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::scan_devices,
            commands::connect_device,
            commands::disconnect_device,
            commands::is_connected,
            commands::start_secure_session,
            commands::send_message,
            commands::get_device_id,
        ])
        .run(tauri::generate_context!())
        .expect("error while running MeshGuard");
}
