//! Conversation history and the outbox (ADR-005), on top of `meshguard-store`.
//!
//! The store opens lazily once the vault key is available (Android Keystore
//! or the desktop OS keyring). Without a vault nothing is written to disk and
//! messages live in the UI for the session only, as before.
//!
//! Outgoing messages are written to the store before any radio write. A
//! background loop then resends what is due, settles ACK timeouts and
//! expires messages past their TTL, following `meshguard_core::delivery`.

use std::time::Duration;

use meshguard_core::delivery::DeliveryState;
use meshguard_core::ids::{ConversationId, MessageId};
use meshguard_core::status::StatusBody;
use meshguard_store::{Cursor, Direction, MessageKind, Store};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

use crate::device_config::WireProtocol;
use crate::error::MeshGuardError;
use crate::state::AppState;

/// How often the outbox is checked.
const OUTBOX_TICK: Duration = Duration::from_secs(5);

pub use meshguard_engine::engine::Slot;

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

/// Open the store on first use (once the vault key is available).
pub async fn ensure_open(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if !matches!(state.engine().history, Slot::Closed) {
        return;
    }
    let vault = state.vault(app).await;
    state.engine().open_history(vault.as_deref());
}

/// Run `f` on the store. `None` when history is memory-only or `f` failed
/// (logged without content).
pub async fn with<T>(
    app: &tauri::AppHandle,
    f: impl FnOnce(&mut Store) -> meshguard_store::Result<T>,
) -> Option<T> {
    ensure_open(app).await;
    app.state::<AppState>().engine().with_store(f)
}

/// Display name of a peer: the name it paired with, else the mesh name.
pub fn peer_title(state: &AppState, peer: u32) -> Option<String> {
    state.engine().peer_title(peer)
}

// ── Incoming ──────────────────────────────────────────────────

/// Record an authenticated ACK, also for a message already marked failed
/// or expired. Returns true when it matched a stored outgoing message.
pub async fn record_peer_ack(
    app: &tauri::AppHandle,
    my_node: u32,
    peer: u32,
    acked: MessageId,
) -> bool {
    ensure_open(app).await;
    let event = app
        .state::<AppState>()
        .engine()
        .record_peer_ack(my_node, peer, acked);
    let matched = event.is_some();
    emit_state(app, event);
    matched
}

// ── Outgoing ──────────────────────────────────────────────────

use meshguard_engine::outbox::{Attempt, Routes, Transmit};
use meshguard_engine::receive::Route;
pub use meshguard_engine::outbox::{Content, MessageStateEvent};

pub fn emit_state(app: &tauri::AppHandle, event: Option<MessageStateEvent>) {
    if let Some(e) = event {
        let _ = app.emit("message-state", e);
    }
}

/// Result of [`send`]: the message is stored (or not) and in this state.
pub struct SendOutcome {
    /// "relay" when the attempt went to the MQTT broker.
    pub via: Option<&'static str>,
    pub message_id: MessageId,
    /// `DeliveryState::as_str()`.
    pub state: &'static str,
    pub stored: bool,
    pub protocol: WireProtocol,
}

/// The local radio we can transmit through right now. `my_node_num` alone
/// is not enough: link loss removes the radio but keeps the node number.
async fn live_radio(state: &AppState) -> Option<u32> {
    let radio_up = state.radio.lock().await.is_some();
    let node = state.engine().my_node_num;
    node.filter(|_| radio_up)
}

fn classify(r: &Result<(), MeshGuardError>) -> Attempt {
    match r {
        Ok(()) => Attempt::Submitted,
        Err(MeshGuardError::NotConnected) => Attempt::Unavailable,
        // send_as: the radio changed while sending.
        Err(MeshGuardError::Protocol(m)) if m.contains("radio changed") => Attempt::Unavailable,
        Err(_) => Attempt::Failed,
    }
}

/// What can carry messages right now.
async fn routes(state: &AppState) -> Routes {
    Routes {
        radio: live_radio(state).await,
        relay: state.relay.connected().await,
    }
}

/// Transmit `t` on its route (radio or relay) and record the result.
async fn transmit(
    app: &tauri::AppHandle,
    t: &Transmit,
    now: u64,
) -> (Attempt, Option<DeliveryState>) {
    let state = app.state::<AppState>();
    let attempt = match t.route {
        Route::Relay => crate::relay::publish(app, t, now).await,
        Route::Radio if live_radio(&state).await == Some(t.my_node) => {
            classify(&crate::commands::send_transmit(&state, t).await)
        }
        Route::Radio => Attempt::Unavailable,
    };
    let (st, event) = state.engine().finish_attempt(t, attempt, now);
    emit_state(app, event);
    (attempt, st)
}

