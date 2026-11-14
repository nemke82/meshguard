//! Deterministic two-peer simulation over [`MockMedium`] with a virtual clock.
//!
//! `SimPeer` is a deliberately tiny test harness wiring the core pieces
//! together the way the future engine will: v2 envelope, dual-read codec,
//! dedup cache, outbox bookkeeping and an opaque-frame transport. It proves
//! the P1 acceptance points without any radio: stable ids, exactly-once
//! display, truthful delivery state and bounded retries.

use std::collections::HashMap;

use meshguard_core::codec::{decode_frame, DecodeError, DecodedFrame};
use meshguard_core::crypto::{derive_p2p_key, SessionKey};
use meshguard_core::dedup::{DedupCache, Seen};
use meshguard_core::delivery::{DeliveryState, DueAction, OutboundMessage, RetryPolicy};
use meshguard_core::envelope::{aad_context, EnvelopeV2, MessageType, DEFAULT_FRAME_BUDGET};
use meshguard_core::transport::{MockMedium, MockTransport, Transport, TransportAddress};
use meshguard_core::{MessageId, PeerIdentity};

/// Message TTL in minutes.
const TTL: u16 = 60;
const TTL_SECS: u64 = TTL as u64 * 60;
const SKEW: u64 = 300;
/// ACKs per received message id, across all its copies.
const MAX_ACKS: u32 = 3;

struct SimPeer {
    me: PeerIdentity,
    key: Option<SessionKey>,
    transport: MockTransport,
    dedup: DedupCache,
    policy: RetryPolicy,
    outbox: HashMap<MessageId, (OutboundMessage, EnvelopeV2)>,
    /// Texts shown to the user (one entry per notification).
    shown: Vec<String>,
    errors: Vec<DecodeError>,
    acks_sent: usize,
}

impl SimPeer {
    fn new(node: u32, my_name: &str, peer_name: &str, medium: &MockMedium) -> Self {
        let mut transport = MockTransport::new(
            "mock-ble",
            TransportAddress::MeshtasticNode(node),
            medium.clone(),
            DEFAULT_FRAME_BUDGET,
        );
        transport.start().unwrap();
        Self {
            me: PeerIdentity::meshtastic_node(node),
            key: Some(derive_p2p_key(my_name, peer_name, "shared secret").unwrap()),
            transport,
            dedup: DedupCache::new(1024, TTL_SECS + 2 * SKEW),
            policy: RetryPolicy {
                jitter_ratio: 0.0,
                ..RetryPolicy::default()
            },
            outbox: HashMap::new(),
            shown: Vec::new(),
            errors: Vec::new(),
            acks_sent: 0,
        }
    }

    fn send_text(&mut self, text: &str, now: u64) -> MessageId {
        let env = EnvelopeV2::text(text, now as u32, TTL);
        let id = env.message_id;
        let out = OutboundMessage::queued(id, now, env.expires_at());
        self.outbox.insert(id, (out, env));
        id
    }

    fn state(&self, id: &MessageId) -> DeliveryState {
        self.outbox[id].0.state
    }

    fn send_frame(&mut self, to: u32, frame: &[u8]) -> bool {
        self.transport
            .send(&TransportAddress::MeshtasticNode(to), frame)
            .is_ok()
    }

    fn tick(&mut self, now: u64, peer_node: u32) {
        let ctx = aad_context(&self.me, &PeerIdentity::meshtastic_node(peer_node));
        let due: Vec<(MessageId, DueAction)> = self
            .outbox
            .iter()
            .filter_map(|(id, (o, _))| o.due_action(now).map(|a| (*id, a)))
            .collect();
        for (id, action) in due {
            match action {
                DueAction::Send => {
                    // Re-seal on every attempt: same id, fresh nonce.
                    let frame = {
                        let key = self.key.as_ref().unwrap();
                        self.outbox[&id]
                            .1
                            .seal(key, &ctx, DEFAULT_FRAME_BUDGET)
                            .unwrap()
                    };
                    let ok = self.send_frame(peer_node, &frame);
                    let (o, _) = self.outbox.get_mut(&id).unwrap();
                    if ok {
                        o.record_submitted(now, self.transport.id(), &self.policy)
                            .unwrap();
                    } else {
                        o.record_submit_failed(now, &self.policy, 0.0).unwrap();
                    }
                }
                DueAction::AckTimedOut => {
                    let policy = self.policy;
                    let (o, _) = self.outbox.get_mut(&id).unwrap();
                    o.record_ack_timeout(now, &policy, 0.0).unwrap();
                }
                DueAction::Expire => self.outbox.get_mut(&id).unwrap().0.expire().unwrap(),
            }
        }
    }

