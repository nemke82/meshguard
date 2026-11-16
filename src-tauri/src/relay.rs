//! Private MQTT relay in the app (ADR-006): owns the one broker connection
//! of this process, feeds relay frames to the engine, publishes outgoing
//! frames and ACKs, and exposes the settings and per-contact commands.
//!
//! The relay is off by default. Nothing here touches the radio link: a
//! broker, TLS or network failure only affects relay attempts.

use std::collections::BTreeSet;
use std::sync::Arc;

use meshguard_engine::config::{RelayPreference, RelaySettings};
use meshguard_engine::outbox::{Attempt, Transmit};
use meshguard_engine::receive::{Action, Route};
use meshguard_engine::relay::ContactRelay;
use meshguard_mqtt::{
    parse_broker_address, BrokerAddress, BrokerSettings, PublishError, RelayClient, RelayStatus,
};
use serde::Serialize;
use tauri::{Emitter, Manager};
use zeroize::Zeroizing;

use crate::error::MeshGuardError;
use crate::state::AppState;

/// Lifetime of a relay ACK at the broker, seconds.
const ACK_EXPIRY_SECS: u32 = 600;
/// How long "Test connection" waits for an answer.
const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

struct Running {
    client: Arc<RelayClient>,
    tasks: Vec<tauri::async_runtime::JoinHandle<()>>,
    /// The inbound topics last handed to the client.
    topics: BTreeSet<String>,
}

/// The relay of this process (one broker session, ADR-006 §7).
#[derive(Default)]
pub struct RelayManager {
    running: tokio::sync::Mutex<Option<Running>>,
    /// Gate shared with cancellation (block, remove): the send generation
    /// is checked under it and held through the hand-over to the broker.
    pub send_gate: tokio::sync::Mutex<()>,
    status: std::sync::Mutex<StatusView>,
}

/// Relay connection state for the UI (`relay-status` event).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StatusView {
    /// "disabled", "not_configured", "connecting", "connected",
    /// "auth_failed", "tls_error", "disconnected".
    pub state: String,
    pub detail: Option<String>,
}

impl Default for StatusView {
    fn default() -> Self {
        Self {
            state: "disabled".into(),
            detail: None,
        }
    }
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

impl RelayManager {
    fn set_status(&self, app: &tauri::AppHandle, view: StatusView) {
        let mut cur = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if *cur != view {
            *cur = view.clone();
            let _ = app.emit("relay-status", view);
        }
    }

