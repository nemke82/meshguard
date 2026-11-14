//! Session crypto for the Tauri app.
//!
//! The implementation lives in `meshguard-core` so that the Tauri commands and
//! a future native Android service share one crypto stack (see
//! `docs/adr/001-android-engine-boundary.md`). This module re-exports it
//! unchanged; the tests below pin v1 behaviour from the app's point of view.

pub use meshguard_core::crypto::{derive_channel_psk, derive_p2p_key, SessionKey};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p2p_key_derivation_is_symmetric() {
        // Alice's side
        let alice_key = derive_p2p_key("Alice-P1000", "Bob-P1000", "our-secret-phrase").unwrap();

        // Bob's side — same inputs but swapped my/peer
        let bob_key = derive_p2p_key("Bob-P1000", "Alice-P1000", "our-secret-phrase").unwrap();

        // Both must derive identical keys
        let msg = b"Hello from MeshGuard P2P!";
        let encrypted = alice_key.encrypt(msg).unwrap();
        let decrypted = bob_key.decrypt(&encrypted).unwrap();
        assert_eq!(&decrypted, msg);
    }

    #[test]
    fn channel_psk_is_symmetric() {
        let psk_a = derive_channel_psk("Alice-P1000", "Bob-P1000", "our-secret").unwrap();

        let psk_b = derive_channel_psk("Bob-P1000", "Alice-P1000", "our-secret").unwrap();

        assert_eq!(psk_a, psk_b);
    }

    #[test]
    fn different_passphrase_gives_different_key() {
        let key_a = derive_p2p_key("A", "B", "pass1").unwrap();
        let key_b = derive_p2p_key("A", "B", "pass2").unwrap();

        let msg = b"test";
        let encrypted = key_a.encrypt(msg).unwrap();
        assert!(key_b.decrypt(&encrypted).is_err());
    }
}