    fn receive(&mut self, now: u64) {
        for inc in self.transport.poll_incoming() {
            let TransportAddress::MeshtasticNode(from_node) = inc.from else {
                continue;
            };
            let sender = PeerIdentity::meshtastic_node(from_node);
            let ctx = aad_context(&sender, &self.me);
            let env = match decode_frame(&inc.frame, self.key.as_ref(), &ctx) {
                Ok(DecodedFrame::V2(env)) => env,
                Ok(DecodedFrame::V1(_)) => continue,
                Err(e) => {
                    self.errors.push(e);
                    continue;
                }
            };
            if env.check_freshness(now, SKEW, TTL).is_err() {
                continue;
            }
            let seen = self.dedup.observe(&sender, env.message_id, now);
            match env.message_type {
                MessageType::AppAck => {
                    if let Some(acked) = env.acked_id() {
                        if let Some((o, _)) = self.outbox.get_mut(&acked) {
                            o.record_peer_ack().unwrap();
                        }
                    }
                }
                MessageType::Text => {
                    if seen == Seen::New {
                        self.shown.push(env.text_body().unwrap().to_string());
                    }
                    // ACK duplicates too so a lost ACK can recover, but only
                    // up to MAX_ACKS per id; and never ACK an ACK.
                    if !seen.should_ack(MAX_ACKS) {
                        continue;
                    }
                    let ack = EnvelopeV2::app_ack(env.message_id, now as u32, 10);
                    let back = aad_context(&self.me, &sender);
                    let frame = ack
                        .seal(self.key.as_ref().unwrap(), &back, DEFAULT_FRAME_BUDGET)
                        .unwrap();
                    if self.send_frame(from_node, &frame) {
                        self.acks_sent += 1;
                    }
                }
                MessageType::PairRequest | MessageType::PairAccept => {}
            }
        }
    }
}

struct Sim {
    medium: MockMedium,
    a: SimPeer,
    b: SimPeer,
    now: u64,
}

impl Sim {
    fn new() -> Self {
        let medium = MockMedium::new();
        let a = SimPeer::new(1, "Alice-T1000", "Bob-T1000", &medium);
        let b = SimPeer::new(2, "Bob-T1000", "Alice-T1000", &medium);
        Self {
            medium,
            a,
            b,
            now: 1_763_100_000,
        }
    }

    /// Advance virtual time in `step`-second increments.
    fn run(&mut self, secs: u64, step: u64) {
        let end = self.now + secs;
        while self.now < end {
            self.a.tick(self.now, 2);
            self.b.tick(self.now, 1);
            self.b.receive(self.now);
            self.a.receive(self.now);
            self.now += step;
        }
    }
}

#[test]
fn happy_path_reaches_peer_ack_once() {
    let mut s = Sim::new();
    let id = s.a.send_text("Zdravo Bob", s.now);
    assert_eq!(s.a.state(&id), DeliveryState::Queued);
    s.run(5, 1);
    assert_eq!(s.b.shown, vec!["Zdravo Bob"]);
    assert_eq!(s.a.state(&id), DeliveryState::PeerAck);
    assert!(s.a.state(&id).is_delivered());
    assert_eq!(s.a.transport.sent.len(), 1, "exactly one transmission");
}

#[test]
fn relayed_duplicates_show_once() {
    let mut s = Sim::new();
    s.medium.duplicate_next(1);
    let id = s.a.send_text("echo", s.now);
    s.run(5, 1);
    assert_eq!(
        s.b.shown,
        vec!["echo"],
        "one notification despite two copies"
    );
    assert_eq!(s.b.acks_sent, 2, "each copy gets a safe ACK");
    assert_eq!(s.a.state(&id), DeliveryState::PeerAck);
}

#[test]
fn lost_ack_triggers_retry_with_same_id_and_no_duplicate_display() {
    let mut s = Sim::new();
    let id = s.a.send_text("retry", s.now);
    // Frame 1 (A's text) goes through; frame 2 (B's ACK) is lost.
    s.a.tick(s.now, 2);
    s.medium.drop_next(1);
    s.b.receive(s.now);
    s.a.receive(s.now);
    assert_eq!(s.a.state(&id), DeliveryState::Submitted);
    assert!(!s.a.state(&id).is_delivered(), "submission is not delivery");

    s.run(200, 5);
    assert_eq!(s.a.state(&id), DeliveryState::PeerAck);
    assert_eq!(s.b.shown, vec!["retry"]);
    assert_eq!(s.a.transport.sent.len(), 2, "one original + one retry");

    let ids: Vec<MessageId> =
        s.a.transport
            .sent
            .iter()
            .map(|(_, f)| {
                let ctx = aad_context(
                    &PeerIdentity::meshtastic_node(1),
                    &PeerIdentity::meshtastic_node(2),
                );
                EnvelopeV2::open(f, s.b.key.as_ref().unwrap(), &ctx)
                    .unwrap()
                    .message_id
            })
            .collect();
    assert_eq!(ids, vec![id, id], "retry reuses the message id");
    assert_ne!(
        s.a.transport.sent[0].1, s.a.transport.sent[1].1,
        "fresh nonce"
    );
}

