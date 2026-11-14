//! Receive-side decisions for one incoming frame (ADR-003 §2, §3).
//!
//! [`process_inbound`] is pure: it decodes (v1/v2 dual-read), enforces the
//! peer's pinned version, checks freshness, deduplicates and prepares an ACK
//! frame. The caller performs the I/O (notify, emit, transmit the ACK subject
//! to an airtime budget, persist a version upgrade).

use crate::codec::{decode_frame, DecodeError, DecodedFrame};
use crate::crypto::SessionKey;
use crate::dedup::{DedupCache, Seen};
use crate::envelope::{aad_context, FreshnessError, MessageType, DEFAULT_FRAME_BUDGET};
use crate::ids::{MessageId, PeerIdentity};
use crate::outbound::{seal_ack, WireVersion};
use crate::status::StatusBody;
use crate::v1::V1Payload;

/// Receiver limits. `dedup_retention_secs()` is derived from them so the
/// replay-safety invariant (retention ≥ max TTL + 2·skew) always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundPolicy {
    pub max_skew_secs: u64,
    pub max_ttl_mins: u16,
    pub max_acks_per_message: u32,
    pub frame_budget: usize,
}

impl Default for InboundPolicy {
    fn default() -> Self {
        Self {
            max_skew_secs: 300,
            // One week: enough for a future store-and-forward gateway.
            max_ttl_mins: 7 * 24 * 60,
            max_acks_per_message: 3,
            frame_budget: DEFAULT_FRAME_BUDGET,
        }
    }
}

impl InboundPolicy {
    pub fn dedup_retention_secs(&self) -> u64 {
        u64::from(self.max_ttl_mins) * 60 + 2 * self.max_skew_secs
    }

    pub fn new_dedup_cache(&self, capacity: usize) -> DedupCache {
        DedupCache::new(capacity, self.dedup_retention_secs())
    }
}

pub struct InboundContext<'a> {
    pub sender: &'a PeerIdentity,
    pub me: &'a PeerIdentity,
    pub key: Option<&'a SessionKey>,
    /// Version pinned for this peer, if it is a known peer.
    pub pinned: Option<WireVersion>,
    pub now: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Unsupported,
    Malformed,
    Stale(FreshnessError),
    /// Dedup cache full of unexpired ids; the sender will retry.
    DedupFull,
    /// v1 frame from a peer pinned to v2 (anti-downgrade).
    V1FromV2Peer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// Show and notify. `ack` is a ready-to-send v2 ACK frame, if one is due.
    Text {
        version: WireVersion,
        message_id: Option<MessageId>,
        text: String,
        sent_at: Option<u32>,
        ack: Option<Vec<u8>>,
    },
    /// Check-in, SOS or location share (v2 only). Show, notify (urgently for
    /// SOS / need help) and send `ack` if present.
    Status {
        message_id: MessageId,
        status: StatusBody,
        sent_at: u32,
        ack: Option<Vec<u8>>,
    },
    /// The peer announces/probes its features (v2 only). Send `ack`.
    Caps {
        features: u32,
        ack: Option<Vec<u8>>,
    },
    /// Already seen: do not show again; send `ack` if present.
    Duplicate {
        ack: Option<Vec<u8>>,
    },
    PairRequest {
        version: WireVersion,
        name: String,
    },
    PairAccept {
        version: WireVersion,
        name: String,
    },
    /// Authenticated application ACK: the peer app received `acked`.
    PeerAck {
        acked: MessageId,
    },
    /// No key, or the key did not authenticate. The caller may treat it as a
    /// (new-passphrase) pairing attempt, subject to rate limiting.
    Undecryptable(DecodeError),
    Dropped(DropReason),
}

impl Inbound {
    /// The version of an accepted, authenticated frame, used to auto-upgrade
    /// a peer to v2 (ADR-003 §3 rule a). `None` for rejected frames.
    pub fn authenticated_version(&self) -> Option<WireVersion> {
        match self {
            Self::Text { version, .. }
            | Self::PairRequest { version, .. }
            | Self::PairAccept { version, .. } => Some(*version),
            Self::PeerAck { .. }
            | Self::Duplicate { .. }
            | Self::Status { .. }
            | Self::Caps { .. } => Some(WireVersion::V2),
            Self::Undecryptable(_) | Self::Dropped(_) => None,
        }
    }
}

