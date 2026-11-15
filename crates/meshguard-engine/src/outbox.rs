//! Sending and the outbox (ADR-005), without I/O.
//!
//! A message is sealed and stored first ([`Engine::enqueue`]); the caller
//! transmits the returned [`Transmit`] and reports how it went
//! ([`Engine::finish_attempt`]). [`Engine::outbox_step`] resends what is
//! due, settles ACK timeouts and expires old messages.
//!
//! The radio write happens outside the engine lock, so a message being
//! transmitted is marked *in flight*: the outbox does not pick it up again
//! meanwhile, and the result is applied to the row as stored at that
//! moment (an ACK or a deletion in between wins over a late result).

use std::collections::HashMap;

use meshguard_core::delivery::{DeliveryState, DueAction, OutboundMessage, RetryPolicy};
use meshguard_core::envelope::{SealError, DEFAULT_FRAME_BUDGET};
use meshguard_core::ids::{ConversationId, MessageId, TransportId};
use meshguard_core::outbound::{self, OutPayload, WireVersion};
use meshguard_core::status::StatusBody;
use meshguard_core::PeerIdentity;
use meshguard_store::{conversation_id, parse_conversation_id, MessageKind};
use meshtastic::protobufs;
use serde::Serialize;

use crate::config::WireProtocol;
use crate::engine::{Engine, Slot};
use crate::error::MeshGuardError;

/// An in-flight mark older than this is stale (the caller never reported
/// the attempt); the message becomes eligible again.
const IN_FLIGHT_SECS: u64 = 120;

/// What a message says, stored as `(kind, body)`. What goes on the wire is
/// decided per attempt by what the peer understands at that moment.
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    Status(StatusBody),
}

impl Content {
    pub fn kind(&self) -> MessageKind {
        match self {
            Self::Text(_) => MessageKind::Text,
            Self::Status(_) => MessageKind::Status,
        }
    }

    pub fn body(&self) -> Result<Vec<u8>, MeshGuardError> {
        match self {
            Self::Text(t) => Ok(t.as_bytes().to_vec()),
            Self::Status(st) => st
                .encode()
                .map_err(|e| MeshGuardError::Protocol(e.to_string())),
        }
    }

    pub fn from_stored(kind: MessageKind, body: &[u8]) -> Option<Self> {
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

/// A frame to transmit, and what to report back.
#[derive(Debug, Clone)]
pub struct Transmit {
    /// `None` when history is memory-only (nothing to update afterwards).
    pub conversation: Option<ConversationId>,
    pub message_id: MessageId,
    /// The local radio it was sealed for (its identity is in the AAD).
    pub my_node: u32,
    pub to: u32,
    pub frame: Vec<u8>,
    pub protocol: WireProtocol,
    /// Attempts made before this one.
    pub attempt: u32,
}

/// How a transmission went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    Submitted,
    /// The radio refused or failed the write: counts as an attempt.
    Failed,
    /// No radio (gone meanwhile, or another one now): not an attempt; the
    /// message waits.
    Unavailable,
}

/// A stored outgoing message changed state (`message-state` event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessageStateEvent {
    pub conversation_id: String,
    pub message_id: String,
    pub state: String,
}

impl MessageStateEvent {
    fn new(c: &ConversationId, id: &MessageId, state: DeliveryState) -> Self {
        Self {
            conversation_id: c.0.clone(),
            message_id: id.to_hex(),
            state: state.as_str().to_string(),
        }
    }
}

/// A uniform value in `[0, 1)` for retry jitter.
pub fn jitter() -> f64 {
    let b = MessageId::random();
    let mut v = [0u8; 8];
    v.copy_from_slice(&b.as_bytes()[..8]);
    (u64::from_be_bytes(v) >> 11) as f64 / (1u64 << 53) as f64
}

pub fn seal_err(e: SealError) -> MeshGuardError {
    match e {
        SealError::TooLarge { len, max } => MeshGuardError::Protocol(format!(
            "Message too long: {len} bytes on the wire, limit is {max}"
        )),
        SealError::InvalidBody => MeshGuardError::Protocol("invalid message body".into()),
        SealError::Crypto => MeshGuardError::Encryption("sealing failed".into()),
    }
}

