//! Wire protocol v2 envelope (see `docs/adr/003-wire-protocol-v2.md`).
//!
//! ```text
//! frame = "MG2" (3) | nonce (12) | AES-256-GCM( inner ) | tag (16)
//! inner = version (1) = 0x02
//!       | type (1)
//!       | message_id (16)
//!       | created_at (u32 BE, unix seconds)
//!       | ttl_mins (u16 BE, minutes; max ~45 days)
//!       | body (rest; length is implied and authenticated)
//! aad   = "MG2" | context
//! ```
//!
//! `context` is supplied by the caller and binds the frame to its direction
//! (see [`aad_context`]). Because v1 keys are symmetric between two peers,
//! binding `(sender, recipient)` stops a frame from being reflected back to
//! its author as if it came from the peer.
//!
//! Fixed overhead is [`OVERHEAD`] = 55 bytes.

use thiserror::Error;
use zeroize::Zeroize;

use crate::crypto::{SessionKey, NONCE_SIZE, TAG_SIZE};
use crate::ids::{MessageId, PeerIdentity, MESSAGE_ID_LEN};

/// Outer, unencrypted discriminator. Also bound into the AAD.
pub const MAGIC: &[u8; 3] = b"MG2";
/// Inner protocol version byte.
pub const VERSION: u8 = 2;

/// Bytes of the encrypted inner header before the body.
pub const INNER_HEADER_LEN: usize = 1 + 1 + MESSAGE_ID_LEN + 4 + 2;
/// Total fixed bytes added around the body.
pub const OVERHEAD: usize = MAGIC.len() + NONCE_SIZE + TAG_SIZE + INNER_HEADER_LEN;

/// Conservative default for the largest frame we hand to Meshtastic as a
/// `PortNum::PrivateApp` payload.
///
/// The protobufs shipped with `meshtastic` 0.1.8 define
/// `Constants::DATA_PAYLOAD_LEN = 233`, but that figure excludes the
/// `Data` protobuf's own fields (e.g. `bitfield`, which firmware may set).
/// 228 matches the limit v1 already assumed. The real figure for current
/// T1000-E firmware must be measured in the P3 hardware test before raising
/// it. Callers pass the budget explicitly so a transport with a different MTU
/// can use its own.
pub const DEFAULT_FRAME_BUDGET: usize = 228;

/// Largest body that fits in `budget` bytes.
pub const fn max_body_len(budget: usize) -> usize {
    budget.saturating_sub(OVERHEAD)
}

/// v2 message types. Values 0x01–0x03 deliberately mirror v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageType {
    Text = 0x01,
    PairRequest = 0x02,
    PairAccept = 0x03,
    /// Check-in, SOS or location share; body is a [`crate::status::StatusBody`].
    Status = 0x04,
    /// Authenticated application ACK. Body = acknowledged [`MessageId`].
    AppAck = 0x10,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::Text),
            0x02 => Some(Self::PairRequest),
            0x03 => Some(Self::PairAccept),
            0x04 => Some(Self::Status),
            0x10 => Some(Self::AppAck),
            _ => None,
        }
    }

    /// Whether the receiver should answer with an [`MessageType::AppAck`].
    ///
    /// ACKs are never acknowledged, which rules out ACK-of-ACK loops.
    pub fn requires_ack(self) -> bool {
        matches!(self, Self::Text | Self::Status)
    }
}