pub fn process_inbound(
    frame: &[u8],
    ctx: &InboundContext<'_>,
    dedup: &mut DedupCache,
    policy: &InboundPolicy,
) -> Inbound {
    let aad = aad_context(ctx.sender, ctx.me);
    let decoded = match decode_frame(frame, ctx.key, &aad) {
        Ok(d) => d,
        Err(e @ (DecodeError::Locked | DecodeError::AuthFailed)) => {
            return Inbound::Undecryptable(e)
        }
        Err(DecodeError::Unsupported) => return Inbound::Dropped(DropReason::Unsupported),
        Err(DecodeError::Malformed) => return Inbound::Dropped(DropReason::Malformed),
    };

    let env = match decoded {
        DecodedFrame::V1(p) => {
            if ctx.pinned == Some(WireVersion::V2) {
                return Inbound::Dropped(DropReason::V1FromV2Peer);
            }
            return match p {
                V1Payload::Text(text) => Inbound::Text {
                    version: WireVersion::V1,
                    message_id: None,
                    text,
                    sent_at: None,
                    ack: None,
                },
                V1Payload::PairRequest(name) => Inbound::PairRequest {
                    version: WireVersion::V1,
                    name,
                },
                V1Payload::PairAccept(name) => Inbound::PairAccept {
                    version: WireVersion::V1,
                    name,
                },
            };
        }
        DecodedFrame::V2(env) => env,
    };

    if let Err(e) = env.check_freshness(ctx.now, policy.max_skew_secs, policy.max_ttl_mins) {
        return Inbound::Dropped(DropReason::Stale(e));
    }

    let seen = dedup.observe(ctx.sender, env.message_id, ctx.now);
    if seen == Seen::Refused {
        return Inbound::Dropped(DropReason::DedupFull);
    }

    let ack = if env.message_type.requires_ack() && seen.should_ack(policy.max_acks_per_message) {
        ctx.key.and_then(|k| {
            seal_ack(
                env.message_id,
                k,
                ctx.me,
                ctx.sender,
                ctx.now as u32,
                policy.frame_budget,
            )
            .ok()
        })
    } else {
        None
    };

    if seen != Seen::New {
        return Inbound::Duplicate { ack };
    }

    match env.message_type {
        MessageType::Text => Inbound::Text {
            version: WireVersion::V2,
            message_id: Some(env.message_id),
            text: env.text_body().unwrap_or_default().to_string(),
            sent_at: Some(env.created_at),
            ack,
        },
        MessageType::PairRequest => Inbound::PairRequest {
            version: WireVersion::V2,
            name: env.text_body().unwrap_or_default().to_string(),
        },
        MessageType::PairAccept => Inbound::PairAccept {
            version: WireVersion::V2,
            name: env.text_body().unwrap_or_default().to_string(),
        },
        MessageType::Status => match StatusBody::decode(&env.body) {
            Some(status) => Inbound::Status {
                message_id: env.message_id,
                status,
                sent_at: env.created_at,
                ack,
            },
            None => Inbound::Dropped(DropReason::Malformed),
        },
        MessageType::Caps => match crate::status::decode_caps(&env.body) {
            Some(features) => Inbound::Caps { features, ack },
            None => Inbound::Dropped(DropReason::Malformed),
        },
        MessageType::AppAck => match env.acked_id() {
            Some(acked) => Inbound::PeerAck { acked },
            None => Inbound::Dropped(DropReason::Malformed),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_p2p_key;
    use crate::envelope::EnvelopeV2;
    use crate::outbound::{seal, OutPayload};

    const NOW: u64 = 1_763_100_000;

    struct T {
        a: PeerIdentity,
        b: PeerIdentity,
        key: SessionKey,
        dedup: DedupCache,
        policy: InboundPolicy,
    }

    fn setup() -> T {
        let policy = InboundPolicy::default();
        T {
            a: PeerIdentity::meshtastic_node(1),
            b: PeerIdentity::meshtastic_node(2),
            key: derive_p2p_key("A", "B", "pw").unwrap(),
            dedup: policy.new_dedup_cache(64),
            policy,
        }
    }

    impl T {
        fn send(&self, v: WireVersion, p: OutPayload<'_>) -> Vec<u8> {
            seal(v, p, &self.key, &self.a, &self.b, NOW as u32, 228)
                .unwrap()
                .frame
        }

        fn recv(&mut self, frame: &[u8], pinned: Option<WireVersion>) -> Inbound {
            let ctx = InboundContext {
                sender: &self.a,
                me: &self.b,
                key: Some(&self.key),
                pinned,
                now: NOW,
            };
            process_inbound(frame, &ctx, &mut self.dedup, &self.policy)
        }
    }

    #[test]
    fn v2_text_is_shown_once_with_stable_id_and_ack() {
        let mut t = setup();
        let f = t.send(WireVersion::V2, OutPayload::Text("zdravo"));
        let Inbound::Text {
            version,
            message_id,
            text,
            ack,
            ..
        } = t.recv(&f, Some(WireVersion::V1))
        else {
            panic!("expected text");
        };
        assert_eq!(version, WireVersion::V2);
        assert_eq!(text, "zdravo");
        let id = message_id.unwrap();

        // The ACK authenticates in the reverse direction and names the id.
        let back = aad_context(&t.b, &t.a);
        let ack_env = EnvelopeV2::open(&ack.unwrap(), &t.key, &back).unwrap();
        assert_eq!(ack_env.acked_id(), Some(id));

        let Inbound::Duplicate { ack } = t.recv(&f, Some(WireVersion::V2)) else {
            panic!("expected duplicate");
        };
        assert!(ack.is_some(), "duplicates are re-ACKed within budget");
    }

    #[test]
    fn ack_budget_is_enforced() {
        let mut t = setup();
        let f = t.send(WireVersion::V2, OutPayload::Text("x"));
        let mut acks = 0;
        for _ in 0..10 {
            match t.recv(&f, None) {
                Inbound::Text { ack: Some(_), .. } | Inbound::Duplicate { ack: Some(_) } => {
                    acks += 1
                }
                _ => {}
            }
        }
        assert_eq!(acks, t.policy.max_acks_per_message);
    }

    #[test]
    fn v1_is_accepted_until_pinned_to_v2() {
        let mut t = setup();
        let f = t.send(WireVersion::V1, OutPayload::Text("old"));
        assert!(matches!(
            t.recv(&f, Some(WireVersion::V1)),
            Inbound::Text {
                version: WireVersion::V1,
                message_id: None,
                ack: None,
                ..
            }
        ));
        assert_eq!(
            t.recv(&f, Some(WireVersion::V2)),
            Inbound::Dropped(DropReason::V1FromV2Peer)
        );
    }

    #[test]
    fn peer_ack_is_reported() {
        let mut t = setup();
        let id = MessageId::random();
        let f = seal_ack(id, &t.key, &t.a, &t.b, NOW as u32, 228).unwrap();
        assert_eq!(t.recv(&f, None), Inbound::PeerAck { acked: id });
        assert_eq!(
            t.recv(&f, None),
            Inbound::Duplicate { ack: None },
            "ACKs are never ACKed"
        );
    }

    #[test]
    fn pairing_frames_carry_version() {
        let mut t = setup();
        let f = t.send(WireVersion::V2, OutPayload::PairAccept("Bob-T1000"));
        assert_eq!(
            t.recv(&f, None),
            Inbound::PairAccept {
                version: WireVersion::V2,
                name: "Bob-T1000".into()
            }
        );
        let f = t.send(WireVersion::V1, OutPayload::PairRequest("Alice"));
        assert_eq!(
            t.recv(&f, None),
            Inbound::PairRequest {
                version: WireVersion::V1,
                name: "Alice".into()
            }
        );
    }

    #[test]
    fn stale_and_future_frames_are_dropped() {
        let mut t = setup();
        let old = seal(
            WireVersion::V2,
            OutPayload::Text("old"),
            &t.key,
            &t.a,
            &t.b,
            (NOW - 2 * 3600) as u32,
            228,
        )
        .unwrap()
        .frame;
        assert_eq!(
            t.recv(&old, None),
            Inbound::Dropped(DropReason::Stale(FreshnessError::Expired))
        );
        let future = seal(
            WireVersion::V2,
            OutPayload::Text("future"),
            &t.key,
            &t.a,
            &t.b,
            (NOW + 3600) as u32,
            228,
        )
        .unwrap()
        .frame;
        assert_eq!(
            t.recv(&future, None),
            Inbound::Dropped(DropReason::Stale(FreshnessError::FromFuture))
        );
    }

    #[test]
    fn locked_and_wrong_key_are_undecryptable() {
        let mut t = setup();
        let f = t.send(WireVersion::V2, OutPayload::Text("x"));
        let ctx = InboundContext {
            sender: &t.a,
            me: &t.b,
            key: None,
            pinned: None,
            now: NOW,
        };
        assert_eq!(
            process_inbound(&f, &ctx, &mut t.dedup, &t.policy),
            Inbound::Undecryptable(DecodeError::Locked)
        );
        let other = derive_p2p_key("A", "B", "other").unwrap();
        let ctx = InboundContext {
            key: Some(&other),
            ..ctx
        };
        assert_eq!(
            process_inbound(&f, &ctx, &mut t.dedup, &t.policy),
            Inbound::Undecryptable(DecodeError::AuthFailed)
        );
    }

    #[test]
    fn authenticated_version_drives_upgrade() {
        let mut t = setup();
        let f = t.send(WireVersion::V2, OutPayload::Text("x"));
        assert_eq!(
            t.recv(&f, Some(WireVersion::V1)).authenticated_version(),
            Some(WireVersion::V2)
        );
        let f = t.send(WireVersion::V1, OutPayload::Text("x"));
        assert_eq!(
            t.recv(&f, None).authenticated_version(),
            Some(WireVersion::V1)
        );
        assert_eq!(
            Inbound::Undecryptable(DecodeError::Locked).authenticated_version(),
            None
        );
    }

    #[test]
    fn retention_covers_max_ttl() {
        let p = InboundPolicy::default();
        assert_eq!(
            p.dedup_retention_secs(),
            u64::from(p.max_ttl_mins) * 60 + 2 * p.max_skew_secs
        );
    }
}