/// Messages being transmitted right now: id -> since when.
#[derive(Default)]
pub struct InFlight(HashMap<MessageId, u64>);

impl InFlight {
    fn mark(&mut self, id: MessageId, now: u64) {
        self.0.insert(id, now);
    }

    fn clear(&mut self, id: &MessageId) {
        self.0.remove(id);
    }

    fn contains(&mut self, id: &MessageId, now: u64) -> bool {
        self.0
            .retain(|_, since| now.saturating_sub(*since) < IN_FLIGHT_SECS);
        self.0.contains_key(id)
    }
}

impl Engine {
    /// The radio messages are sent from: the one the chat keys belong to
    /// (also while it is away, so messages queue for it).
    pub fn sending_node(&self) -> Option<u32> {
        self.keys_owner.or(self.my_node_num)
    }

    fn seal_for(
        &self,
        my_node: u32,
        peer: u32,
        content: &Content,
        message_id: MessageId,
        created_at: u64,
    ) -> Result<(Vec<u8>, WireProtocol), MeshGuardError> {
        let (protocol, capable) = self.peer_wire(peer);
        if self.keys_owner != Some(my_node) {
            return Err(MeshGuardError::NoSession);
        }
        let key = self.keys.get(&peer).ok_or(MeshGuardError::NoSession)?;
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
        .map_err(seal_err)?;
        Ok((sealed.frame, protocol))
    }

    fn transport_id(&self) -> TransportId {
        TransportId::new(
            self.config
                .last_link
                .as_ref()
                .map(|l| l.kind())
                .unwrap_or("radio"),
        )
    }

    /// Seal a new message for `peer` and store it (when history is on).
    /// The caller transmits the result if the radio is there and reports
    /// back with [`Self::finish_attempt`]. A message that cannot be sent
    /// (no key, too long) is not stored.
    pub fn enqueue(
        &mut self,
        peer: u32,
        content: &Content,
        now: u64,
    ) -> Result<Transmit, MeshGuardError> {
        let my_node = self.sending_node().ok_or(MeshGuardError::NotConnected)?;
        let (protocol, capable) = self.peer_wire(peer);
        let message_id = MessageId::random();
        let ttl = u64::from(outbound::ttl_mins(content.payload(protocol, capable))) * 60;
        let out = OutboundMessage::queued(message_id, now, now + ttl);
        let (frame, protocol) = self.seal_for(my_node, peer, content, message_id, now)?;

        let title = self.peer_title(peer);
        let (kind, body) = (content.kind(), content.body()?);
        let conversation = self.with_store(|s| {
            let c = s.upsert_conversation(my_node, peer, title.as_deref())?;
            s.insert_outgoing(&c, kind, &body, now, &out)?;
            Ok(c)
        });
        if conversation.is_some() {
            // The outbox must not send it too while the caller does.
            self.in_flight.mark(message_id, now);
        }
        Ok(Transmit {
            conversation,
            message_id,
            my_node,
            to: peer,
            frame,
            protocol,
            attempt: 0,
        })
    }

    /// Record how a transmission went. Returns the message's state now
    /// (`None`: no longer in the outbox, e.g. deleted) and the event to
    /// send to the UI, if it changed.
    pub fn finish_attempt(
        &mut self,
        t: &Transmit,
        attempt: Attempt,
        now: u64,
    ) -> (Option<DeliveryState>, Option<MessageStateEvent>) {
        self.in_flight.clear(&t.message_id);
        let Some(c) = &t.conversation else {
            // Memory-only: nothing stored, only the result counts.
            let st = match attempt {
                Attempt::Submitted => Some(DeliveryState::Submitted),
                Attempt::Failed | Attempt::Unavailable => None,
            };
            return (st, None);
        };
        let transport = self.transport_id();
        let policy = RetryPolicy::default();
        let unconfirmable = t.protocol == WireProtocol::V1;
        let r = self.with_store(|s| {
            // The row as it is now: an ACK or a deletion may have happened
            // while the frame was on its way to the radio.
            let Some(mut out) = s.outbound(c, &t.message_id)? else {
                return Ok((None, false));
            };
            let changed = match attempt {
                Attempt::Submitted => out.record_submitted(now, transport, &policy).is_ok(),
                Attempt::Failed => out.record_submit_failed(now, &policy, jitter()).is_ok(),
                Attempt::Unavailable => false,
            };
            if changed {
                if unconfirmable && out.state == DeliveryState::Submitted {
                    // v1 peers never ACK; resending could only duplicate.
                    s.settle_unconfirmed(c, &out)?;
                } else {
                    s.update_outbound(c, &out)?;
                }
            }
            Ok((Some(out.state), changed))
        });
        match r {
            Some((Some(st), true)) => {
                (Some(st), Some(MessageStateEvent::new(c, &t.message_id, st)))
            }
            Some((st, _)) => (st, None),
            None => (None, None),
        }
    }

