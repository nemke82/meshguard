//! Compatibility fixtures: bytes produced by an independent implementation
//! (`tests/vectors/gen_vectors.py`) must decode with the Rust codec.
//!
//! The v1 vectors pin the format that released MeshGuard builds put on the
//! air; if these fail, existing installations can no longer be read.

mod v {
    include!("vectors/vectors.rs");
}

use meshguard_core::codec::{decode_frame, DecodeError, DecodedFrame};
use meshguard_core::crypto::derive_p2p_key;
use meshguard_core::envelope::{aad_context, EnvelopeV2, MessageType};
use meshguard_core::v1::V1Payload;
use meshguard_core::{MessageId, PeerIdentity};

fn ctx_1_to_2() -> Vec<u8> {
    aad_context(
        &PeerIdentity::meshtastic_node(1),
        &PeerIdentity::meshtastic_node(2),
    )
}

fn decode(hex_frame: &str) -> Result<DecodedFrame, DecodeError> {
    let frame = hex::decode(hex_frame).unwrap();
    // Bob derives the key with the names swapped: KDF must be symmetric.
    let key = derive_p2p_key("Bob", "Alice", "secret").unwrap();
    decode_frame(&frame, Some(&key), &ctx_1_to_2())
}

#[test]
fn v1_text_fixture() {
    assert_eq!(
        decode(v::V1_TEXT_FRAME_HEX),
        Ok(DecodedFrame::V1(V1Payload::Text("Hello mesh".into())))
    );
}

#[test]
fn v1_pair_request_fixture() {
    assert_eq!(
        decode(v::V1_PAIR_REQUEST_FRAME_HEX),
        Ok(DecodedFrame::V1(V1Payload::PairRequest(
            "Alice-Radio".into()
        )))
    );
}

#[test]
fn v1_multibyte_fixture() {
    assert_eq!(
        decode(v::V1_MULTIBYTE_TEXT_FRAME_HEX),
        Ok(DecodedFrame::V1(V1Payload::Text("Zdravo 👋 ćao".into())))
    );
}

#[test]
fn v2_text_fixture() {
    let DecodedFrame::V2(env) = decode(v::V2_TEXT_FRAME_HEX).unwrap() else {
        panic!("expected v2");
    };
    assert_eq!(env.message_type, MessageType::Text);
    assert_eq!(env.message_id, MessageId::from_bytes(*b"0123456789abcdef"));
    assert_eq!(env.created_at, 1_763_100_000);
    assert_eq!(env.ttl_mins, 60);
    assert_eq!(env.text_body(), Some("Zdravo 👋"));
}

#[test]
fn v2_ack_fixture() {
    let DecodedFrame::V2(env) = decode(v::V2_ACK_FRAME_HEX).unwrap() else {
        panic!("expected v2");
    };
    assert_eq!(env.message_type, MessageType::AppAck);
    assert_eq!(
        env.acked_id(),
        Some(MessageId::from_bytes(*b"0123456789abcdef"))
    );
}

#[test]
fn v2_fixture_rejected_in_reflected_direction() {
    let frame = hex::decode(v::V2_TEXT_FRAME_HEX).unwrap();
    let key = derive_p2p_key("Alice", "Bob", "secret").unwrap();
    let reflected = aad_context(
        &PeerIdentity::meshtastic_node(2),
        &PeerIdentity::meshtastic_node(1),
    );
    assert_eq!(
        decode_frame(&frame, Some(&key), &reflected),
        Err(DecodeError::AuthFailed)
    );
    assert!(EnvelopeV2::open(&frame, &key, &reflected).is_err());
}

#[test]
fn fixtures_fail_with_wrong_passphrase() {
    let key = derive_p2p_key("Alice", "Bob", "wrong").unwrap();
    for f in [v::V1_TEXT_FRAME_HEX, v::V2_TEXT_FRAME_HEX] {
        assert_eq!(
            decode_frame(&hex::decode(f).unwrap(), Some(&key), &ctx_1_to_2()),
            Err(DecodeError::AuthFailed)
        );
    }
}