#[test]
fn link_down_keeps_message_queued_then_delivers() {
    let mut s = Sim::new();
    let a_addr = TransportAddress::MeshtasticNode(1);
    s.medium.set_down(&a_addr, true);
    let id = s.a.send_text("when back", s.now);
    s.run(10, 1);
    assert_eq!(s.a.state(&id), DeliveryState::Retrying);
    assert!(s.b.shown.is_empty());

    s.medium.set_down(&a_addr, false);
    s.run(120, 5);
    assert_eq!(s.a.state(&id), DeliveryState::PeerAck);
    assert_eq!(s.b.shown, vec!["when back"]);
}

#[test]
fn unreachable_peer_fails_after_bounded_attempts() {
    let mut s = Sim::new();
    s.b.transport.stop();
    let id = s.a.send_text("into the void", s.now);
    s.run(3000, 5);
    assert_eq!(s.a.state(&id), DeliveryState::Failed);
    assert!(!s.a.state(&id).is_delivered());
    assert_eq!(
        s.a.transport.sent.len() as u32,
        s.a.policy.max_attempts,
        "airtime bounded by max_attempts"
    );
}

#[test]
fn ttl_expiry_is_reported_truthfully() {
    let mut s = Sim::new();
    s.b.transport.stop();
    s.a.policy.max_attempts = 1000;
    s.a.policy.max_delay_secs = 120;
    let id = s.a.send_text("too late", s.now);
    s.run(TTL_SECS + 60, 10);
    assert_eq!(s.a.state(&id), DeliveryState::Expired);
}

#[test]
fn acks_are_never_acked() {
    let mut s = Sim::new();
    s.a.send_text("one", s.now);
    s.run(300, 5);
    assert_eq!(s.a.acks_sent, 0, "A never ACKs B's ACK");
    assert_eq!(s.medium.delivered_count(), 2, "text + ack, nothing else");
}

#[test]
fn reflected_frame_is_rejected() {
    let mut s = Sim::new();
    s.a.send_text("mine", s.now);
    s.a.tick(s.now, 2);
    // An attacker replays A's own frame to A, claiming to be B.
    let (_, frame) = s.a.transport.sent[0].clone();
    let mut attacker = MockTransport::new(
        "evil",
        TransportAddress::MeshtasticNode(2),
        s.medium.clone(),
        DEFAULT_FRAME_BUDGET,
    );
    attacker.start().unwrap();
    attacker
        .send(&TransportAddress::MeshtasticNode(1), &frame)
        .unwrap();
    s.a.receive(s.now);
    assert!(s.a.shown.is_empty());
    assert_eq!(s.a.errors, vec![DecodeError::AuthFailed]);
}

#[test]
fn locked_receiver_shows_nothing_and_reports_locked() {
    let mut s = Sim::new();
    s.b.key = None;
    let id = s.a.send_text("secret", s.now);
    s.run(5, 1);
    assert!(s.b.shown.is_empty());
    assert_eq!(s.b.errors, vec![DecodeError::Locked]);
    assert_ne!(s.a.state(&id), DeliveryState::PeerAck);
}

#[test]
fn dedup_survives_restart() {
    let mut s = Sim::new();
    s.a.send_text("persist", s.now);
    s.a.tick(s.now, 2);
    let (_, frame) = s.a.transport.sent[0].clone();
    s.b.receive(s.now);
    assert_eq!(s.b.shown.len(), 1);

    // "Restart" B: rebuild its dedup cache from persisted entries.
    let saved: Vec<_> =
        s.b.dedup
            .entries()
            .map(|(p, i, t, n)| (p.clone(), *i, t, n))
            .collect();
    s.b.dedup = DedupCache::restore(1024, TTL_SECS + 2 * SKEW, saved, s.now + 10);
    s.b.shown.clear();

    // The same frame is replayed after the restart.
    s.a.send_frame(2, &frame);
    s.b.receive(s.now + 10);
    assert!(
        s.b.shown.is_empty(),
        "replay after restart is not shown again"
    );
}

#[test]
fn replay_flood_triggers_bounded_acks() {
    let mut s = Sim::new();
    s.a.send_text("flood me", s.now);
    s.a.tick(s.now, 2);
    let (_, frame) = s.a.transport.sent[0].clone();
    s.b.receive(s.now);
    // An attacker replays the captured frame 50 times.
    for _ in 0..50 {
        s.a.send_frame(2, &frame);
    }
    s.b.receive(s.now + 1);
    assert_eq!(s.b.shown, vec!["flood me"]);
    assert_eq!(s.b.acks_sent, MAX_ACKS as usize, "ACK airtime is bounded");
}

#[test]
fn over_long_ttl_is_rejected() {
    let mut s = Sim::new();
    let env = EnvelopeV2::text("forever", s.now as u32, TTL + 1);
    let ctx = aad_context(
        &PeerIdentity::meshtastic_node(1),
        &PeerIdentity::meshtastic_node(2),
    );
    let frame = env
        .seal(s.a.key.as_ref().unwrap(), &ctx, DEFAULT_FRAME_BUDGET)
        .unwrap();
    s.a.send_frame(2, &frame);
    s.b.receive(s.now);
    assert!(s.b.shown.is_empty());
    assert_eq!(s.b.acks_sent, 0);
}
