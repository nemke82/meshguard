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

use meshguard_core::delivery::{DeliveryState, DueAction, OutboundMessage, RetryPolicy};
use meshguard_core::envelope::DEFAULT_FRAME_BUDGET;
use meshguard_core::ids::{ConversationId, MessageId, TransportId};
use meshguard_core::outbound::{self, OutPayload, WireVersion};
use meshguard_core::status::StatusBody;
use meshguard_core::PeerIdentity;
use meshguard_store::{
    conversation_id, parse_conversation_id, Cursor, Direction, MessageKind, Store,
};
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

/// A uniform value in `[0, 1)` for retry jitter.
fn jitter() -> f64 {
    let b = MessageId::random();
    let mut v = [0u8; 8];
    v.copy_from_slice(&b.as_bytes()[..8]);
    (u64::from_be_bytes(v) >> 11) as f64 / (1u64 << 53) as f64
}

/// Open the store on first use (once the vault key is available).
async fn ensure_open(app: &tauri::AppHandle) {
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
    let state = app.state::<AppState>();
    let _claim = state.outbox_lock.lock().await;
    let c = conversation_id(my_node, peer);
    let updated = with(app, |s| {
        let Some(mut out) = s.outgoing(&c, &acked)? else {
            return Ok(None);
        };
        if out.record_peer_ack().is_err() {
            return Ok(None);
        }
        s.update_outbound(&c, &out)?;
        Ok(Some(out.state))
    })
    .await
    .flatten();
    if let Some(st) = updated {
        emit_state(app, &c, &acked, st.as_str());
    }
    updated.is_some()
}

// ── Outgoing ──────────────────────────────────────────────────

#[derive(Clone, Serialize)]
pub struct MessageStateEvent {
    pub conversation_id: String,
    pub message_id: String,
    pub state: String,
}

fn emit_state(app: &tauri::AppHandle, c: &ConversationId, id: &MessageId, state: &str) {
    let _ = app.emit(
        "message-state",
        MessageStateEvent {
            conversation_id: c.0.clone(),
            message_id: id.to_hex(),
            state: state.to_string(),
        },
    );
}

/// What to put on the wire for a stored message, given what the peer
/// understands now (checked again on every retry).
pub enum Content {
    Text(String),
    Status(StatusBody),
}

impl Content {
    fn kind(&self) -> MessageKind {
        match self {
            Self::Text(_) => MessageKind::Text,
            Self::Status(_) => MessageKind::Status,
        }
    }

    fn body(&self) -> Result<Vec<u8>, MeshGuardError> {
        match self {
            Self::Text(t) => Ok(t.as_bytes().to_vec()),
            Self::Status(st) => st
                .encode()
                .map_err(|e| MeshGuardError::Protocol(e.to_string())),
        }
    }

    fn from_stored(kind: MessageKind, body: &[u8]) -> Option<Self> {
        match kind {
            MessageKind::Text => String::from_utf8(body.to_vec()).ok().map(Self::Text),
            MessageKind::Status => StatusBody::decode(body).map(Self::Status),
        }
    }

    fn payload(&self, protocol: WireProtocol, status_capable: bool) -> OutPayload<'_> {
        match self {
            Self::Text(t) => OutPayload::Text(t),
            // v1 peers get the fallback inside `seal`; v2 peers on an older
            // build would drop a Status frame, so they get v2 text until
            // they are known to understand it.
            Self::Status(st) if protocol == WireProtocol::V2 && !status_capable => {
                OutPayload::StatusAsText(st)
            }
            Self::Status(st) => OutPayload::Status(st),
        }
    }
}

/// Result of [`send`]: the message is stored (or not) and in this state.
pub struct SendOutcome {
    pub message_id: MessageId,
    /// `DeliveryState::as_str()`.
    pub state: &'static str,
    pub stored: bool,
    pub protocol: WireProtocol,
}