/// Send a message to `peer`: stored first (when history is on), then
/// transmitted if the chat's radio is there. With history on, a missing
/// radio is not an error: the message waits in the outbox.
pub async fn send(
    app: &tauri::AppHandle,
    peer: u32,
    content: Content,
) -> Result<SendOutcome, MeshGuardError> {
    ensure_open(app).await;
    let now = now_secs();
    let state = app.state::<AppState>();
    let routes = routes(&state).await;
    let t = state.engine().enqueue(peer, &content, now, routes)?;
    let (attempt, st) = transmit(app, &t, now).await;
    if t.conversation.is_some() {
        return Ok(SendOutcome {
            via: (t.route == Route::Relay && attempt == Attempt::Submitted).then_some("relay"),
            message_id: t.message_id,
            // Deleted meanwhile: report what was attempted.
            state: st.unwrap_or(DeliveryState::Queued).as_str(),
            stored: true,
            protocol: t.protocol,
        });
    }
    // Memory-only history: as before, only an accepted write succeeds.
    match attempt {
        Attempt::Submitted => Ok(SendOutcome {
            via: (t.route == Route::Relay).then_some("relay"),
            message_id: t.message_id,
            state: DeliveryState::Submitted.as_str(),
            stored: false,
            protocol: t.protocol,
        }),
        Attempt::Unavailable => Err(MeshGuardError::NotConnected),
        Attempt::Failed => Err(MeshGuardError::Protocol(
            "The radio did not accept the message".into(),
        )),
    }
}

// ── Outbox loop ───────────────────────────────────────────────

/// Start the background loop that works through the outbox.
pub fn spawn_outbox_loop(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(OUTBOX_TICK).await;
            outbox_tick(&app).await;
        }
    });
}

async fn outbox_tick(app: &tauri::AppHandle) {
    // Opens the store on the first tick, so messages queued before a
    // restart are worked on without any other activity.
    ensure_open(app).await;
    let state = app.state::<AppState>();
    let now = now_secs();
    crate::relay::sync_topics(app).await;
    crate::relay::housekeeping(app);
    let routes = routes(&state).await;
    let (events, resend) = state.engine().outbox_step(now, routes);
    for e in events {
        emit_state(app, Some(e));
    }
    send_relay_controls(app, now).await;
    if let Some(t) = resend {
        let (attempt, _) = transmit(app, &t, now).await;
        tracing::info!(
            "Outbox resend to {} (attempt {}): {}",
            t.to,
            t.attempt + 1,
            match attempt {
                Attempt::Submitted => "submitted",
                Attempt::Failed => "failed",
                Attempt::Unavailable => "radio unavailable",
            }
        );
    }
}

/// Send the relay control messages that are due (ADR-006 §4): over the
/// radio they were sealed for when it is there (at most one per tick,
/// shared airtime), otherwise over the relay to contacts it is already
/// active with (accept retries, revokes).
async fn send_relay_controls(app: &tauri::AppHandle, now: u64) {
    let state = app.state::<AppState>();
    let live = live_radio(&state).await;
    let due = state.engine().relay_due_controls(now);
    if let Some(c) = due.iter().find(|c| Some(c.my_node) == live) {
        if let Err(e) = crate::commands::send_control(&state, c, now).await {
            tracing::info!("Relay control to {} not sent: {e}", c.peer);
        }
        return;
    }
    for c in &due {
        crate::relay::publish_control(app, c, now).await;
    }
}

// ── Commands ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct HistoryInfo {
    /// History is saved on this device.
    pub saved: bool,
    /// Why not, when it is not.
    pub reason: Option<String>,
}

#[tauri::command]
pub async fn get_history_info(app_handle: tauri::AppHandle) -> Result<HistoryInfo, MeshGuardError> {
    ensure_open(&app_handle).await;
    let state = app_handle.state::<AppState>();
    let engine = state.engine();
    Ok(match &engine.history {
        Slot::Open(_) => HistoryInfo {
            saved: true,
            reason: None,
        },
        Slot::Off(reason) => HistoryInfo {
            saved: false,
            reason: Some(reason.clone()),
        },
        Slot::Closed => HistoryInfo {
            saved: false,
            reason: None,
        },
    })
}

#[derive(Serialize)]
pub struct ConversationView {
    pub conversation_id: String,
    pub my_node: u32,
    pub peer_node: u32,
    pub title: String,
    pub last_message_at: u64,
    pub unread: u32,
    /// Short text of the newest message.
    pub preview: Option<String>,
    pub preview_outgoing: bool,
}

