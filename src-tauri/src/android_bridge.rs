//! JNI entry points for the Kotlin `CoreBridge` (ADR-001 option D).
//!
//! The Android foreground service loads this same library and reaches the
//! process-wide engine through these functions, without starting Tauri.
//! The surface is small and string/byte based: results are JSON. Decrypted
//! content is never logged on either side.
//!
//! Step 3A (the §6 gate) covers: `version`, `init`, `selfTest`, `peers` and
//! `enqueueReply`. Radio packets (`onFromRadio`) follow once the service
//! owns the GATT link (step 3B).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;
use meshguard_engine::outbox::{Attempt, Content};
use serde_json::json;

use crate::crypto::SessionKey;
use crate::state::{lock_engine, process_engine};

/// `init` result bits.
const INIT_HISTORY_OPEN: jint = 1;
const INIT_KEYS_RESTORED: jint = 2;
const INIT_FAILED: jint = -1;

fn to_jstring(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s)
        .map(|j| j.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

/// The engine of this process, if `init` (or the app) created it.
fn engine() -> Option<crate::state::SharedEngine> {
    ENGINE_DIR.get().map(|dir| process_engine(dir))
}

static ENGINE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[no_mangle]
pub extern "system" fn Java_com_meshguard_app_CoreBridge_version<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    let v = format!("meshguard {} (engine)", env!("CARGO_PKG_VERSION"));
    to_jstring(&mut env, &v)
}

/// Open the engine on `data_dir` (the app's data directory, the same
/// folder the Tauri app uses) with the vault key unwrapped by Kotlin, if
/// there is one yet. Opens the history and restores the chat keys of the
/// radio used last, so replies can be queued without the app. Idempotent.
/// Returns `INIT_*` bits, or -1 on failure.
#[no_mangle]
pub extern "system" fn Java_com_meshguard_app_CoreBridge_init<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    data_dir: JString<'l>,
    vault: JByteArray<'l>,
) -> jint {
    let dir: String = match env.get_string(&data_dir) {
        Ok(s) => s.into(),
        Err(_) => return INIT_FAILED,
    };
    let vault_key = if vault.is_null() {
        None
    } else {
        env.convert_byte_array(&vault)
            .ok()
            .map(zeroize::Zeroizing::new)
            .and_then(|raw| <[u8; 32]>::try_from(raw.as_slice()).ok())
            .map(SessionKey::from_bytes)
    };
    catch_unwind(AssertUnwindSafe(|| {
        let dir = PathBuf::from(dir);
        let _ = ENGINE_DIR.set(dir.clone());
        let engine = process_engine(&dir);
        let mut e = lock_engine(&engine);
        let mut bits = 0;
        // Without a vault key yet, leave the history undecided: the app
        // creates the vault on its first connect and opens it then.
        if let Some(v) = &vault_key {
            e.open_history(Some(v));
            if e.keys_owner.is_none() {
                if let Some(node) = e.last_radio() {
                    let peers: Vec<u32> = e.config.peers.iter().map(|p| p.node_num).collect();
                    let loaded = crate::key_store::load_for(&dir, v, node, &peers);
                    let (restored, _) = e.adopt_keys(node, loaded);
                    if restored > 0 {
                        bits |= INIT_KEYS_RESTORED;
                    }
                }
            }
        }
        if e.history_saved() {
            bits |= INIT_HISTORY_OPEN;
        }
        tracing::info!("CoreBridge init: {bits:#x}");
        bits
    }))
    .unwrap_or(INIT_FAILED)
}

/// Decode the shared v2 fixture frame (gate step 1). JSON
/// `{"ok": true, "text": ...}` or `{"ok": false, "error": ...}`.
#[no_mangle]
pub extern "system" fn Java_com_meshguard_app_CoreBridge_selfTest<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    let out = catch_unwind(meshguard_engine::gate::self_test)
        .unwrap_or_else(|_| Err("panic".into()));
    let v = match out {
        Ok(text) => json!({ "ok": true, "text": text }),
        Err(e) => json!({ "ok": false, "error": e }),
    };
    to_jstring(&mut env, &v.to_string())
}

/// Paired contacts with a chat key on this radio: JSON
/// `[{"node": n, "name": "..."}]`.
#[no_mangle]
pub extern "system" fn Java_com_meshguard_app_CoreBridge_peers<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
) -> jstring {
    let list = catch_unwind(|| {
        let Some(engine) = engine() else {
            return json!([]);
        };
        let e = lock_engine(&engine);
        let peers: Vec<_> = e
            .config
            .peers
            .iter()
            .filter(|p| e.keys.contains_key(&p.node_num))
            .map(|p| json!({ "node": p.node_num, "name": p.device_name }))
            .collect();
        json!(peers)
    })
    .unwrap_or_else(|_| json!([]));
    to_jstring(&mut env, &list.to_string())
}

/// Queue a reply typed into a notification. It is stored in the outbox
/// before anything goes to a radio (the app's outbox sends it when its
/// radio is connected). JSON `{"ok": true, "messageId", "state"}` or
/// `{"ok": false, "error"}`.
#[no_mangle]
pub extern "system" fn Java_com_meshguard_app_CoreBridge_enqueueReply<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    peer: jint,
    text: JString<'l>,
) -> jstring {
    let text: String = match env.get_string(&text) {
        Ok(s) => s.into(),
        Err(_) => return to_jstring(&mut env, r#"{"ok":false,"error":"bad text"}"#),
    };
    let v = catch_unwind(AssertUnwindSafe(|| {
        let Some(engine) = engine() else {
            return json!({ "ok": false, "error": "not initialised" });
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            return json!({ "ok": false, "error": "empty" });
        }
        let now = now_secs();
        let mut e = lock_engine(&engine);
        match e.enqueue(
            peer as u32,
            &Content::Text(text),
            now,
            meshguard_engine::outbox::Routes::default(),
        ) {
            Ok(t) if t.conversation.is_none() => json!({
                "ok": false,
                "error": "Message history is not open yet, so the reply cannot be saved. Open MeshGuard once.",
            }),
            Ok(t) => {
                // No radio here: release the in-flight mark right away so
                // the outbox picks it up as soon as a radio is connected.
                let (state, _) = e.finish_attempt(&t, Attempt::Unavailable, now);
                json!({
                    "ok": true,
                    "messageId": t.message_id.to_hex(),
                    "state": state.map(|s| s.as_str()).unwrap_or("queued"),
                })
            }
            Err(err) => json!({ "ok": false, "error": err.to_string() }),
        }
    }))
    .unwrap_or_else(|_| json!({ "ok": false, "error": "panic" }));
    to_jstring(&mut env, &v.to_string())
}