/// Decrypted v2 envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeV2 {
    pub message_type: MessageType,
    pub message_id: MessageId,
    /// Sender clock, unix seconds.
    pub created_at: u32,
    /// Lifetime after `created_at`, in **minutes**. Minutes rather than
    /// seconds so a store-and-forward gateway (RFC P4) can hold a message
    /// for days; `u16` minutes covers about 45 days.
    pub ttl_mins: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SealError {
    #[error("frame of {len} bytes exceeds budget of {max} bytes")]
    TooLarge { len: usize, max: usize },
    #[error("body is invalid for its message type")]
    InvalidBody,
    #[error("encryption failed")]
    Crypto,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// Missing `MG2` marker or too short to be v2.
    #[error("not a v2 frame")]
    NotV2,
    /// Wrong key, wrong context, or tampered bytes.
    #[error("authentication failed")]
    AuthFailed,
    /// Authenticated, but version or type is unknown to this build.
    #[error("unsupported version or message type")]
    Unsupported,
    /// Authenticated, but the inner structure is invalid.
    #[error("malformed envelope")]
    Malformed,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessError {
    #[error("message expired")]
    Expired,
    #[error("message timestamp is too far in the future")]
    FromFuture,
    /// TTL longer than the receiver accepts. Receivers must cap TTL at or
    /// below their dedup retention, or an old frame could be replayed after
    /// its id was forgotten.
    #[error("message TTL exceeds the accepted maximum")]
    TtlTooLong,
}

/// Build the AAD context binding a frame to `sender -> recipient`.
///
/// Length-prefixed so that distinct pairs can never produce equal bytes.
pub fn aad_context(sender: &PeerIdentity, recipient: &PeerIdentity) -> Vec<u8> {
    let (s, r) = (sender.as_bytes(), recipient.as_bytes());
    let mut v = Vec::with_capacity(4 + s.len() + r.len());
    v.extend_from_slice(&(s.len() as u16).to_be_bytes());
    v.extend_from_slice(s);
    v.extend_from_slice(&(r.len() as u16).to_be_bytes());
    v.extend_from_slice(r);
    v
}

/// Cheap pre-check: does this look like a v2 frame?
pub fn looks_like_v2(frame: &[u8]) -> bool {
    frame.len() >= OVERHEAD && frame.starts_with(MAGIC)
}

fn full_aad(context: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(MAGIC.len() + context.len());
    aad.extend_from_slice(MAGIC);
    aad.extend_from_slice(context);
    aad
}

fn body_is_valid(ty: MessageType, body: &[u8]) -> bool {
    match ty {
        MessageType::Text | MessageType::PairRequest | MessageType::PairAccept => {
            std::str::from_utf8(body).is_ok()
        }
        MessageType::Status => crate::status::StatusBody::decode(body).is_some(),
        MessageType::AppAck => body.len() == MESSAGE_ID_LEN,
    }
}

impl EnvelopeV2 {
    /// New text message with a fresh random id.
    pub fn text(text: &str, now: u32, ttl_mins: u16) -> Self {
        Self {
            message_type: MessageType::Text,
            message_id: MessageId::random(),
            created_at: now,
            ttl_mins,
            body: text.as_bytes().to_vec(),
        }
    }

    /// ACK for `acked`. Gets its own id so duplicate ACKs can be dropped too.
    pub fn app_ack(acked: MessageId, now: u32, ttl_mins: u16) -> Self {
        Self {
            message_type: MessageType::AppAck,
            message_id: MessageId::random(),
            created_at: now,
            ttl_mins,
            body: acked.as_bytes().to_vec(),
        }
    }

    /// Body as text for text-like types.
    pub fn text_body(&self) -> Option<&str> {
        match self.message_type {
            MessageType::AppAck | MessageType::Status => None,
            _ => std::str::from_utf8(&self.body).ok(),
        }
    }

    /// The acknowledged id, for [`MessageType::AppAck`].
    pub fn acked_id(&self) -> Option<MessageId> {
        match self.message_type {
            MessageType::AppAck => MessageId::from_slice(&self.body),
            _ => None,
        }
    }

    pub fn expires_at(&self) -> u64 {
        u64::from(self.created_at) + u64::from(self.ttl_mins) * 60
    }

    /// Reject over-long TTLs, expired messages and implausibly
    /// future-dated ones.
    pub fn check_freshness(
        &self,
        now: u64,
        max_skew_secs: u64,
        max_ttl_mins: u16,
    ) -> Result<(), FreshnessError> {
        if self.ttl_mins > max_ttl_mins {
            return Err(FreshnessError::TtlTooLong);
        }
        if u64::from(self.created_at) > now.saturating_add(max_skew_secs) {
            return Err(FreshnessError::FromFuture);
        }
        if now > self.expires_at().saturating_add(max_skew_secs) {
            return Err(FreshnessError::Expired);
        }
        Ok(())
    }

    fn inner_bytes(&self) -> Vec<u8> {
        let mut inner = Vec::with_capacity(INNER_HEADER_LEN + self.body.len());
        inner.push(VERSION);
        inner.push(self.message_type as u8);
        inner.extend_from_slice(self.message_id.as_bytes());
        inner.extend_from_slice(&self.created_at.to_be_bytes());
        inner.extend_from_slice(&self.ttl_mins.to_be_bytes());
        inner.extend_from_slice(&self.body);
        inner
    }

    fn check_seal(&self, budget: usize) -> Result<(), SealError> {
        if !body_is_valid(self.message_type, &self.body) {
            return Err(SealError::InvalidBody);
        }
        let len = OVERHEAD + self.body.len();
        if len > budget {
            return Err(SealError::TooLarge { len, max: budget });
        }
        Ok(())
    }

    /// Encrypt into a wire frame with a fresh random nonce.
    pub fn seal(
        &self,
        key: &SessionKey,
        context: &[u8],
        budget: usize,
    ) -> Result<Vec<u8>, SealError> {
        self.check_seal(budget)?;
        let mut inner = self.inner_bytes();
        let sealed = key.encrypt_with_aad(&inner, &full_aad(context));
        inner.zeroize();
        Self::frame(sealed.map_err(|_| SealError::Crypto)?)
    }

    #[cfg(test)]
    pub(crate) fn seal_with_nonce(
        &self,
        key: &SessionKey,
        context: &[u8],
        budget: usize,
        nonce: &[u8; NONCE_SIZE],
    ) -> Result<Vec<u8>, SealError> {
        self.check_seal(budget)?;
        let mut inner = self.inner_bytes();
        let sealed = key.encrypt_with_aad_and_nonce(&inner, &full_aad(context), nonce);
        inner.zeroize();
        Self::frame(sealed.map_err(|_| SealError::Crypto)?)
    }

    fn frame(sealed: Vec<u8>) -> Result<Vec<u8>, SealError> {
        let mut frame = Vec::with_capacity(MAGIC.len() + sealed.len());
        frame.extend_from_slice(MAGIC);
        frame.extend_from_slice(&sealed);
        Ok(frame)
    }

    /// Authenticate, decrypt and parse a v2 frame.
    pub fn open(frame: &[u8], key: &SessionKey, context: &[u8]) -> Result<Self, OpenError> {
        if !looks_like_v2(frame) {
            return Err(OpenError::NotV2);
        }
        let mut inner = key
            .decrypt_with_aad(&frame[MAGIC.len()..], &full_aad(context))
            .map_err(|_| OpenError::AuthFailed)?;
        let parsed = Self::parse_inner(&inner);
        inner.zeroize();
        parsed
    }

    fn parse_inner(inner: &[u8]) -> Result<Self, OpenError> {
        if inner.len() < INNER_HEADER_LEN {
            return Err(OpenError::Malformed);
        }
        if inner[0] != VERSION {
            return Err(OpenError::Unsupported);
        }
        let message_type = MessageType::from_u8(inner[1]).ok_or(OpenError::Unsupported)?;
        let message_id =
            MessageId::from_slice(&inner[2..2 + MESSAGE_ID_LEN]).ok_or(OpenError::Malformed)?;
        let t = 2 + MESSAGE_ID_LEN;
        let created_at = u32::from_be_bytes([inner[t], inner[t + 1], inner[t + 2], inner[t + 3]]);
        let ttl_mins = u16::from_be_bytes([inner[t + 4], inner[t + 5]]);
        let body = inner[INNER_HEADER_LEN..].to_vec();
        if !body_is_valid(message_type, &body) {
            return Err(OpenError::Malformed);
        }
        Ok(Self {
            message_type,
            message_id,
            created_at,
            ttl_mins,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_p2p_key;

    fn key() -> SessionKey {
        derive_p2p_key("Alice", "Bob", "secret").unwrap()
    }

    fn ctx() -> Vec<u8> {
        aad_context(
            &PeerIdentity::meshtastic_node(1),
            &PeerIdentity::meshtastic_node(2),
        )
    }

    #[test]
    fn overhead_is_55_bytes() {
        assert_eq!(OVERHEAD, 55);
        assert_eq!(max_body_len(DEFAULT_FRAME_BUDGET), 173);
    }

    #[test]
    fn roundtrip_preserves_message_id() {
        let env = EnvelopeV2::text("Zdravo 👋", 1_700_000_000, 3600);
        let frame = env.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET).unwrap();
        assert!(frame.starts_with(MAGIC));
        assert_eq!(frame.len(), OVERHEAD + env.body.len());
        let back = EnvelopeV2::open(&frame, &key(), &ctx()).unwrap();
        assert_eq!(back, env);
        assert_eq!(back.text_body(), Some("Zdravo 👋"));
    }

    #[test]
    fn retry_reencrypts_with_same_id_and_fresh_nonce() {
        let env = EnvelopeV2::text("retry me", 1, 60);
        let a = env.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET).unwrap();
        let b = env.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET).unwrap();
        assert_ne!(a[3..15], b[3..15], "nonce must be fresh per encryption");
        assert_eq!(
            EnvelopeV2::open(&a, &key(), &ctx()).unwrap().message_id,
            EnvelopeV2::open(&b, &key(), &ctx()).unwrap().message_id
        );
    }

    #[test]
    fn budget_is_enforced_exactly() {
        let max = max_body_len(DEFAULT_FRAME_BUDGET);
        let fits = EnvelopeV2::text(&"a".repeat(max), 0, 60);
        assert_eq!(
            fits.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET)
                .unwrap()
                .len(),
            DEFAULT_FRAME_BUDGET
        );
        let over = EnvelopeV2::text(&"a".repeat(max + 1), 0, 60);
        assert_eq!(
            over.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET),
            Err(SealError::TooLarge {
                len: DEFAULT_FRAME_BUDGET + 1,
                max: DEFAULT_FRAME_BUDGET
            })
        );
    }

    #[test]
    fn multibyte_budget_counts_bytes_not_chars() {
        // "ć" is 2 bytes in UTF-8: 87 chars = 174 bytes > 173.
        let env = EnvelopeV2::text(&"ć".repeat(87), 0, 60);
        assert!(matches!(
            env.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET),
            Err(SealError::TooLarge { .. })
        ));
    }

    #[test]
    fn wrong_key_and_wrong_context_fail_auth() {
        let frame = EnvelopeV2::text("x", 0, 60)
            .seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET)
            .unwrap();
        let other = derive_p2p_key("Alice", "Bob", "other").unwrap();
        assert_eq!(
            EnvelopeV2::open(&frame, &other, &ctx()),
            Err(OpenError::AuthFailed)
        );
        let reflected = aad_context(
            &PeerIdentity::meshtastic_node(2),
            &PeerIdentity::meshtastic_node(1),
        );
        assert_eq!(
            EnvelopeV2::open(&frame, &key(), &reflected),
            Err(OpenError::AuthFailed)
        );
    }

    #[test]
    fn every_single_bit_flip_is_rejected() {
        let frame = EnvelopeV2::text("tamper", 0, 60)
            .seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET)
            .unwrap();
        for i in 0..frame.len() {
            let mut t = frame.clone();
            t[i] ^= 0x01;
            assert!(
                EnvelopeV2::open(&t, &key(), &ctx()).is_err(),
                "flip at byte {i} accepted"
            );
        }
    }

    #[test]
    fn truncation_is_rejected_without_panic() {
        let frame = EnvelopeV2::text("truncate", 0, 60)
            .seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET)
            .unwrap();
        for n in 0..frame.len() {
            assert!(EnvelopeV2::open(&frame[..n], &key(), &ctx()).is_err());
        }
    }

    #[test]
    fn invalid_bodies_are_refused_on_seal() {
        let mut env = EnvelopeV2::app_ack(MessageId::random(), 0, 60);
        env.body.pop();
        assert_eq!(
            env.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET),
            Err(SealError::InvalidBody)
        );
        let mut env = EnvelopeV2::text("", 0, 60);
        env.body = vec![0xff];
        assert_eq!(
            env.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET),
            Err(SealError::InvalidBody)
        );
    }

    #[test]
    fn authenticated_but_unknown_version_or_type_is_unsupported() {
        let k = key();
        let aad = full_aad(&ctx());
        let mut inner = EnvelopeV2::text("x", 0, 60).inner_bytes();
        inner[0] = 3;
        let frame = EnvelopeV2::frame(k.encrypt_with_aad(&inner, &aad).unwrap()).unwrap();
        assert_eq!(
            EnvelopeV2::open(&frame, &k, &ctx()),
            Err(OpenError::Unsupported)
        );
        inner[0] = VERSION;
        inner[1] = 0x7e;
        let frame = EnvelopeV2::frame(k.encrypt_with_aad(&inner, &aad).unwrap()).unwrap();
        assert_eq!(
            EnvelopeV2::open(&frame, &k, &ctx()),
            Err(OpenError::Unsupported)
        );
    }

    #[test]
    fn authenticated_invalid_body_is_malformed() {
        let k = key();
        let aad = full_aad(&ctx());
        // AppAck whose body is not a 16-byte id.
        let mut inner = EnvelopeV2::app_ack(MessageId::random(), 0, 60).inner_bytes();
        inner.truncate(INNER_HEADER_LEN + 3);
        let frame = EnvelopeV2::frame(k.encrypt_with_aad(&inner, &aad).unwrap()).unwrap();
        assert_eq!(
            EnvelopeV2::open(&frame, &k, &ctx()),
            Err(OpenError::Malformed)
        );
        // Text whose body is not UTF-8.
        let mut inner = EnvelopeV2::text("", 0, 60).inner_bytes();
        inner.push(0xff);
        let frame = EnvelopeV2::frame(k.encrypt_with_aad(&inner, &aad).unwrap()).unwrap();
        assert_eq!(
            EnvelopeV2::open(&frame, &k, &ctx()),
            Err(OpenError::Malformed)
        );
    }

    #[test]
    fn too_short_frames_are_not_v2() {
        let k = key();
        let frame = EnvelopeV2::frame(
            k.encrypt_with_aad(&[VERSION, 1], &full_aad(&ctx()))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(EnvelopeV2::open(&frame, &k, &ctx()), Err(OpenError::NotV2));
    }

    #[test]
    fn ack_carries_acked_id_and_is_never_acked() {
        let target = MessageId::from_bytes([9; 16]);
        let ack = EnvelopeV2::app_ack(target, 0, 60);
        assert_eq!(ack.acked_id(), Some(target));
        assert_eq!(ack.text_body(), None);
        assert!(!MessageType::AppAck.requires_ack());
        assert!(MessageType::Text.requires_ack());
    }

    #[test]
    fn freshness_window() {
        let env = EnvelopeV2::text("t", 1000, 2); // expires at 1120
        assert_eq!(env.expires_at(), 1120);
        assert_eq!(env.check_freshness(1000, 30, 60), Ok(()));
        assert_eq!(env.check_freshness(1150, 30, 60), Ok(()));
        assert_eq!(
            env.check_freshness(1151, 30, 60),
            Err(FreshnessError::Expired)
        );
        assert_eq!(
            env.check_freshness(969, 30, 60),
            Err(FreshnessError::FromFuture)
        );
        assert_eq!(env.check_freshness(970, 30, 60), Ok(()));
        assert_eq!(env.check_freshness(1000, 30, 2), Ok(()));
        assert_eq!(
            env.check_freshness(1000, 30, 1),
            Err(FreshnessError::TtlTooLong)
        );
    }

    /// Known-answer vector, cross-checked by an independent Python
    /// implementation in `tests/vectors/gen_vectors.py`.
    #[test]
    fn known_answer_vector_layout() {
        let env = EnvelopeV2 {
            message_type: MessageType::Text,
            message_id: MessageId::from_bytes(*b"0123456789abcdef"),
            created_at: 1_763_100_000,
            ttl_mins: 60,
            body: "Zdravo 👋".as_bytes().to_vec(),
        };
        let nonce = [0x11u8; NONCE_SIZE];
        let frame = env
            .seal_with_nonce(&key(), &ctx(), DEFAULT_FRAME_BUDGET, &nonce)
            .unwrap();
        assert_eq!(
            hex::encode(&frame),
            crate::test_vectors::V2_TEXT_FRAME_HEX,
            "v2 layout changed — this breaks the wire protocol"
        );
    }
}