    pub fn status(&self) -> StatusView {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn client(&self) -> Option<Arc<RelayClient>> {
        self.running.lock().await.as_ref().map(|r| r.client.clone())
    }

    /// Whether the relay is connected to the broker now.
    pub async fn connected(&self) -> bool {
        match self.client().await {
            Some(c) => c.status() == RelayStatus::Connected,
            None => false,
        }
    }
}

fn status_view(s: &RelayStatus) -> StatusView {
    let detail = match s {
        RelayStatus::TlsError(d) | RelayStatus::Disconnected(d) => Some(d.clone()),
        RelayStatus::AuthFailed => Some("The broker refused the username or password".into()),
        _ => None,
    };
    StatusView {
        state: s.as_str().into(),
        detail,
    }
}

/// Broker settings from the engine's config and relay book, if complete.
fn broker_settings(
    state: &AppState,
) -> Option<(BrokerSettings, BTreeSet<String>, BTreeSet<String>)> {
    let engine = state.engine();
    let s = engine.config.relay.as_ref()?;
    let password = engine.relay.password.clone()?;
    let settings = BrokerSettings {
        address: BrokerAddress {
            host: s.host.clone(),
            port: s.port,
        },
        username: s.username.clone(),
        password: Zeroizing::new(password),
        ca_pem: s.ca_pem.clone(),
        client_id: s.client_id.clone(),
        session_expiry_secs: s.session_expiry_secs,
    };
    let topics = engine
        .relay_inbound_topics()
        .into_keys()
        .chain(engine.relay_retiring_topics().into_keys())
        .collect();
    let unsubscribe = engine.relay.unsubscribe.clone();
    Some((settings, topics, unsubscribe))
}

/// Make sure chat keys are loaded when no radio has connected yet, so the
/// relay can serve the contacts of the last radio (ADR-006 §6).
async fn ensure_keys(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let (owner, last, peers) = {
        let e = state.engine();
        let peers: Vec<u32> = e.config.peers.iter().map(|p| p.node_num).collect();
        (e.keys_owner, e.last_radio(), peers)
    };
    if owner.is_some() {
        return;
    }
    let Some(last) = last else { return };
    let Some(vault) = state.vault(app).await else {
        return;
    };
    let dir = state.config_dir.clone();
    let loaded =
        tokio::task::spawn_blocking(move || crate::key_store::load_for(&dir, &vault, last, &peers))
            .await
            .unwrap_or_default();
    let mut e = state.engine();
    if e.keys_owner.is_none() {
        let (n, _) = e.adopt_keys(last, loaded);
        tracing::info!("Relay: using {n} chat key(s) of the last radio");
    }
}

/// (Re)start the relay from the saved settings, or stop it when it is off.
pub async fn restart(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let mgr = &state.relay;
    // Under the send gate: no publish is in progress, and every later one
    // sees the new client (or none).
    let _gate = mgr.send_gate.lock().await;
    let mut running = mgr.running.lock().await;
    if let Some(old) = running.take() {
        for t in &old.tasks {
            t.abort();
        }
        // Stops the connection even if someone still holds the Arc.
        old.client.shutdown().await;
    }
    crate::history::ensure_open(app).await;
    let enabled = state
        .engine()
        .config
        .relay
        .as_ref()
        .is_some_and(|r| r.enabled);
    if !enabled {
        mgr.set_status(app, StatusView::default());
        return;
    }
    ensure_keys(app).await;
    let Some((settings, topics, unsubscribe)) = broker_settings(&state) else {
        mgr.set_status(
            app,
            StatusView {
                state: "not_configured".into(),
                detail: None,
            },
        );
        return;
    };
    let (client, mut rx) = match RelayClient::start(&settings, topics.clone(), unsubscribe) {
        Ok(c) => c,
        Err(e) => {
            mgr.set_status(
                app,
                StatusView {
                    state: "not_configured".into(),
                    detail: Some(e.to_string()),
                },
            );
            return;
        }
    };
    let client = Arc::new(client);
    let mut tasks = Vec::new();
    // Incoming frames go through the engine like radio packets.
    let a = app.clone();
    tasks.push(tauri::async_runtime::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let now = now_secs();
            let actions =
                a.state::<AppState>()
                    .engine()
                    .on_relay_frame(&frame.topic, &frame.payload, now);
            run_actions(&a, actions).await;
        }
    }));
    // Status for the UI.
    let a = app.clone();
    let mut watch = client.watch_status();
    tasks.push(tauri::async_runtime::spawn(async move {
        loop {
            let s = watch.borrow_and_update().clone();
            a.state::<AppState>().relay.set_status(&a, status_view(&s));
            if watch.changed().await.is_err() {
                break;
            }
        }
    }));
    *running = Some(Running {
        client,
        tasks,
        topics,
    });
}

/// Hand the current inbound topics to the client and remember the topics
/// still to be removed from the broker session (survives restarts).
pub async fn sync_topics(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let mut running = state.relay.running.lock().await;
    let Some(r) = running.as_mut() else { return };
    let (topics, unsubscribe) = {
        let mut e = state.engine();
        let now = now_secs();
        if let Err(err) = e.update_relay(|b| b.prune_retiring(now)) {
            tracing::warn!("Relay state not saved: {err}");
        }
        let topics: BTreeSet<String> = e
            .relay_inbound_topics()
            .into_keys()
            .chain(e.relay_retiring_topics().into_keys())
            .collect();
        (topics, e.relay.unsubscribe.clone())
    };
    if topics != r.topics && r.client.set_topics(topics.clone()).is_ok() {
        r.topics = topics;
    }
    let _ = r.client.add_unsubscribes(unsubscribe);
    let pending = r.client.pending_unsubscribes();
    let mut engine = state.engine();
    if engine.relay.unsubscribe != pending {
        if let Err(e) = engine.update_relay(|b| b.unsubscribe = pending) {
            tracing::warn!("Relay unsubscribe list not saved: {e}");
        }
    }
}

