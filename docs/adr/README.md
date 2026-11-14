# Architecture Decision Records

| ADR | Title | Status |
|---|---|---|
| [001](001-android-engine-boundary.md) | Android engine boundary — sharing the Rust core with a native service | Accepted; device gate pending |
| 002 | Key persistence and unlock policy (Android Keystore, locked state) | Not started; required before keys are persisted |
| [003](003-wire-protocol-v2.md) | Wire protocol v2 — stable message id, direction-bound AEAD, dual-read | Accepted; codec implemented, live rollout in progress |

The design context is in [`../design/MESHGUARD_2_0_DESIGN_RFC.md`](../design/MESHGUARD_2_0_DESIGN_RFC.md), and the starting point is described in [`../audit/p0-baseline.md`](../audit/p0-baseline.md).
