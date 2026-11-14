//! Sealing outgoing frames in the wire version negotiated with a peer.
//!
//! Both versions are checked against the frame budget **in bytes**. v1
//! previously had no check at all, so a long multibyte message could exceed
//! the LoRa payload and fail silently inside the radio stack.

use crate::crypto::SessionKey;
use crate::envelope::{aad_context, EnvelopeV2, MessageType, SealError};
use crate::ids::{MessageId, PeerIdentity};
use crate::status::{StatusBody, STATUS_OVERHEAD, STATUS_OVERHEAD_WITH_LOCATION};
use crate::v1::V1Payload;

/// Default lifetime of a text message, in minutes. Covers the default retry
/// schedule with margin; a gateway transport may choose a longer TTL later.
pub const DEFAULT_TEXT_TTL_MINS: u16 = 60;
/// Lifetime of an application ACK, in minutes.
pub const ACK_TTL_MINS: u16 = 10;
/// Lifetime of pairing frames, in minutes.
pub const PAIRING_TTL_MINS: u16 = 30;
/// Lifetime of check-ins and location shares, in minutes.
pub const STATUS_TTL_MINS: u16 = 60;
/// SOS and "need help" stay valid longer, so a late relay still delivers.
pub const URGENT_STATUS_TTL_MINS: u16 = 6 * 60;

