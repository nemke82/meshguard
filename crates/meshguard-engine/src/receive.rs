//! What happens when the radio hands us something: node updates and
//! MeshGuard messages. Pure decisions on [`Engine`] state; the I/O comes
//! back as [`Action`]s for the caller (the Tauri listener today, the
//! Android service later).

use meshguard_core::inbound::{process_inbound, Inbound, InboundContext};
use meshguard_core::outbound::WireVersion;
use meshguard_core::status::{StatusBody, StatusKind, FEATURE_STATUS, SUPPORTED_FEATURES};
use meshguard_core::{MessageId, PeerIdentity};
use meshguard_store::MessageKind;
use meshtastic::protobufs;
use meshtastic::protobufs::from_radio::PayloadVariant;
use serde::Serialize;

use crate::config::WireProtocol;
use crate::diagnostics::SignalSample;
use crate::engine::{Engine, Slot};

/// Something the caller has to do after the engine handled a packet.
#[derive(Debug)]
pub enum Action {
    /// Transmit an application ACK to `to` (PrivateApp port), if the link
    /// this packet came from is still the current one. The airtime budget
    /// was already charged.
    SendAck { to: u32, frame: Vec<u8> },
    /// Tell the UI.
    Emit(Event),
    /// Post a system notification (Android).
    Notify {
        title: String,
        body: String,
        node: u32,
    },
    /// Our radio reported its GPS position.
    MyPosition(protobufs::Position),
    /// A path test (traceroute) reply addressed to us.
    PathReply { from: u32, data: protobufs::Data },
    /// Signal, hops or battery to record for a node's diagnostics.
    Sample { node: u32, sample: SignalSample },
    /// A peer app confirmed one of our messages: update the outbox, then
    /// emit `message-delivered`. Left to the caller because the outbox is
    /// still worked on outside the engine.
    PeerAck { peer: u32, message_id: MessageId },
    /// A frame no chat key opens. The caller checks it against a QR offer
    /// it is showing, and otherwise hands it back through
    /// [`Engine::note_pair_request`].
    Undecryptable { from: u32, payload: Vec<u8> },
    /// A peer proved it holds our key for the first time: probe which
    /// message types it understands.
    ProbePeers,
}

