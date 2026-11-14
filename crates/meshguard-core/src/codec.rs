//! Dual-read of v1 and v2 frames.
//!
//! Sending stays on v1 until peers negotiate v2 (ADR-003). Receiving accepts
//! both, so a v2-capable build can talk to an old one.
//!
//! The `MG2` marker is not proof of v2: a v1 frame begins with a random
//! 12-byte nonce, which starts with `MG2` with probability 2^-24. A frame
//! that carries the marker but fails v2 authentication is therefore retried
//! as v1 before being reported as [`DecodeError::AuthFailed`].

use thiserror::Error;

use crate::crypto::SessionKey;
use crate::envelope::{self, EnvelopeV2, OpenError};
use crate::v1::{V1Error, V1Payload};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedFrame {
    V1(V1Payload),
    V2(EnvelopeV2),
}

/// Why a frame could not be read. Never carries plaintext or key material,
/// so it is safe to log and to show in the UI.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// No session key is available (store locked or peer not paired).
    /// The ciphertext can be kept and retried after unlock.
    #[error("locked: no key available")]
    Locked,
    /// Neither v1 nor v2 authentication succeeded with this key.
    #[error("authentication failed")]
    AuthFailed,
    /// Authenticated, but uses a version or type this build does not know.
    #[error("unsupported version or message type")]
    Unsupported,
    /// Authenticated, but structurally invalid, or empty.
    #[error("malformed frame")]
    Malformed,
}

/// Decode a received frame.
///
/// `v2_context` is the AAD context for the expected direction, see
/// [`envelope::aad_context`]. It is ignored for v1, which has no AAD.
pub fn decode_frame(
    frame: &[u8],
    key: Option<&SessionKey>,
    v2_context: &[u8],
) -> Result<DecodedFrame, DecodeError> {
    if frame.is_empty() {
        return Err(DecodeError::Malformed);
    }
    let key = key.ok_or(DecodeError::Locked)?;

    if envelope::looks_like_v2(frame) {
        match EnvelopeV2::open(frame, key, v2_context) {
            Ok(env) => return Ok(DecodedFrame::V2(env)),
            Err(OpenError::Unsupported) => return Err(DecodeError::Unsupported),
            Err(OpenError::Malformed) => return Err(DecodeError::Malformed),
            // Possibly a v1 frame whose nonce happens to start with "MG2".
            Err(OpenError::AuthFailed) | Err(OpenError::NotV2) => {}
        }
    }

    let wire = key.decrypt(frame).map_err(|_| DecodeError::AuthFailed)?;
    match V1Payload::from_wire(&wire) {
        Ok(p) => Ok(DecodedFrame::V1(p)),
        Err(V1Error::UnknownType(_)) => Err(DecodeError::Unsupported),
        Err(V1Error::Empty) | Err(V1Error::InvalidUtf8) => Err(DecodeError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_p2p_key, NONCE_SIZE};
    use crate::envelope::{aad_context, DEFAULT_FRAME_BUDGET, MAGIC};
    use crate::ids::PeerIdentity;

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
    fn reads_v1() {
        let frame = key()
            .encrypt(&V1Payload::Text("legacy".into()).to_wire())
            .unwrap();
        assert_eq!(
            decode_frame(&frame, Some(&key()), &ctx()),
            Ok(DecodedFrame::V1(V1Payload::Text("legacy".into())))
        );
    }

    #[test]
    fn reads_v2() {
        let env = EnvelopeV2::text("modern", 5, 60);
        let frame = env.seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET).unwrap();
        assert_eq!(
            decode_frame(&frame, Some(&key()), &ctx()),
            Ok(DecodedFrame::V2(env))
        );
    }

    #[test]
    fn v1_frame_whose_nonce_starts_with_marker_still_reads_as_v1() {
        let mut nonce = [0x42u8; NONCE_SIZE];
        nonce[..3].copy_from_slice(MAGIC);
        // Long enough to pass looks_like_v2().
        let text = "x".repeat(40);
        let frame = key()
            .encrypt_with_aad_and_nonce(&V1Payload::Text(text.clone()).to_wire(), &[], &nonce)
            .unwrap();
        assert!(envelope::looks_like_v2(&frame));
        assert_eq!(
            decode_frame(&frame, Some(&key()), &ctx()),
            Ok(DecodedFrame::V1(V1Payload::Text(text)))
        );
    }

    #[test]
    fn locked_without_key() {
        let frame = key().encrypt(b"\x01hi").unwrap();
        assert_eq!(decode_frame(&frame, None, &ctx()), Err(DecodeError::Locked));
    }

    #[test]
    fn wrong_key_is_auth_failed_for_both_versions() {
        let other = derive_p2p_key("Alice", "Bob", "nope").unwrap();
        let v1 = key().encrypt(b"\x01hi").unwrap();
        let v2 = EnvelopeV2::text("hi", 0, 60)
            .seal(&key(), &ctx(), DEFAULT_FRAME_BUDGET)
            .unwrap();
        for f in [v1, v2] {
            assert_eq!(
                decode_frame(&f, Some(&other), &ctx()),
                Err(DecodeError::AuthFailed)
            );
        }
    }

    #[test]
    fn v1_errors_map_to_typed_reasons() {
        let unknown = key().encrypt(&[0x7f, b'a']).unwrap();
        assert_eq!(
            decode_frame(&unknown, Some(&key()), &ctx()),
            Err(DecodeError::Unsupported)
        );
        let empty = key().encrypt(&[]).unwrap();
        assert_eq!(
            decode_frame(&empty, Some(&key()), &ctx()),
            Err(DecodeError::Malformed)
        );
        assert_eq!(
            decode_frame(&[], Some(&key()), &ctx()),
            Err(DecodeError::Malformed)
        );
    }

    #[test]
    fn garbage_never_panics() {
        for len in 0..120 {
            let junk: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let _ = decode_frame(&junk, Some(&key()), &ctx());
            let mut marked = MAGIC.to_vec();
            marked.extend_from_slice(&junk);
            let _ = decode_frame(&marked, Some(&key()), &ctx());
        }
    }
}