    /// One pass over the outbox: expire old messages, settle ACK timeouts,
    /// and pick at most one message to resend (shared LoRa airtime) when
    /// its radio `live_radio` is connected. Returns the state changes and
    /// the frame to transmit, which is marked in flight until
    /// [`Self::finish_attempt`].
    pub fn outbox_step(
        &mut self,
        now: u64,
        live_radio: Option<u32>,
    ) -> (Vec<MessageStateEvent>, Option<Transmit>) {
        let mut events = Vec::new();
        let Some(due) = self.with_store(|s| s.due_outbound(now)) else {
            return (events, None);
        };
        let mut resend = None;
        for item in due {
            let c = item.conversation;
            let Some((my_node, peer)) = parse_conversation_id(&c) else {
                continue;
            };
            let mut out = item.outbound;
            if self.in_flight.contains(&out.message_id, now) {
                continue;
            }
            match out.due_action(now) {
                Some(DueAction::Expire) => {
                    if out.expire().is_ok() && self.persist(&c, &out) {
                        events.push(MessageStateEvent::new(&c, &out.message_id, out.state));
                    }
                }
                Some(DueAction::AckTimedOut) => {
                    if out
                        .record_ack_timeout(now, &RetryPolicy::default(), jitter())
                        .is_ok()
                        && self.persist(&c, &out)
                    {
                        events.push(MessageStateEvent::new(&c, &out.message_id, out.state));
                    }
                }
                Some(DueAction::Send) => {
                    // Only through the radio the chat belongs to; otherwise
                    // wait (the TTL still applies).
                    if resend.is_some() || live_radio != Some(my_node) {
                        continue;
                    }
                    let Some(content) = Content::from_stored(item.kind, &item.body) else {
                        continue;
                    };
                    let Ok((frame, protocol)) =
                        self.seal_for(my_node, peer, &content, out.message_id, item.created_at)
                    else {
                        // No key for this chat right now (forgotten peer): wait.
                        continue;
                    };
                    self.in_flight.mark(out.message_id, now);
                    resend = Some(Transmit {
                        conversation: Some(c),
                        message_id: out.message_id,
                        my_node,
                        to: peer,
                        frame,
                        protocol,
                        attempt: out.attempts,
                    });
                }
                None => {}
            }
        }
        (events, resend)
    }

    fn persist(&mut self, c: &ConversationId, out: &OutboundMessage) -> bool {
        self.with_store(|s| s.update_outbound(c, out)).is_some()
    }

    /// Record an authenticated ACK, also for a message already marked
    /// failed or expired. Returns the event when it matched a stored
    /// outgoing message.
    pub fn record_peer_ack(
        &mut self,
        my_node: u32,
        peer: u32,
        acked: MessageId,
    ) -> Option<MessageStateEvent> {
        let c = conversation_id(my_node, peer);
        let st = self
            .with_store(|s| {
                let Some(mut out) = s.outgoing(&c, &acked)? else {
                    return Ok(None);
                };
                if out.record_peer_ack().is_err() {
                    return Ok(None);
                }
                s.update_outbound(&c, &out)?;
                Ok(Some(out.state))
            })
            .flatten()?;
        Some(MessageStateEvent::new(&c, &acked, st))
    }

    /// History is saved on this device.
    pub fn history_saved(&self) -> bool {
        matches!(self.history, Slot::Open(_))
    }
}