fn seal_for(
    state: &AppState,
    my_node: u32,
    peer: u32,
    content: &Content,
    message_id: MessageId,
    created_at: u64,
) -> Result<(Vec<u8>, WireProtocol), MeshGuardError> {
    let engine = state.engine();
    let (protocol, capable) = engine.peer_wire(peer);
    if engine.keys_owner != Some(my_node) {
        return Err(MeshGuardError::NoSession);
    }
    let key = engine.keys.get(&peer).ok_or(MeshGuardError::NoSession)?;
    let sealed = outbound::seal_with_id(
        WireVersion::from(protocol),
        content.payload(protocol, capable),
        key,
        &PeerIdentity::meshtastic_node(my_node),
        &PeerIdentity::meshtastic_node(peer),
        u32::try_from(created_at).unwrap_or(u32::MAX),
        DEFAULT_FRAME_BUDGET,
        message_id,
    )
    .map_err(crate::commands::seal_err)?;
    Ok((sealed.frame, protocol))
}

fn transport_id(state: &AppState) -> TransportId {
    let kind = state
        .engine()
        .config
        .last_link
        .as_ref()
        .map(|l| l.kind())
        .unwrap_or("radio");
    TransportId::new(kind)
}

/// The local radio we can transmit through right now. `my_node_num` alone
/// is not enough: link loss removes the radio but keeps the node number.
async fn live_radio(state: &AppState) -> Option<u32> {
    let radio_up = state.radio.lock().await.is_some();
    let node = state.engine().my_node_num;
    node.filter(|_| radio_up)
}