/// UI events (the names are the Tauri event names).
#[derive(Debug, Clone)]
pub enum Event {
    NodesUpdated,
    IncomingMessage(IncomingMessageEvent),
    IncomingStatus(IncomingStatusEvent),
    PeerProtocol(PeerProtocolEvent),
    PairRequest(PairRequestEvent),
    PairAccepted(PairRequestEvent),
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Self::NodesUpdated => "mesh-nodes-updated",
            Self::IncomingMessage(_) => "incoming-message",
            Self::IncomingStatus(_) => "incoming-status",
            Self::PeerProtocol(_) => "peer-protocol",
            Self::PairRequest(_) => "pair-request",
            Self::PairAccepted(_) => "pair-accepted",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IncomingMessageEvent {
    pub from_node: u32,
    pub from_name: String,
    pub text: String,
    pub timestamp: i64,
    pub message_id: String,
    /// Set when the message was saved to the history store.
    pub conversation_id: Option<String>,
}

/// A check-in, SOS or location share from a peer.
#[derive(Debug, Clone, Serialize)]
pub struct IncomingStatusEvent {
    pub from_node: u32,
    pub from_name: String,
    pub message_id: String,
    pub status: StatusBody,
    /// Sender clock, unix seconds.
    pub timestamp: i64,
    /// Set when the message was saved to the history store.
    pub conversation_id: Option<String>,
}

/// The peer app confirmed receipt of one of our v2 messages (authenticated).
#[derive(Debug, Clone, Serialize)]
pub struct MessageDeliveredEvent {
    pub peer_node: u32,
    pub message_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeerProtocolEvent {
    pub node_num: u32,
    pub protocol: WireProtocol,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairRequestEvent {
    pub from_node: u32,
    pub from_name: String,
    pub timestamp: i64,
}

/// What happened to an incoming message in the history.
enum Recorded {
    /// Saved in this conversation.
    Stored(String),
    /// History is memory-only on this device (by design, not an error).
    MemoryOnly,
    /// History is on but the write failed: no ACK, so the sender retries
    /// and the message is not lost from the history.
    Failed,
}

impl Engine {
    /// One packet from the radio of `my_node` (the link it came from).
    /// `now`: unix seconds.
    pub fn on_from_radio(
        &mut self,
        from_radio: &protobufs::FromRadio,
        my_node: u32,
        now: u64,
    ) -> Vec<Action> {
        match &from_radio.payload_variant {
            Some(PayloadVariant::NodeInfo(ni)) => {
                let node = crate::nodes::from_node_info(ni, now as i64);
                if node.node_num == my_node {
                    return Vec::new();
                }
                tracing::info!("Mesh node update: {} ({})", node.long_name, node.node_num);
                self.nodes.insert(node.node_num, node);
                vec![Action::Emit(Event::NodesUpdated)]
            }
            Some(PayloadVariant::Packet(packet)) => self.on_mesh_packet(packet, my_node, now),
            _ => Vec::new(),
        }
    }

    /// A mesh packet: update the sender's node entry, then handle MeshGuard
    /// frames (PrivateApp port).
    pub fn on_mesh_packet(
        &mut self,
        packet: &protobufs::MeshPacket,
        my_node: u32,
        now: u64,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let seen = crate::nodes::observe(&mut self.nodes, packet, my_node, now as i64);
        if let Some(pos) = seen.my_position {
            actions.push(Action::MyPosition(pos));
        }
        if let Some(data) = seen.path_reply {
            actions.push(Action::PathReply {
                from: packet.from,
                data,
            });
        }
        if let Some(sample) = seen.sample {
            actions.push(Action::Sample {
                node: packet.from,
                sample,
            });
        }
        if seen.nodes_changed {
            actions.push(Action::Emit(Event::NodesUpdated));
        }
        if packet.from != my_node && packet.from != 0 {
            self.on_private_app(packet, my_node, now, &mut actions);
        }
        actions
    }

    fn on_private_app(
        &mut self,
        packet: &protobufs::MeshPacket,
        my_node: u32,
        now: u64,
        actions: &mut Vec<Action>,
    ) {
        let decoded = match &packet.payload_variant {
            Some(protobufs::mesh_packet::PayloadVariant::Decoded(d)) => d,
            Some(protobufs::mesh_packet::PayloadVariant::Encrypted(e)) => {
                tracing::warn!(
                    "Received encrypted packet from {} ({} bytes) — cannot process. \
                     Both devices must share the same Meshtastic channel PSK.",
                    packet.from,
                    e.len()
                );
                return;
            }
            None => return,
        };
        if decoded.portnum != protobufs::PortNum::PrivateApp as i32 {
            return;
        }
        let from_node = packet.from;
        let payload = &decoded.payload;
        if payload.is_empty() {
            tracing::debug!("Empty PrivateApp payload from {from_node}");
            return;
        }
        tracing::info!(
            "PrivateApp message from node {from_node} ({} bytes)",
            payload.len()
        );

        let sender = PeerIdentity::meshtastic_node(from_node);
        let me = PeerIdentity::meshtastic_node(my_node);
        let pinned: Option<WireVersion> = self
            .config
            .peers
            .iter()
            .find(|p| p.node_num == from_node)
            .map(|p| p.protocol.into());

        // Keys must belong to this radio (they are bound to its identity).
        let key = if self.keys_owner == Some(my_node) {
            self.keys.get(&from_node)
        } else {
            None
        };
        let ictx = InboundContext {
            sender: &sender,
            me: &me,
            key,
            pinned,
            now,
        };
        let policy = self.protection.policy;
        let outcome = process_inbound(payload, &ictx, &mut self.protection.dedup, &policy);
        let is_new = matches!(
            outcome,
            Inbound::Text {
                message_id: Some(_),
                ..
            } | Inbound::Status { .. }
        );
        // A new message's id is saved only once the message is in the
        // history (commit_id). Other authenticated v2 frames: now.
        if !is_new && outcome.authenticated_version() == Some(WireVersion::V2) {
            if let Err(e) = self.save_dedup() {
                tracing::warn!("Could not persist dedup cache: {e}");
            }
        }

        // ADR-003 §3 rule (a): an authenticated v2 frame upgrades a v1 peer.
        if outcome.authenticated_version() == Some(WireVersion::V2)
            && pinned == Some(WireVersion::V1)
        {
            self.upgrade_peer_to_v2(from_node, actions);
        }

        // Any authenticated frame proves the peer holds our key; from then
        // on background frames (feature probes) cannot be mistaken for a
        // request.
        if outcome.authenticated_version().is_some() {
            let before = self
                .config
                .peers
                .iter()
                .any(|p| p.node_num == from_node && p.confirmed);
            crate::config::mark_peer_confirmed(&mut self.config, &self.config_dir, from_node);
            let newly = !before && self.config.peers.iter().any(|p| p.node_num == from_node);
            if newly {
                actions.push(Action::ProbePeers);
            }
        }

        match outcome {
            Inbound::Text {
                version,
                message_id,
                text,
                sent_at,
                ack,
            } => {
                // v1 frames carry no id: give the stored copy a local one.
                let wire_id = message_id;
                let id = message_id.unwrap_or_else(MessageId::random);
                let sent_at = sent_at.map(u64::from).unwrap_or(now);
                // Saved before the UI hears of it (and before the ACK), so
                // a message the user saw is never missing from the history.
                let recorded = self.record_incoming(
                    my_node,
                    from_node,
                    id,
                    MessageKind::Text,
                    text.as_bytes(),
                    sent_at,
                    now,
                );
                let Some(conversation_id) = self.commit_id(&sender, wire_id.as_ref(), recorded)
                else {
                    return;
                };
                // Never log decrypted content (RFC §5.4).
                tracing::info!(
                    "Incoming {version:?} text from {from_node} ({} bytes, id={id})",
                    text.len()
                );
                actions.push(Action::Notify {
                    title: "New Message".into(),
                    body: text.clone(),
                    node: from_node,
                });
                actions.push(Action::Emit(Event::IncomingMessage(IncomingMessageEvent {
                    from_node,
                    from_name: String::new(),
                    text,
                    timestamp: sent_at as i64,
                    message_id: id.to_hex(),
                    conversation_id,
                })));
                self.ack(from_node, ack, now, actions);
            }
            Inbound::Status {
                message_id,
                status,
                sent_at,
                ack,
            } => {
                tracing::info!(
                    "Incoming status {:?} from {from_node} (location: {}, id={message_id})",
                    status.kind,
                    status.location.is_some()
                );
                self.mark_features(from_node, FEATURE_STATUS);
                let recorded = match status.encode() {
                    Ok(body) => self.record_incoming(
                        my_node,
                        from_node,
                        message_id,
                        MessageKind::Status,
                        &body,
                        u64::from(sent_at),
                        now,
                    ),
                    Err(_) => Recorded::Failed,
                };
                let Some(conversation_id) = self.commit_id(&sender, Some(&message_id), recorded)
                else {
                    return;
                };
                let from_name = self
                    .node_name(from_node)
                    .unwrap_or_else(|| format!("!{from_node:08x}"));
                let title = match status.kind {
                    StatusKind::Sos => format!("\u{1F198} SOS from {from_name}"),
                    StatusKind::NeedHelp => format!("\u{26A0}\u{FE0F} {from_name} needs help"),
                    StatusKind::Location => format!("\u{1F4CD} {from_name} shared a location"),
                    _ => format!("{from_name} checked in"),
                };
                actions.push(Action::Notify {
                    title,
                    body: status.fallback_text(),
                    node: from_node,
                });
                actions.push(Action::Emit(Event::IncomingStatus(IncomingStatusEvent {
                    from_node,
                    from_name,
                    message_id: message_id.to_hex(),
                    status,
                    timestamp: i64::from(sent_at),
                    conversation_id,
                })));
                self.ack(from_node, ack, now, actions);
            }
            Inbound::Caps { features, ack } => {
                tracing::info!("Node {from_node} announced features {features:#x}");
                self.mark_features(from_node, features & SUPPORTED_FEATURES);
                self.ack(from_node, ack, now, actions);
            }
            Inbound::Duplicate { message_id, ack } => {
                tracing::debug!("Duplicate frame from {from_node} suppressed");
                // Messages are handled one at a time under the engine lock,
                // so the first copy is saved by now unless saving failed
                // (then its id was forgotten and this is not a duplicate).
                if self.protection.dedup.contains(&sender, &message_id) {
                    self.ack(from_node, ack, now, actions);
                }
            }
            Inbound::PeerAck { acked } => {
                tracing::info!("Peer ACK from {from_node} for {acked}");
                let probed = self
                    .protection
                    .caps_probes
                    .get(&from_node)
                    .is_some_and(|(id, _)| *id == acked);
                if probed {
                    self.protection.caps_probes.remove(&from_node);
                    tracing::info!("Node {from_node} understands check-in/SOS messages");
                    self.mark_features(from_node, FEATURE_STATUS);
                    return;
                }
                actions.push(Action::PeerAck {
                    peer: from_node,
                    message_id: acked,
                });
            }
            Inbound::PairAccept { version, name } => {
                tracing::info!("Pair accepted ({version:?}) by node {from_node}");
                actions.push(Action::Emit(Event::PairAccepted(PairRequestEvent {
                    from_node,
                    from_name: name,
                    timestamp: now as i64,
                })));
            }
            Inbound::PairRequest { version, .. } => {
                tracing::info!(
                    "Pair request ({version:?}) from node {from_node} — already have key"
                );
            }
            Inbound::Dropped(reason) => {
                tracing::info!("Dropped frame from {from_node}: {reason:?}");
            }
            Inbound::Undecryptable(reason) => {
                tracing::info!(
                    "Cannot decrypt frame from {from_node} ({reason}) — treating as pair request"
                );
                actions.push(Action::Undecryptable {
                    from: from_node,
                    payload: payload.clone(),
                });
            }
        }
    }

    /// Keep an undecryptable frame as a pending chat request and, subject
    /// to rate limits, tell the user. Rate limiting stops unknown nodes on
    /// a public channel from flooding the user with notifications.
    pub fn note_pair_request(&mut self, from_node: u32, payload: Vec<u8>, now: u64) -> Vec<Action> {
        self.pending_pair_requests.insert(from_node, payload);
        let allowed = self
            .protection
            .pair_notify_per_node
            .try_fire(&from_node, now)
            && self.protection.pair_notify_global.try_take(now);
        if !allowed {
            tracing::info!("Pair-request notification from {from_node} rate-limited");
            return Vec::new();
        }
        let from_name = self.node_name(from_node).unwrap_or_default();
        let body = if from_name.is_empty() {
            format!("Node {from_node} wants to start a secure chat")
        } else {
            format!("{from_name} wants to start a secure chat")
        };
        vec![
            Action::Notify {
                title: "Chat Request".into(),
                body,
                node: from_node,
            },
            Action::Emit(Event::PairRequest(PairRequestEvent {
                from_node,
                from_name,
                timestamp: now as i64,
            })),
        ]
    }

    /// Queue an ACK if the global airtime budget allows it.
    fn ack(&mut self, to: u32, frame: Option<Vec<u8>>, now: u64, actions: &mut Vec<Action>) {
        let Some(frame) = frame else { return };
        if !self.protection.ack_budget.try_take(now) {
            tracing::info!("ACK to {to} skipped: airtime budget exhausted");
            return;
        }
        actions.push(Action::SendAck { to, frame });
    }

    #[allow(clippy::too_many_arguments)]
    fn record_incoming(
        &mut self,
        my_node: u32,
        peer: u32,
        message_id: MessageId,
        kind: MessageKind,
        body: &[u8],
        created_at: u64,
        now: u64,
    ) -> Recorded {
        let title = self.peer_title(peer);
        let store = match &mut self.history {
            Slot::Open(store) => store,
            Slot::Off(_) => return Recorded::MemoryOnly,
            // Not opened yet: not knowing whether history is on, do not
            // ACK (the sender retries once it is open).
            Slot::Closed => return Recorded::Failed,
        };
        let r = (|| {
            let c = store.upsert_conversation(my_node, peer, title.as_deref())?;
            store.insert_incoming(&c, message_id, kind, body, created_at, now)?;
            Ok::<_, meshguard_store::StoreError>(c.0)
        })();
        match r {
            Ok(c) => Recorded::Stored(c),
            Err(e) => {
                tracing::warn!("Could not save an incoming message: {e}");
                Recorded::Failed
            }
        }
    }

    /// After saving an incoming message: make its id durable.
    /// `Some(conversation id or None)` = go on (show, notify, ACK); `None` =
    /// drop the frame unACKed (the id is forgotten, so the sender's retry
    /// delivers it again). v1 frames (no id) cannot be retried by the
    /// sender and are shown even when saving failed.
    fn commit_id(
        &mut self,
        sender: &PeerIdentity,
        wire_id: Option<&MessageId>,
        recorded: Recorded,
    ) -> Option<Option<String>> {
        let conversation = match recorded {
            Recorded::Stored(c) => Some(c),
            Recorded::MemoryOnly => None,
            Recorded::Failed => {
                let Some(id) = wire_id else {
                    return Some(None);
                };
                self.protection.dedup.forget(sender, id);
                tracing::warn!("Dropped a message until it can be saved; the sender will retry");
                return None;
            }
        };
        let Some(id) = wire_id else {
            return Some(conversation);
        };
        if let Err(e) = self.save_dedup() {
            tracing::warn!("Could not persist dedup cache: {e}");
            // Fail closed: without a durable record, a replay after restart
            // would be shown again. Forget the id and drop the frame
            // unACKed; the sender's retry delivers it (the history ignores
            // the copy).
            self.protection.dedup.forget(sender, id);
            return None;
        }
        Some(conversation)
    }

    fn mark_features(&mut self, node: u32, bits: u32) {
        if bits != 0 {
            crate::config::mark_peer_features(&mut self.config, &self.config_dir, node, bits);
        }
    }

    fn upgrade_peer_to_v2(&mut self, node_num: u32, actions: &mut Vec<Action>) {
        let Some(peer) = self
            .config
            .peers
            .iter_mut()
            .find(|p| p.node_num == node_num)
        else {
            return;
        };
        peer.protocol = WireProtocol::V2;
        match self.config.save(&self.config_dir) {
            Ok(()) => tracing::info!("Peer {node_num} upgraded to wire protocol v2"),
            Err(e) => tracing::warn!("Peer {node_num} upgraded to v2 but saving failed: {e}"),
        }
        actions.push(Action::Emit(Event::PeerProtocol(PeerProtocolEvent {
            node_num,
            protocol: WireProtocol::V2,
        })));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PeerConfig;
    use meshguard_core::crypto::SessionKey;
    use meshguard_core::envelope::DEFAULT_FRAME_BUDGET;
    use meshguard_core::outbound::{seal, OutPayload};

    const ME: u32 = 0x11;
    const PEER: u32 = 0x22;
    const NOW: u64 = 1_763_200_000;

    fn key() -> SessionKey {
        SessionKey::from_bytes([7; 32])
    }

    fn engine(name: &str, protocol: WireProtocol, history: bool) -> Engine {
        let dir = std::env::temp_dir().join(format!("mg-recv-{}-{name}", std::process::id()));
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
        e.adopt_keys(ME, vec![(PEER, key())]);
        e.open_history(history.then(|| SessionKey::from_bytes([1; 32])).as_ref());
        e
    }

    fn frame(version: WireVersion, payload: OutPayload<'_>) -> Vec<u8> {
        seal(
            version,
            payload,
            &key(),
            &PeerIdentity::meshtastic_node(PEER),
            &PeerIdentity::meshtastic_node(ME),
            NOW as u32,
            DEFAULT_FRAME_BUDGET,
        )
        .unwrap()
        .frame
    }

    fn packet(from: u32, payload: Vec<u8>) -> protobufs::MeshPacket {
        protobufs::MeshPacket {
            from,
            to: ME,
            payload_variant: Some(protobufs::mesh_packet::PayloadVariant::Decoded(
                protobufs::Data {
                    portnum: protobufs::PortNum::PrivateApp as i32,
                    payload,
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    fn incoming(actions: &[Action]) -> Vec<&IncomingMessageEvent> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Emit(Event::IncomingMessage(m)) => Some(m),
                _ => None,
            })
            .collect()
    }

    fn acks(actions: &[Action]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, Action::SendAck { to: PEER, .. }))
            .count()
    }

    #[test]
    fn a_v2_text_is_saved_shown_and_acked_once() {
        let mut e = engine("text", WireProtocol::V2, true);
        let p = packet(PEER, frame(WireVersion::V2, OutPayload::Text("hi")));

        let a = e.on_mesh_packet(&p, ME, NOW);
        let msgs = incoming(&a);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "hi");
        let conv = msgs[0].conversation_id.clone().expect("saved to history");
        assert_eq!(acks(&a), 1);
        assert!(a.iter().any(|a| matches!(a, Action::Notify { .. })));
        let stored = e
            .with_store(|s| s.messages(&meshguard_core::ids::ConversationId(conv), None, 10))
            .unwrap();
        assert_eq!(stored.len(), 1);

        // The sender retries (our ACK was lost): ACK again, do not show.
        let again = e.on_mesh_packet(&p, ME, NOW + 1);
        assert!(incoming(&again).is_empty());
        assert_eq!(acks(&again), 1);
    }

    #[test]
    fn nothing_is_acked_before_history_is_opened() {
        let dir = std::env::temp_dir().join(format!("mg-recv-{}-closed", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = Engine::new(dir, NOW);
        e.adopt_keys(ME, vec![(PEER, key())]);
        let p = packet(PEER, frame(WireVersion::V2, OutPayload::Text("hi")));
        let a = e.on_mesh_packet(&p, ME, NOW);
        assert!(incoming(&a).is_empty());
        assert_eq!(acks(&a), 0);
        // Once open, the sender's retry is saved and shown.
        e.open_history(Some(&SessionKey::from_bytes([1; 32])));
        let a = e.on_mesh_packet(&p, ME, NOW + 1);
        assert!(incoming(&a)[0].conversation_id.is_some());
        assert_eq!(acks(&a), 1);
    }

    #[test]
    fn without_history_messages_are_still_shown() {
        let mut e = engine("memory", WireProtocol::V2, false);
        let p = packet(PEER, frame(WireVersion::V2, OutPayload::Text("hi")));
        let a = e.on_mesh_packet(&p, ME, NOW);
        assert_eq!(incoming(&a)[0].conversation_id, None);
        assert_eq!(acks(&a), 1);
    }

    #[test]
    fn a_v2_frame_upgrades_a_v1_peer() {
        let mut e = engine("upgrade", WireProtocol::V1, false);
        let p = packet(PEER, frame(WireVersion::V2, OutPayload::Text("hi")));
        let a = e.on_mesh_packet(&p, ME, NOW);
        assert!(a
            .iter()
            .any(|a| matches!(a, Action::Emit(Event::PeerProtocol(_)))));
        assert_eq!(e.peer_wire(PEER).0, WireProtocol::V2);
    }

    #[test]
    fn unknown_senders_become_rate_limited_chat_requests() {
        let mut e = engine("pair", WireProtocol::V2, false);
        let p = packet(
            0x33,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17],
        );
        let a = e.on_mesh_packet(&p, ME, NOW);
        let Some(Action::Undecryptable { from, payload }) = a
            .into_iter()
            .find(|a| matches!(a, Action::Undecryptable { .. }))
        else {
            panic!("expected an undecryptable frame");
        };
        assert_eq!(from, 0x33);
        let first = e.note_pair_request(from, payload.clone(), NOW);
        assert!(first
            .iter()
            .any(|a| matches!(a, Action::Emit(Event::PairRequest(_)))));
        // Same node again right away: kept, but no second notification.
        assert!(e.note_pair_request(from, payload, NOW + 1).is_empty());
        assert!(e.pending_pair_requests.contains_key(&0x33));
    }

    #[test]
    fn peer_acks_go_to_the_outbox_and_probe_acks_mark_features() {
        let mut e = engine("acks", WireProtocol::V2, false);
        let id = MessageId::random();
        let ack = meshguard_core::outbound::seal_ack(
            id,
            &key(),
            &PeerIdentity::meshtastic_node(PEER),
            &PeerIdentity::meshtastic_node(ME),
            NOW as u32,
            DEFAULT_FRAME_BUDGET,
        )
        .unwrap();
        let a = e.on_mesh_packet(&packet(PEER, ack.clone()), ME, NOW);
        assert!(a
            .iter()
            .any(|a| matches!(a, Action::PeerAck { peer: PEER, message_id } if *message_id == id)));

        let probe = MessageId::random();
        e.protection.caps_probes.insert(PEER, (probe, NOW));
        let ack = meshguard_core::outbound::seal_ack(
            probe,
            &key(),
            &PeerIdentity::meshtastic_node(PEER),
            &PeerIdentity::meshtastic_node(ME),
            NOW as u32,
            DEFAULT_FRAME_BUDGET,
        )
        .unwrap();
        let a = e.on_mesh_packet(&packet(PEER, ack), ME, NOW);
        assert!(!a.iter().any(|a| matches!(a, Action::PeerAck { .. })));
        assert!(
            e.peer_wire(PEER).1,
            "peer now known to understand check-ins"
        );
    }

    #[test]
    fn keys_of_another_radio_are_not_used() {
        let mut e = engine("owner", WireProtocol::V2, false);
        let p = packet(PEER, frame(WireVersion::V2, OutPayload::Text("hi")));
        // The packet came through radio 0x99, but the keys belong to ME.
        let a = e.on_mesh_packet(&p, 0x99, NOW);
        assert!(incoming(&a).is_empty());
    }

    #[test]
    fn node_info_updates_the_list_but_not_our_own_radio() {
        let mut e = engine("nodeinfo", WireProtocol::V2, false);
        let fr = |num| protobufs::FromRadio {
            payload_variant: Some(PayloadVariant::NodeInfo(protobufs::NodeInfo {
                num,
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(e.on_from_radio(&fr(ME), ME, NOW).is_empty());
        assert_eq!(e.on_from_radio(&fr(0x44), ME, NOW).len(), 1);
        assert!(e.nodes.contains_key(&0x44));
    }
}
