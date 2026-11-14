//! Sealing outgoing frames in the wire version negotiated with a peer.
//!
//! Both versions are checked against the frame budget **in bytes**. v1
//! previously had no check at all, so a long multibyte message could exceed
//! the LoRa payload and fail silently inside the radio stack.

use crate::crypto::SessionKey;
use crate::envelope::{aad_context, EnvelopeV2, MessageType, SealError};
use crate::ids::{MessageId, PeerIdentity};
use crate::v1::V1Payload;

/// Default lifetime of a text message, in minutes. Covers the default retry
/// schedule with margin; a gateway transport may choose a longer TTL later.
pub const DEFAULT_TEXT_TTL_MINS: u16 = 60;
/// Lifetime of an application ACK, in minutes.
pub const ACK_TTL_MINS: u16 = 10;
/// Lifetime of pairing frames, in minutes.
pub const PAIRING_TTL_MINS: u16 = 30;

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
                OutPayload::Text(s) => (MessageType::Text, s, DEFAULT_TEXT_TTL_MINS),
                OutPayload::PairRequest(s) => (MessageType::PairRequest, s, PAIRING_TTL_MINS),
                OutPayload::PairAccept(s) => (MessageType::PairAccept, s, PAIRING_TTL_MINS),
            };
            let env = EnvelopeV2 {
                message_type: ty,
                message_id: MessageId::random(),
                created_at: now,
                ttl_mins: ttl,
                body: body.as_bytes().to_vec(),
            };
            let frame = env.seal(key, &aad_context(me, peer), budget)?;
            Ok(Sealed {
                frame,
                message_id: Some(env.message_id),
            })
        }
    }
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
        assert_eq!(max_text_bytes(WireVersion::V2, 228), 173);
    }
}