fn attempt_of(r: &Result<(), PublishError>) -> Attempt {
    match r {
        Ok(()) => Attempt::Submitted,
        Err(PublishError::Unavailable | PublishError::Busy) => Attempt::Unavailable,
        Err(_) => Attempt::Failed,
    }
}

/// Publish an outbox attempt on the relay. The broker's PUBACK means *sent
/// to broker*; only the peer's AppAck means delivered.
pub async fn publish(app: &tauri::AppHandle, t: &Transmit, now: u64) -> Attempt {
    let state = app.state::<AppState>();
    let _gate = state.relay.send_gate.lock().await;
    let Some(client) = state.relay.client().await else {
        return Attempt::Unavailable;
    };
    let topic = {
        let e = state.engine();
        if !e.still_sendable(t) {
            return Attempt::Unavailable;
        }
        // The relay must still be active with this contact, on this topic.
        match (e.relay_outbound_topic(t.to), &t.topic) {
            (Some(cur), Some(topic)) if cur == *topic => cur,
            _ => return Attempt::Unavailable,
        }
    };
    let expiry = t
        .expires_at
        .saturating_sub(now)
        .clamp(1, u64::from(u32::MAX)) as u32;
    attempt_of(&client.publish(&topic, &t.frame, expiry).await)
}

/// Send a relay control message over the relay (contacts the relay is
/// already active with), reserving the attempt first like the radio path.
pub async fn publish_control(
    app: &tauri::AppHandle,
    c: &meshguard_engine::relay::ControlTransmit,
    now: u64,
) -> bool {
    let state = app.state::<AppState>();
    let _gate = state.relay.send_gate.lock().await;
    let Some(client) = state.relay.client().await else {
        return false;
    };
    if client.status() != RelayStatus::Connected {
        return false;
    }
    let topic = {
        let mut e = state.engine();
        // A revoke keeps the topic of the relay it ends.
        let Some(topic) = e.relay_outbound_topic(c.peer).or_else(|| c.topic.clone()) else {
            return false;
        };
        if !e.relay_reserve_control(c, now) {
            return true; // handled: nothing (more) to send
        }
        topic
    };
    if let Err(e) = client.publish(&topic, &c.frame, (24 * 3600) as u32).await {
        tracing::info!("Relay control to {} not published: {e}", c.peer);
    }
    true
}

/// Carry out what the engine decided for a relay frame.
async fn run_actions(app: &tauri::AppHandle, actions: Vec<Action>) {
    let state = app.state::<AppState>();
    for action in actions {
        match action {
            Action::SendAck {
                to,
                frame,
                route: Route::Relay,
                reply: Some(reply),
            } => {
                // Same gate as block/remove: re-checked right before the
                // hand-over to the broker.
                let _gate = state.relay.send_gate.lock().await;
                let Some(client) = state.relay.client().await else {
                    continue;
                };
                if !state.engine().relay_reply_valid(to, &reply) {
                    continue;
                }
                if let Err(e) = client.publish(&reply.topic, &frame, ACK_EXPIRY_SECS).await {
                    tracing::info!("Relay ACK to {to} not published: {e}");
                }
            }
            Action::Emit(event) => crate::mesh_radio::emit_event(app, event),
            Action::Notify { title, body, node } => {
                crate::mesh_radio::notify(app, &title, &body, node);
            }
            Action::PeerAck { peer, message_id } => {
                let my_node = state.engine().keys_owner;
                if let Some(my_node) = my_node {
                    crate::history::record_peer_ack(app, my_node, peer, message_id).await;
                    let _ = app.emit(
                        "message-delivered",
                        meshguard_engine::receive::MessageDeliveredEvent {
                            peer_node: peer,
                            message_id: message_id.to_hex(),
                        },
                    );
                }
            }
            Action::RelayTopicsChanged => sync_topics(app).await,
            Action::AnnounceCaps { peer } => {
                crate::status_tx::announce_caps(&state, peer).await;
            }
            Action::RelayControlsDue
            | Action::SendAck { .. }
            | Action::MyPosition(_)
            | Action::PathReply { .. }
            | Action::Sample { .. }
            | Action::Undecryptable { .. }
            | Action::ProbePeers => {}
        }
    }
}

