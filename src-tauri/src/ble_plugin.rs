//! Tauri plugin for Android BLE.
//!
//! On Android, this plugin registers the native Kotlin `BlePlugin` class and
//! stores the `PluginHandle` so that regular Tauri commands in `commands.rs`
//! can call `run_mobile_plugin()` to reach Kotlin via JNI.
//!
//! On desktop, this plugin does nothing — BLE is handled by btleplug directly.
//!
//! The Kotlin `BlePlugin.kt` is injected by `scripts/patch-android.sh`.

use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

/// Holds the PluginHandle used to call Kotlin via JNI on Android.
#[cfg(target_os = "android")]
pub struct BlePluginState<R: Runtime>(pub tauri::plugin::PluginHandle<R>);

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    // No invoke_handler — commands are registered in the main invoke_handler
    // (commands.rs) so they get "core:default" permissions automatically.
    Builder::new("ble-scanner")
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
