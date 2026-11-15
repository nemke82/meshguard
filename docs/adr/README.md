# Architecture Decision Records

| ADR | Title | Status |
|---|---|---|
| [001](001-android-engine-boundary.md) | Android engine boundary — sharing the Rust core with a native service | Accepted; device gate pending |
| [002](002-key-persistence.md) | Remembering chat keys on the device (Android Keystore) | Accepted; app lock (PIN/biometric) is a follow-up |
| [003](003-wire-protocol-v2.md) | Wire protocol v2 — stable message id, direction-bound AEAD, dual-read | Accepted; codec implemented, live rollout in progress |
| [004](004-qr-pairing.md) | Offline QR pairing (random secret, no passphrase) | Accepted |
| [005](005-message-store.md) | Message store: encrypted history, inbox and outbox (SQLite) | Accepted; store crate implemented, app integration next |

The design context is in [`../design/MESHGUARD_2_0_DESIGN_RFC.md`](../design/MESHGUARD_2_0_DESIGN_RFC.md), and the starting point is described in [`../audit/p0-baseline.md`](../audit/p0-baseline.md).
