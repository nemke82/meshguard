pub mod ble_plugin;
pub mod commands;
pub mod crypto;
pub mod device_config;
pub mod error;
pub mod mesh_radio;
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

            tracing::info!("MeshGuard started — scan for a device to begin");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::scan_ble_devices,
            commands::get_serial_ports,
            commands::connect_device,
            commands::connect_tcp,
            commands::connect_serial,
            commands::disconnect_device,
            commands::is_connected,
            commands::get_mesh_nodes,
            commands::get_my_device_info,
            commands::start_chat,
            commands::accept_chat,
            commands::send_message,
            commands::has_session,
            commands::list_peers,
            commands::remove_peer,
            commands::get_last_connection,
        ])
        .run(tauri::generate_context!())
        .expect("error while running MeshGuard");
}