// ── Commands ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RelaySettingsView {
    pub configured: bool,
    pub enabled: bool,
    /// `mqtts://host:port`.
    pub broker: String,
    pub username: String,
    pub has_password: bool,
    pub ca_pem: Option<String>,
    pub session_days: u32,
    pub preference: RelayPreference,
    pub status: StatusView,
    /// A secure key store exists (otherwise the relay cannot be used).
    pub vault: bool,
}

#[tauri::command]
pub async fn relay_get_settings(
    app_handle: tauri::AppHandle,
) -> Result<RelaySettingsView, MeshGuardError> {
    crate::history::ensure_open(&app_handle).await;
    let state = app_handle.state::<AppState>();
    let vault = state.vault(&app_handle).await.is_some();
    let e = state.engine();
    let s = e.config.relay.clone();
    Ok(RelaySettingsView {
        configured: s.is_some() && e.relay.password.is_some(),
        enabled: s.as_ref().is_some_and(|s| s.enabled),
        broker: s
            .as_ref()
            .map(|s| {
                BrokerAddress {
                    host: s.host.clone(),
                    port: s.port,
                }
                .to_string()
            })
            .unwrap_or_default(),
        username: s.as_ref().map(|s| s.username.clone()).unwrap_or_default(),
        has_password: e.relay.password.is_some(),
        ca_pem: s.as_ref().and_then(|s| s.ca_pem.clone()),
        session_days: s
            .as_ref()
            .map(|s| s.session_expiry_secs / 86_400)
            .unwrap_or(3),
        preference: s.as_ref().map(|s| s.preference).unwrap_or_default(),
        status: state.relay.status(),
        vault,
    })
}

/// A new opaque broker username for the administrator to create.
#[tauri::command]
pub fn relay_new_username() -> String {
    meshguard_core::relay::suggest_username()
}