/// Outcome of one transmission.
enum Attempt {
    Submitted,
    /// The radio refused or failed the write: counts as an attempt.
    Failed,
    /// No radio (gone meanwhile, or another one now): not an attempt; the
    /// message waits.
    Unavailable,
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

/// Bookkeeping after one transmission; caller holds `outbox_lock` and
/// passed the current stored state in `out`.
async fn after_attempt(
    app: &tauri::AppHandle,
    c: &ConversationId,
    out: &mut OutboundMessage,
    protocol: WireProtocol,
    attempt: Attempt,
    now: u64,
) {
    let state = app.state::<AppState>();
    let policy = RetryPolicy::default();
    let r = match attempt {
        Attempt::Submitted => out.record_submitted(now, transport_id(&state), &policy),
        Attempt::Failed => out.record_submit_failed(now, &policy, jitter()),
        Attempt::Unavailable => return,
    };
    if r.is_err() {
        return;
    }
    let unconfirmable = protocol == WireProtocol::V1 && out.state == DeliveryState::Submitted;
    let snapshot = out.clone();
    let c2 = c.clone();
    with(app, move |s| {
        if unconfirmable {
            // v1 peers never ACK; resending could only duplicate.
            s.settle_unconfirmed(&c2, &snapshot)
        } else {
            s.update_outbound(&c2, &snapshot)
        }
    })
    .await;
    emit_state(app, c, &out.message_id, out.state.as_str());
}

/// Send a message to `peer`: stored first (when history is on), then
/// transmitted if the chat's radio is there. With history on, a missing
/// radio is not an error: the message waits in the outbox.
pub async fn send(
    app: &tauri::AppHandle,
    peer: u32,
    content: Content,
) -> Result<SendOutcome, MeshGuardError> {
    let state = app.state::<AppState>();
    // Keys belong to one local radio; while it is away, queue for it.
    let (my_node, (protocol, capable)) = {
        let engine = state.engine();
        let my_node = engine
            .keys_owner
            .or(engine.my_node_num)
            .ok_or(MeshGuardError::NotConnected)?;
        (my_node, engine.peer_wire(peer))
    };
    let now = now_secs();
    let message_id = MessageId::random();
    let ttl = u64::from(outbound::ttl_mins(content.payload(protocol, capable))) * 60;
    let mut out = OutboundMessage::queued(message_id, now, now + ttl);

    // Seal first: a message that cannot be sent (no key, too long) is not
    // stored.
    let (frame, protocol) = seal_for(&state, my_node, peer, &content, message_id, now)?;

    // Held from storing until the first attempt is recorded, so the outbox
    // loop cannot transmit the same new row as well.
    let _claim = state.outbox_lock.lock().await;
    let title = peer_title(&state, peer);
    let (kind, body) = (content.kind(), content.body()?);
    let snapshot = out.clone();
    let c = with(app, move |s| {
        let c = s.upsert_conversation(my_node, peer, title.as_deref())?;
        s.insert_outgoing(&c, kind, &body, now, &snapshot)?;
        Ok(c)
    })
    .await;

    let attempt = if live_radio(&state).await == Some(my_node) {
        classify(&crate::commands::send_as(&state, my_node, peer, frame).await)
    } else {
        Attempt::Unavailable
    };
    match c {
        Some(c) => {
            after_attempt(app, &c, &mut out, protocol, attempt, now).await;
            Ok(SendOutcome {
                message_id,
                state: out.state.as_str(),
                stored: true,
                protocol,
            })
        }
        // Memory-only history: as before, only an accepted write succeeds.
        None => match attempt {
            Attempt::Submitted => Ok(SendOutcome {
                message_id,
                state: DeliveryState::Submitted.as_str(),
                stored: false,
                protocol,
            }),
            Attempt::Unavailable => Err(MeshGuardError::NotConnected),
            Attempt::Failed => Err(MeshGuardError::Protocol(
                "The radio did not accept the message".into(),
            )),
        },
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
    let Some(due) = with(app, |s| s.due_outbound(now)).await else {
        return;
    };
    // At most one transmission per tick: shared LoRa airtime.
    let mut sent_one = false;
    for item in due {
        let c = item.conversation;
        let Some((my_node, peer)) = parse_conversation_id(&c) else {
            continue;
        };
        // Claim the row and re-read it: an ACK, a send or a deletion may
        // have changed it since the list was taken.
        let _claim = state.outbox_lock.lock().await;
        let id = item.outbound.message_id;
        let Some(mut outbound) = with(app, |s| s.outbound(&c, &id)).await.flatten() else {
            continue;
        };
        match outbound.due_action(now) {
            Some(DueAction::Expire) => {
                if outbound.expire().is_ok() {
                    persist(app, &c, &outbound).await;
                }
            }
            Some(DueAction::AckTimedOut) => {
                if outbound
                    .record_ack_timeout(now, &RetryPolicy::default(), jitter())
                    .is_ok()
                {
                    persist(app, &c, &outbound).await;
                }
            }
            Some(DueAction::Send) => {
                // Only through the radio the chat belongs to; otherwise wait
                // (the TTL still applies).
                if sent_one || live_radio(&state).await != Some(my_node) {
                    continue;
                }
                let Some(content) = Content::from_stored(item.kind, &item.body) else {
                    continue;
                };
                let Ok((frame, protocol)) = seal_for(
                    &state,
                    my_node,
                    peer,
                    &content,
                    outbound.message_id,
                    item.created_at,
                ) else {
                    // No key for this chat right now (forgotten peer): wait.
                    continue;
                };
                sent_one = true;
                let attempt =
                    classify(&crate::commands::send_as(&state, my_node, peer, frame).await);
                tracing::info!(
                    "Outbox resend to {peer} (attempt {}): {}",
                    outbound.attempts + 1,
                    match attempt {
                        Attempt::Submitted => "submitted",
                        Attempt::Failed => "failed",
                        Attempt::Unavailable => "radio unavailable",
                    }
                );
                after_attempt(app, &c, &mut outbound, protocol, attempt, now).await;
            }
            None => {}
        }
    }
}

async fn persist(app: &tauri::AppHandle, c: &ConversationId, out: &OutboundMessage) {
    let (c2, snapshot) = (c.clone(), out.clone());
    with(app, move |s| s.update_outbound(&c2, &snapshot)).await;
    emit_state(app, c, &out.message_id, out.state.as_str());
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
    // Not in the middle of a transmission for it, and none after.
    let state = app_handle.state::<AppState>();
    let _claim = state.outbox_lock.lock().await;
    Ok(with(&app_handle, |s| s.delete_conversation(&c))
        .await
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_round_trips_through_storage() {
        let st = StatusBody {
            kind: meshguard_core::status::StatusKind::Sos,
            location: None,
            text: "help".into(),
        };
        let c = Content::Status(st.clone());
        let back = Content::from_stored(c.kind(), &c.body().unwrap()).unwrap();
        assert!(matches!(back, Content::Status(b) if b == st));
        let t = Content::Text("hi".into());
        assert!(matches!(
            Content::from_stored(t.kind(), &t.body().unwrap()),
            Some(Content::Text(s)) if s == "hi"
        ));
    }

    #[test]
    fn jitter_is_in_range() {
        for _ in 0..1000 {
            let j = jitter();
            assert!((0.0..1.0).contains(&j));
        }
    }
}