/// Wire protocol spoken with a peer. Pins both send and minimum receive
/// version (ADR-003 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WireVersion {
    #[default]
    V1,
    V2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutPayload<'a> {
    Text(&'a str),
    PairRequest(&'a str),
    PairAccept(&'a str),
    /// Sent as type `Status` on v2, as its plain-text fallback on v1.
    Status(&'a StatusBody),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub frame: Vec<u8>,
    /// Stable id for v2 frames; v1 has none on the wire.
    pub message_id: Option<MessageId>,
}

/// Largest text body, in UTF-8 bytes, that fits `budget` for `version`.
pub fn max_text_bytes(version: WireVersion, budget: usize) -> usize {
    match version {
        // nonce + type byte + tag
        WireVersion::V1 => budget.saturating_sub(12 + 1 + 16),
        WireVersion::V2 => crate::envelope::max_body_len(budget),
    }
}

/// Largest status text, in UTF-8 bytes, that fits `budget` on v2.
pub fn max_status_text_bytes(with_location: bool, budget: usize) -> usize {
    let fixed = if with_location {
        STATUS_OVERHEAD_WITH_LOCATION
    } else {
        STATUS_OVERHEAD
    };
    crate::envelope::max_body_len(budget).saturating_sub(fixed)
}

/// Encrypt `payload` from `me` to `peer`.
#[allow(clippy::too_many_arguments)]
pub fn seal(
    version: WireVersion,
    payload: OutPayload<'_>,
    key: &SessionKey,
    me: &PeerIdentity,
    peer: &PeerIdentity,
    now: u32,
    budget: usize,
) -> Result<Sealed, SealError> {
    match version {
        WireVersion::V1 => {
            let p = match payload {
                OutPayload::Text(s) => V1Payload::Text(s.to_string()),
                OutPayload::PairRequest(s) => V1Payload::PairRequest(s.to_string()),
                OutPayload::PairAccept(s) => V1Payload::PairAccept(s.to_string()),
                OutPayload::Status(st) => V1Payload::Text(st.fallback_text()),
            };
            let wire = p.to_wire();
            let len = 12 + wire.len() + 16;
            if len > budget {
                return Err(SealError::TooLarge { len, max: budget });
            }
            let frame = key.encrypt(&wire).map_err(|_| SealError::Crypto)?;
            Ok(Sealed {
                frame,
                message_id: None,
            })
        }
        WireVersion::V2 => {
            let (ty, body, ttl) = match payload {
                OutPayload::Text(s) => (
                    MessageType::Text,
                    s.as_bytes().to_vec(),
                    DEFAULT_TEXT_TTL_MINS,
                ),
                OutPayload::PairRequest(s) => (
                    MessageType::PairRequest,
                    s.as_bytes().to_vec(),
                    PAIRING_TTL_MINS,
                ),
                OutPayload::PairAccept(s) => (
                    MessageType::PairAccept,
                    s.as_bytes().to_vec(),
                    PAIRING_TTL_MINS,
                ),
                OutPayload::Status(st) => {
                    let ttl = if st.kind.is_urgent() {
                        URGENT_STATUS_TTL_MINS
                    } else {
                        STATUS_TTL_MINS
                    };
                    let body = st.encode().map_err(|_| SealError::InvalidBody)?;
                    (MessageType::Status, body, ttl)
                }
            };
            let env = EnvelopeV2 {
                message_type: ty,
                message_id: MessageId::random(),
                created_at: now,
                ttl_mins: ttl,
                body,
            };
            let frame = env.seal(key, &aad_context(me, peer), budget)?;
            Ok(Sealed {
                frame,
                message_id: Some(env.message_id),
            })
        }
    }
}

/// Seal a feature probe/announcement (always v2). Returns frame and id; an
/// ACK for the id proves the peer understands the newer message types.
pub fn seal_caps(
    features: u32,
    key: &SessionKey,
    me: &PeerIdentity,
    peer: &PeerIdentity,
    now: u32,
    budget: usize,
) -> Result<Sealed, SealError> {
    let env = EnvelopeV2 {
        message_type: MessageType::Caps,
        message_id: MessageId::random(),
        created_at: now,
        ttl_mins: STATUS_TTL_MINS,
        body: crate::status::encode_caps(features),
    };
    let frame = env.seal(key, &aad_context(me, peer), budget)?;
    Ok(Sealed {
        frame,
        message_id: Some(env.message_id),
    })
}

/// Seal an application ACK for `acked`, from `me` to `peer` (always v2).
pub fn seal_ack(
    acked: MessageId,
    key: &SessionKey,
    me: &PeerIdentity,
    peer: &PeerIdentity,
    now: u32,
    budget: usize,
) -> Result<Vec<u8>, SealError> {
    EnvelopeV2::app_ack(acked, now, ACK_TTL_MINS).seal(key, &aad_context(me, peer), budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_frame, DecodedFrame};
    use crate::crypto::derive_p2p_key;
    use crate::envelope::DEFAULT_FRAME_BUDGET;

    fn ids() -> (PeerIdentity, PeerIdentity) {
        (
            PeerIdentity::meshtastic_node(1),
            PeerIdentity::meshtastic_node(2),
        )
    }

    #[test]
    fn v1_and_v2_decode_on_the_other_side() {
        let k = derive_p2p_key("A", "B", "pw").unwrap();
        let (a, b) = ids();
        let ctx = aad_context(&a, &b);
        let s1 = seal(WireVersion::V1, OutPayload::Text("hi"), &k, &a, &b, 0, 228).unwrap();
        assert!(s1.message_id.is_none());
        assert!(matches!(
            decode_frame(&s1.frame, Some(&k), &ctx),
            Ok(DecodedFrame::V1(V1Payload::Text(_)))
        ));
        let s2 = seal(
            WireVersion::V2,
            OutPayload::PairRequest("A"),
            &k,
            &a,
            &b,
            5,
            228,
        )
        .unwrap();
        let Ok(DecodedFrame::V2(env)) = decode_frame(&s2.frame, Some(&k), &ctx) else {
            panic!("expected v2");
        };
        assert_eq!(Some(env.message_id), s2.message_id);
        assert_eq!(env.message_type, MessageType::PairRequest);
        assert_eq!(env.ttl_mins, PAIRING_TTL_MINS);
    }

    #[test]
    fn budgets_are_in_bytes_for_both_versions() {
        let k = derive_p2p_key("A", "B", "pw").unwrap();
        let (a, b) = ids();
        for v in [WireVersion::V1, WireVersion::V2] {
            let max = max_text_bytes(v, DEFAULT_FRAME_BUDGET);
            let ok = "a".repeat(max);
            let s = seal(
                v,
                OutPayload::Text(&ok),
                &k,
                &a,
                &b,
                0,
                DEFAULT_FRAME_BUDGET,
            )
            .unwrap();
            assert_eq!(s.frame.len(), DEFAULT_FRAME_BUDGET, "{v:?}");
            let over = "a".repeat(max + 1);
            assert!(matches!(
                seal(
                    v,
                    OutPayload::Text(&over),
                    &k,
                    &a,
                    &b,
                    0,
                    DEFAULT_FRAME_BUDGET
                ),
                Err(SealError::TooLarge { .. })
            ));
        }
        assert_eq!(max_text_bytes(WireVersion::V1, 228), 199);
        assert_eq!(max_status_text_bytes(true, 228), 156);
        assert_eq!(max_status_text_bytes(false, 228), 171);
        assert_eq!(max_text_bytes(WireVersion::V2, 228), 173);
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::codec::{decode_frame, DecodedFrame};
    use crate::crypto::derive_p2p_key;
    use crate::envelope::DEFAULT_FRAME_BUDGET;
    use crate::status::{FixSource, GeoFix, StatusKind};

    fn sos() -> StatusBody {
        StatusBody {
            kind: StatusKind::Sos,
            location: Some(GeoFix {
                lat_e7: 448_123_400,
                lon_e7: 204_567_800,
                accuracy_m: Some(12),
                fix_time: 1_763_100_000,
                source: FixSource::RadioGps,
            }),
            text: "SOS".into(),
        }
    }

    #[test]
    fn v2_status_roundtrip_and_urgent_ttl() {
        let k = derive_p2p_key("A", "B", "pw").unwrap();
        let (a, b) = (
            PeerIdentity::meshtastic_node(1),
            PeerIdentity::meshtastic_node(2),
        );
        let st = sos();
        let s = seal(
            WireVersion::V2,
            OutPayload::Status(&st),
            &k,
            &a,
            &b,
            5,
            DEFAULT_FRAME_BUDGET,
        )
        .unwrap();
        let Ok(DecodedFrame::V2(env)) = decode_frame(&s.frame, Some(&k), &aad_context(&a, &b))
        else {
            panic!("expected v2");
        };
        assert_eq!(env.message_type, MessageType::Status);
        assert_eq!(env.ttl_mins, URGENT_STATUS_TTL_MINS);
        assert_eq!(StatusBody::decode(&env.body), Some(st));
    }

    #[test]
    fn v1_peers_get_plain_text() {
        let k = derive_p2p_key("A", "B", "pw").unwrap();
        let (a, b) = (
            PeerIdentity::meshtastic_node(1),
            PeerIdentity::meshtastic_node(2),
        );
        let st = sos();
        let s = seal(
            WireVersion::V1,
            OutPayload::Status(&st),
            &k,
            &a,
            &b,
            5,
            DEFAULT_FRAME_BUDGET,
        )
        .unwrap();
        assert!(s.message_id.is_none());
        let Ok(DecodedFrame::V1(V1Payload::Text(t))) =
            decode_frame(&s.frame, Some(&k), &aad_context(&a, &b))
        else {
            panic!("expected v1 text");
        };
        assert_eq!(t, st.fallback_text());
    }

    #[test]
    fn status_text_limit_is_exact() {
        let k = derive_p2p_key("A", "B", "pw").unwrap();
        let (a, b) = (
            PeerIdentity::meshtastic_node(1),
            PeerIdentity::meshtastic_node(2),
        );
        let mut st = sos();
        st.text = "a".repeat(max_status_text_bytes(true, DEFAULT_FRAME_BUDGET));
        let s = seal(
            WireVersion::V2,
            OutPayload::Status(&st),
            &k,
            &a,
            &b,
            5,
            DEFAULT_FRAME_BUDGET,
        )
        .unwrap();
        assert_eq!(s.frame.len(), DEFAULT_FRAME_BUDGET);
        st.text.push('a');
        assert!(seal(
            WireVersion::V2,
            OutPayload::Status(&st),
            &k,
            &a,
            &b,
            5,
            DEFAULT_FRAME_BUDGET
        )
        .is_err());
    }
}