/// Save the relay settings. `password: None` keeps the saved one. The
/// broker and username cannot change while contacts use the relay (their
/// topics depend on them).
#[tauri::command]
pub async fn relay_save_settings(
    broker: String,
    username: String,
    password: Option<String>,
    ca_pem: Option<String>,
    session_days: u32,
    preference: RelayPreference,
    app_handle: tauri::AppHandle,
) -> Result<(), MeshGuardError> {
    crate::history::ensure_open(&app_handle).await;
    let state = app_handle.state::<AppState>();
    if state.vault(&app_handle).await.is_none() {
        return Err(MeshGuardError::InvalidConfig(
            "This device has no secure key store, so internet relay is not available".into(),
        ));
    }
    let address =
        parse_broker_address(&broker).map_err(|e| MeshGuardError::InvalidConfig(e.to_string()))?;
    let username = username.trim().to_string();
    if !meshguard_core::relay::is_app_username(&username) {
        return Err(MeshGuardError::InvalidConfig(
            "Use a generated username (mg- and 20 letters/digits); tap Generate".into(),
        ));
    }
    let ca_pem = ca_pem
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    if let Some(pem) = &ca_pem {
        meshguard_mqtt::tls_config(Some(pem))
            .map_err(|e| MeshGuardError::InvalidConfig(e.to_string()))?;
    }
    if !(1..=7).contains(&session_days) {
        return Err(MeshGuardError::InvalidConfig(
            "Offline queue must be 1 to 7 days".into(),
        ));
    }
    {
        let mut e = state.engine();
        let prev = e.config.relay.clone();
        let identity_changed = prev.as_ref().is_some_and(|p| {
            p.host != address.host || p.port != address.port || p.username != username
        });
        if identity_changed && !e.relay.contacts.is_empty() {
            return Err(MeshGuardError::InvalidConfig(
                "Turn internet relay off for your contacts before changing the broker or username"
                    .into(),
            ));
        }
        let password = password.filter(|p| !p.is_empty());
        if password.is_none() && e.relay.password.is_none() {
            return Err(MeshGuardError::InvalidConfig("Password is required".into()));
        }
        if let Some(pw) = password {
            e.update_relay(|b| b.password = Some(pw))?;
        }
        let settings = RelaySettings {
            enabled: prev.as_ref().is_some_and(|p| p.enabled),
            host: address.host,
            port: address.port,
            username,
            ca_pem,
            client_id: prev
                .as_ref()
                .map(|p| p.client_id.clone())
                .unwrap_or_else(meshguard_mqtt::new_client_id),
            session_expiry_secs: session_days * 86_400,
            preference,
        };
        e.config.relay = Some(settings);
        if let Err(err) = e.config.save(&state.config_dir) {
            e.config.relay = prev;
            return Err(err);
        }
    }
    restart(&app_handle).await;
    Ok(())
}

/// Turn the relay on or off (off: no MQTT connection at all).
#[tauri::command]
pub async fn relay_set_enabled(
    enabled: bool,
    app_handle: tauri::AppHandle,
) -> Result<(), MeshGuardError> {
    let state = app_handle.state::<AppState>();
    {
        let mut e = state.engine();
        let Some(prev) = e.config.relay.clone() else {
            return Err(MeshGuardError::InvalidConfig(
                "Set up the broker first".into(),
            ));
        };
        e.config.relay.as_mut().expect("checked").enabled = enabled;
        if let Err(err) = e.config.save(&state.config_dir) {
            e.config.relay = Some(prev);
            return Err(err);
        }
    }
    restart(&app_handle).await;
    Ok(())
}

/// Connect with the saved settings and report how it went.
#[tauri::command]
pub async fn relay_test(app_handle: tauri::AppHandle) -> Result<StatusView, MeshGuardError> {
    let state = app_handle.state::<AppState>();
    let Some((settings, _, _)) = broker_settings(&state) else {
        return Err(MeshGuardError::InvalidConfig(
            "Set up the broker first".into(),
        ));
    };
    // A separate, short-lived session: the main one is not disturbed.
    let mut probe = settings.clone();
    probe.client_id = meshguard_mqtt::new_client_id();
    probe.session_expiry_secs = meshguard_mqtt::config::SESSION_EXPIRY_RANGE
        .start()
        .to_owned();
    let (client, _rx) = RelayClient::start(&probe, BTreeSet::new(), BTreeSet::new())
        .map_err(|e| MeshGuardError::InvalidConfig(e.to_string()))?;
    let mut watch = client.watch_status();
    let result = tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let s = watch.borrow_and_update().clone();
            if matches!(
                s,
                RelayStatus::Connected
                    | RelayStatus::AuthFailed
                    | RelayStatus::TlsError(_)
                    | RelayStatus::Disconnected(_)
            ) {
                return s;
            }
            if watch.changed().await.is_err() {
                return client.status();
            }
        }
    })
    .await
    .unwrap_or(RelayStatus::Disconnected(
        "no answer from the broker".into(),
    ));
    client.stop().await;
    Ok(status_view(&result))
}

/// Turn the relay off and forget its settings and secrets. Contacts are
/// told (revoke) over the radio when it is next connected.
#[tauri::command]
pub async fn relay_remove(app_handle: tauri::AppHandle) -> Result<(), MeshGuardError> {
    let state = app_handle.state::<AppState>();
    {
        let mut e = state.engine();
        e.relay_remove_all(now_secs())?;
        let prev = e.config.relay.take();
        if let Err(err) = e.config.save(&state.config_dir) {
            e.config.relay = prev;
            return Err(err);
        }
    }
    restart(&app_handle).await;
    Ok(())
}

