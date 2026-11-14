//! MeshGuard core — the transport-agnostic message engine.
//!
//! This crate holds the logic that must behave identically no matter which
//! front end (Tauri desktop, Tauri Android UI, native Android service) or
//! which transport (Meshtastic BLE, future LAN gateway) is in use:
//!
//! * [`crypto`] — AES-256-GCM session keys and the v1 passphrase KDF
//!   (moved verbatim from `src-tauri/src/crypto.rs`).
//! * [`v1`] — the legacy `[type][UTF-8]` wire payload, kept for dual-read.
//! * [`envelope`] — the v2 envelope with a stable end-to-end [`MessageId`].
//! * [`codec`] — dual-read of v1 and v2 frames with typed failure reasons.
//! * [`dedup`] — bounded `(sender, message_id)` replay/duplicate cache.
//! * [`delivery`] — truthful delivery states, retry policy and outbound
//!   bookkeeping.
//! * [`transport`] — the opaque-frame transport contract and a mock.
//! * [`outbound`] / [`inbound`] — sealing per negotiated [`outbound::WireVersion`]
//!   and the receive-side decision for one frame (the start of the engine).
//! * [`ratelimit`] — airtime budgets and notification cooldowns.
//!
//! The crate has no Tauri, tokio, BLE or Android dependency on purpose, so it
//! can later be exposed to a native Android service (see
//! `docs/adr/001-android-engine-boundary.md`). Nothing in here logs.

pub mod codec;
pub mod crypto;
pub mod dedup;
pub mod delivery;
pub mod envelope;
pub mod error;
pub mod ids;
pub mod inbound;
pub mod outbound;
pub mod ratelimit;
pub mod transport;
pub mod v1;

pub use error::CoreError;
pub use ids::{ConversationId, MessageId, PeerIdentity, TransportId};

#[cfg(test)]
mod test_vectors {
    include!("../tests/vectors/vectors.rs");
}
