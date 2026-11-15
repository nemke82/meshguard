//! MeshGuard engine: the part of the app that does not need Tauri.
//!
//! ADR-001 (option D) moves message processing out of the Tauri runtime so
//! that an Android foreground service can run it while the UI is closed.
//! This crate is that shared core above `meshguard-core` (wire format,
//! crypto) and `meshguard-store` (history): radio packets, nodes, peers,
//! settings and the receive-side state. It has no Tauri, tokio or transport
//! dependency; the app and the service feed it and carry out the I/O.

pub mod config;
pub mod dedup_store;
pub mod diagnostics;
pub mod error;
pub mod link_target;
pub mod nodes;

pub use error::MeshGuardError;