#[derive(Serialize)]
pub struct RelayContactView {
    /// "none", "offered", "incoming", "active", "rotation".
    pub state: &'static str,
    /// Our relay is set up.
    pub configured: bool,
    /// The peer announced relay support.
    pub supported: bool,
    /// For an incoming offer: whether it is for our broker.
    pub same_broker: Option<bool>,
}

fn contact_view(state: &AppState, peer: u32) -> RelayContactView {
    let e = state.engine();
    let me = e.relay_identity();
    let (st, same) = match e.keys_owner.and_then(|m| e.relay.state(m, peer)) {
        None => ("none", None),
        Some(ContactRelay::Offered { .. }) => ("offered", None),
        Some(ContactRelay::Incoming { peer_tag, .. }) => (
            "incoming",
            Some(me.as_ref().is_some_and(|m| {
                peer_tag
                    == &m
                        .broker_tag
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
            })),
        ),
        Some(ContactRelay::Active {
            rotation: Some(_), ..
        }) => ("rotation", None),
        Some(ContactRelay::Active { .. }) => ("active", None),
    };
    RelayContactView {
        state: st,
        configured: me.is_some() && e.relay.password.is_some(),
        supported: e.peer_supports_relay(peer),
        same_broker: same,
    }
}

#[tauri::command]
pub async fn relay_contact(
    peer: u32,
    app_handle: tauri::AppHandle,
) -> Result<RelayContactView, MeshGuardError> {
    crate::history::ensure_open(&app_handle).await;
    Ok(contact_view(&app_handle.state::<AppState>(), peer))
}

/// Turn the relay on for a contact (offer), or accept its offer.
#[tauri::command]
pub async fn relay_contact_enable(
    peer: u32,
    app_handle: tauri::AppHandle,
) -> Result<RelayContactView, MeshGuardError> {
    crate::history::ensure_open(&app_handle).await;
    ensure_keys(&app_handle).await;
    let state = app_handle.state::<AppState>();
    let r = state.engine().relay_enable(peer, now_secs());
    if let Err(e) = r {
        if !state.engine().peer_supports_relay(peer) {
            // Ask the contact's app what it supports; the answer arrives as
            // its Caps.
            crate::status_tx::request_caps(&state, peer).await;
        }
        return Err(e);
    }
    sync_topics(&app_handle).await;
    Ok(contact_view(&state, peer))
}

/// Turn the relay off for a contact, or decline its offer.
#[tauri::command]
pub async fn relay_contact_disable(
    peer: u32,
    app_handle: tauri::AppHandle,
) -> Result<RelayContactView, MeshGuardError> {
    let state = app_handle.state::<AppState>();
    state.engine().relay_disable(peer, now_secs())?;
    sync_topics(&app_handle).await;
    Ok(contact_view(&state, peer))
}

/// The exact broker ACL lines for a contact (strict mode, ADR-006 §5).
#[tauri::command]
pub async fn relay_acl_lines(
    peer: u32,
    app_handle: tauri::AppHandle,
) -> Result<Vec<String>, MeshGuardError> {
    let state = app_handle.state::<AppState>();
    let e = state.engine();
    let my_node = e.keys_owner.ok_or(MeshGuardError::NotConnected)?;
    let me = e
        .relay_identity()
        .ok_or_else(|| MeshGuardError::InvalidConfig("Set up the broker first".into()))?;
    let (inbound, outbound) = e.relay.topics_of(my_node, peer, &me).ok_or_else(|| {
        MeshGuardError::Protocol("Internet relay is not on for this contact".into())
    })?;
    Ok(vec![
        format!("user {}", me.user),
        format!("topic write {outbound}"),
        format!("topic read {inbound}"),
    ])
}
