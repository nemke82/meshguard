//! Self-test for the ADR-001 §6 gate: proves that the engine code runs in
//! the process without the Tauri runtime (the Android service calls it
//! through JNI). Decodes the shared v2 fixture frame
//! (`crates/meshguard-core/tests/vectors`).

use meshguard_core::codec::{decode_frame, DecodedFrame};
use meshguard_core::crypto::derive_p2p_key;
use meshguard_core::envelope::aad_context;
use meshguard_core::PeerIdentity;

/// `V2_TEXT_FRAME_HEX` from the shared vectors: v2 text "Zdravo 👋" from
/// node 1 to node 2, key `derive_p2p_key("Alice", "Bob", "secret")`.
const V2_TEXT_FRAME_HEX: &str = "4d47321111111111111111111111114c8fb4ddc850dc587a7b71bd181377f795dec1bc68813aa177639bd8a83f1efe6aac78a47313a6f6b4ad77d11ab316e5ca168b";

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Decrypt the fixture frame and return its text.
pub fn self_test() -> Result<String, String> {
    let frame = unhex(V2_TEXT_FRAME_HEX).ok_or("bad fixture hex")?;
    let key = derive_p2p_key("Alice", "Bob", "secret").map_err(|e| e.to_string())?;
    let ctx = aad_context(
        &PeerIdentity::meshtastic_node(1),
        &PeerIdentity::meshtastic_node(2),
    );
    match decode_frame(&frame, Some(&key), &ctx) {
        Ok(DecodedFrame::V2(env)) => env
            .text_body()
            .map(str::to_string)
            .ok_or_else(|| "fixture is not a text".into()),
        Ok(_) => Err("fixture decoded as v1".into()),
        Err(e) => Err(format!("{e:?}")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_fixture_decodes() {
        assert_eq!(super::self_test().unwrap(), "Zdravo 👋");
    }
}