fn preview_of(kind: MessageKind, body: &[u8]) -> Option<String> {
    match Content::from_stored(kind, body)? {
        Content::Text(t) => Some(t),
        Content::Status(st) => Some(st.fallback_text()),
    }
}

/// The chat list, newest activity first.
#[tauri::command]
pub async fn list_conversations(
    app_handle: tauri::AppHandle,
) -> Result<Vec<ConversationView>, MeshGuardError> {
    let rows = with(&app_handle, |s| {
        let mut out = Vec::new();
        for c in s.conversations()? {
            let last = s.messages(&c.id, None, 1)?.pop();
            out.push((c, last));
        }
        Ok(out)
    })
    .await
    .unwrap_or_default();
    let state = app_handle.state::<AppState>();
    let mut views = Vec::with_capacity(rows.len());
    for (c, last) in rows {
        let title = match c.title {
            Some(t) if !t.is_empty() => t,
            _ => peer_title(&state, c.peer_node).unwrap_or_else(|| format!("!{:08x}", c.peer_node)),
        };
        views.push(ConversationView {
            conversation_id: c.id.0,
            my_node: c.my_node,
            peer_node: c.peer_node,
            title,
            last_message_at: c.last_message_at,
            unread: c.unread,
            preview: last.as_ref().and_then(|m| preview_of(m.kind, &m.body)),
            preview_outgoing: last.is_some_and(|m| m.direction == Direction::Out),
        });
    }
    Ok(views)
}

/// Paging position, as the UI passes it back.
#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct CursorView {
    pub stored_at: u64,
    pub seq: i64,
}

#[derive(Serialize)]
pub struct MessageView {
    pub message_id: String,
    pub outgoing: bool,
    pub text: Option<String>,
    pub status: Option<StatusBody>,
    pub created_at: u64,
    pub stored_at: u64,
    /// Outgoing: delivery state; incoming: "received" / "read".
    pub state: String,
    /// "relay" when the last attempt of a queued message went to the MQTT
    /// broker (`submitted` then means *sent to broker*).
    pub via: Option<&'static str>,
    pub cursor: CursorView,
}

/// Messages of a conversation before `before` (newest when absent),
/// oldest first.
#[tauri::command]
pub async fn get_messages(
    conversation_id: String,
    before: Option<CursorView>,
    limit: Option<u32>,
    app_handle: tauri::AppHandle,
) -> Result<Vec<MessageView>, MeshGuardError> {
    let c = ConversationId(conversation_id);
    let before = before.map(|b| Cursor {
        stored_at: b.stored_at,
        seq: b.seq,
    });
    let limit = limit.unwrap_or(50).clamp(1, 200);
    let msgs = with(&app_handle, |s| s.messages(&c, before, limit))
        .await
        .unwrap_or_default();
    Ok(msgs
        .into_iter()
        .map(|m| {
            let content = Content::from_stored(m.kind, &m.body);
            let (text, status) = match content {
                Some(Content::Text(t)) => (Some(t), None),
                Some(Content::Status(st)) => (None, Some(st)),
                None => (None, None),
            };
            MessageView {
                message_id: m.message_id.to_hex(),
                outgoing: m.direction == Direction::Out,
                text,
                status,
                created_at: m.created_at,
                stored_at: m.stored_at,
                state: m.state,
                via: (m.last_transport.as_deref() == Some("mqtt")).then_some("relay"),
                cursor: CursorView {
                    stored_at: m.cursor.stored_at,
                    seq: m.cursor.seq,
                },
            }
        })
        .collect())
}

#[tauri::command]
pub async fn mark_conversation_read(
    conversation_id: String,
    app_handle: tauri::AppHandle,
) -> Result<(), MeshGuardError> {
    let c = ConversationId(conversation_id);
    with(&app_handle, |s| s.mark_read(&c)).await;
    Ok(())
}

/// Delete a conversation's history and queued messages. The chat key and
/// the peer stay; this only removes the messages.
#[tauri::command]
pub async fn delete_conversation(
    conversation_id: String,
    app_handle: tauri::AppHandle,
) -> Result<bool, MeshGuardError> {
    let c = ConversationId(conversation_id);
    // A transmission in progress for it finds the row gone and changes
    // nothing (see meshguard_engine::outbox).
    Ok(with(&app_handle, |s| s.delete_conversation(&c))
        .await
        .unwrap_or(false))
}