/// A `ToRadio` packet carrying `payload` on `port` from our radio `from`
/// to node `to` on the primary channel, as the Meshtastic client library
/// builds it. For transports that write raw `ToRadio` bytes (the Android
/// service); `packet_id` must be non-zero and random.
pub fn to_radio_packet(
    from: u32,
    to: u32,
    port: protobufs::PortNum,
    payload: Vec<u8>,
    want_ack: bool,
    packet_id: u32,
    now: u32,
) -> Vec<u8> {
    use meshtastic::Message as _;
    let packet = protobufs::MeshPacket {
        payload_variant: Some(protobufs::mesh_packet::PayloadVariant::Decoded(
            protobufs::Data {
                portnum: port as i32,
                payload,
                ..Default::default()
            },
        )),
        from,
        to,
        id: packet_id,
        want_ack,
        channel: 0,
        rx_time: now,
        ..Default::default()
    };
    protobufs::ToRadio {
        payload_variant: Some(protobufs::to_radio::PayloadVariant::Packet(packet)),
    }
    .encode_to_vec()
}

/// A `ToRadio` asking the radio for its configuration and NodeDB (the
/// first thing a client sends after connecting).
pub fn to_radio_want_config(config_id: u32) -> Vec<u8> {
    use meshtastic::Message as _;
    protobufs::ToRadio {
        payload_variant: Some(protobufs::to_radio::PayloadVariant::WantConfigId(config_id)),
    }
    .encode_to_vec()
}

