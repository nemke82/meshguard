pub mod ble;
pub mod ble_plugin;
pub mod commands;
pub mod crypto;
pub mod device_config;
pub mod error;
pub mod protocol;
pub mod state;

use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(ble_plugin::init())
        .setup(|app| {
            let config_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));

            let state = AppState::new(config_dir);
            app.manage(state);

            tracing::info!("MeshGuard started — configure your device to begin");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::check_bluetooth,
            commands::scan_devices,
            commands::save_device_config,
            commands::get_device_config,
            commands::has_device,
            commands::remove_device,
            commands::setup_peer,
            commands::get_peer_config,
            commands::connect_local_device,
            commands::disconnect_local_device,
            commands::is_connected,
            commands::apply_config_to_device,
            commands::send_message,
            commands::has_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running MeshGuard");
}
