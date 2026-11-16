#[cfg(target_os = "android")]
pub mod android_bridge;
pub mod ble_plugin;
pub mod commands;
pub mod crypto;
pub mod dedup_store;
pub mod device_config;
pub mod diagnostics;
pub mod discovery;
pub mod error;
pub mod history;
pub mod key_store;
pub mod link;
pub mod mesh_radio;
pub mod protocol;
pub mod state;
pub mod status_tx;

use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(ble_plugin::init());

    builder
        .setup(|app| {
            let config_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));

            let state = AppState::new(config_dir);
            app.manage(state);
            history::spawn_outbox_loop(app.handle().clone());

            tracing::info!("MeshGuard started — scan for a device to begin");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::scan_ble_devices,
            commands::move_to_background,
            commands::block_node,
            commands::unblock_node,
            commands::list_blocked,
            commands::terms_accepted,
            commands::accept_terms,
            commands::get_serial_ports,
            commands::connect_device,
            commands::connect_tcp,
            commands::connect_serial,
            commands::scan_wifi_radios,
            commands::get_saved_links,
            commands::forget_saved_link,
            commands::connect_saved_link,
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
            commands::get_last_link,
            commands::auto_connect,
            commands::set_background_service,
            commands::get_status_presets,
            commands::set_status_presets,
            commands::reset_status_presets,
            commands::get_my_position,
            commands::send_status,
            commands::create_pair_offer,
            commands::cancel_pair_offer,
            commands::pair_with_code,
            commands::get_node_history,
            commands::run_path_test,
            history::get_history_info,
            history::list_conversations,
            history::get_messages,
            history::mark_conversation_read,
            history::delete_conversation,
        ])
        .run(tauri::generate_context!())
        .expect("error while running MeshGuard");
}