/// A random non-zero packet id.
pub fn random_packet_id() -> u32 {
    loop {
        let b = MessageId::random();
        let id = u32::from_be_bytes([
            b.as_bytes()[0],
            b.as_bytes()[1],
            b.as_bytes()[2],
            b.as_bytes()[3],
        ]);
        if id != 0 {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PeerConfig;
    use meshguard_core::crypto::SessionKey;

    const ME: u32 = 0x11;
    const PEER: u32 = 0x22;
    const NOW: u64 = 1_763_200_000;

    fn engine(name: &str, protocol: WireProtocol, history: bool) -> Engine {
        let dir = std::env::temp_dir().join(format!("mg-outbox-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = Engine::new(dir, NOW);
        e.config.peers.push(PeerConfig {
            id: "p".into(),
            device_name: "Alice".into(),
            node_num: PEER,
            protocol,
            features: 0,
            confirmed: true,
        });
        e.adopt_keys(ME, vec![(PEER, SessionKey::from_bytes([7; 32]))]);
        e.open_history(history.then(|| SessionKey::from_bytes([1; 32])).as_ref());
        e
    }

    fn text() -> Content {
        Content::Text("hello".into())
    }

    #[test]
    fn a_message_in_flight_is_not_sent_twice() {
        let mut e = engine("flight", WireProtocol::V2, true);
        let t = e.enqueue(PEER, &text(), NOW).unwrap();
        assert!(t.conversation.is_some());
        // Due right away, but the caller is transmitting it.
        assert!(e.outbox_step(NOW, Some(ME)).1.is_none());
        let (st, ev) = e.finish_attempt(&t, Attempt::Submitted, NOW);
        assert_eq!(st, Some(DeliveryState::Submitted));
        assert_eq!(ev.unwrap().state, "submitted");
    }

    #[test]
    fn an_ack_during_the_write_is_not_undone() {
        let mut e = engine("ack-race", WireProtocol::V2, true);
        let t = e.enqueue(PEER, &text(), NOW).unwrap();
        let ack = e.record_peer_ack(ME, PEER, t.message_id).unwrap();
        assert_eq!(ack.state, "peer_ack");
        let (st, ev) = e.finish_attempt(&t, Attempt::Submitted, NOW);
        assert!(ev.is_none());
        assert_ne!(st, Some(DeliveryState::Submitted));
    }

    #[test]
    fn an_unacked_message_is_resent_through_its_radio_only() {
        let mut e = engine("retry", WireProtocol::V2, true);
        let t = e.enqueue(PEER, &text(), NOW).unwrap();
        e.finish_attempt(&t, Attempt::Submitted, NOW);

        let (ev, tx) = e.outbox_step(NOW + 61, Some(ME));
        assert_eq!(ev[0].state, "retrying");
        assert!(tx.is_none());
        let later = NOW + 61 + 60;
        // Another radio connected: wait.
        assert!(e.outbox_step(later, Some(0x99)).1.is_none());
        let tx = e.outbox_step(later, Some(ME)).1.expect("resend");
        assert_eq!((tx.message_id, tx.attempt, tx.to), (t.message_id, 1, PEER));
        // Same id, fresh nonce (ADR-003): the frame differs.
        assert_ne!(tx.frame, t.frame);
        assert!(e.outbox_step(later, Some(ME)).1.is_none(), "in flight");
        let (st, _) = e.finish_attempt(&tx, Attempt::Submitted, later);
        assert_eq!(st, Some(DeliveryState::Submitted));
    }

    #[test]
    fn v1_messages_are_never_resent() {
        let mut e = engine("v1", WireProtocol::V1, true);
        let t = e.enqueue(PEER, &text(), NOW).unwrap();
        e.finish_attempt(&t, Attempt::Submitted, NOW);
        let (ev, tx) = e.outbox_step(NOW + 3600, Some(ME));
        assert!(ev.is_empty() && tx.is_none());
    }

    #[test]
    fn a_missing_radio_does_not_count_as_an_attempt() {
        let mut e = engine("away", WireProtocol::V2, true);
        let t = e.enqueue(PEER, &text(), NOW).unwrap();
        let (st, ev) = e.finish_attempt(&t, Attempt::Unavailable, NOW);
        assert_eq!(st, Some(DeliveryState::Queued));
        assert!(ev.is_none());
        let tx = e
            .outbox_step(NOW + 1, Some(ME))
            .1
            .expect("sent once the radio is back");
        assert_eq!(tx.attempt, 0);
    }

    #[test]
    fn deleting_during_the_write_wins() {
        let mut e = engine("delete", WireProtocol::V2, true);
        let t = e.enqueue(PEER, &text(), NOW).unwrap();
        let c = t.conversation.clone().unwrap();
        e.with_store(|s| s.delete_conversation(&c)).unwrap();
        assert_eq!(e.finish_attempt(&t, Attempt::Submitted, NOW), (None, None));
    }

    #[test]
    fn without_history_only_the_write_counts() {
        let mut e = engine("memory", WireProtocol::V2, false);
        let t = e.enqueue(PEER, &text(), NOW).unwrap();
        assert!(t.conversation.is_none());
        assert_eq!(
            e.finish_attempt(&t, Attempt::Submitted, NOW).0,
            Some(DeliveryState::Submitted)
        );
        assert_eq!(e.finish_attempt(&t, Attempt::Unavailable, NOW).0, None);
    }

    #[test]
    fn nothing_is_stored_without_a_key() {
        let mut e = engine("nokey", WireProtocol::V2, true);
        assert!(matches!(
            e.enqueue(0x33, &text(), NOW),
            Err(MeshGuardError::NoSession)
        ));
        assert!(e.with_store(|s| s.conversations()).unwrap().is_empty());
    }

    #[test]
    fn to_radio_packets_decode() {
        use meshtastic::Message as _;
        let bytes = to_radio_packet(
            ME,
            PEER,
            protobufs::PortNum::PrivateApp,
            vec![1, 2],
            true,
            7,
            9,
        );
        let tr = protobufs::ToRadio::decode(bytes.as_slice()).unwrap();
        let Some(protobufs::to_radio::PayloadVariant::Packet(p)) = tr.payload_variant else {
            panic!("packet expected");
        };
        assert_eq!((p.from, p.to, p.id, p.want_ack), (ME, PEER, 7, true));
        let want = protobufs::ToRadio::decode(to_radio_want_config(5).as_slice()).unwrap();
        assert!(matches!(
            want.payload_variant,
            Some(protobufs::to_radio::PayloadVariant::WantConfigId(5))
        ));
        assert_ne!(random_packet_id(), 0);
    }

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
            assert!((0.0..1.0).contains(&jitter()));
        }
    }
}
