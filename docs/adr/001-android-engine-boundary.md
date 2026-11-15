# ADR-001: Android engine boundary — sharing the Rust core with a native service

**Status:** Accepted (2026-11-14). The owner approved option D, a Rust-owned SQLite store and minSdk ≥ 26. The device gate in §6 is still pending and must pass before P2 feature work.
**Date:** 2026-11-14
**Context docs:** `docs/design/MESHGUARD_2_0_DESIGN_RFC.md` §3, `docs/audit/p0-baseline.md` §3

## 1. Problem

MeshGuard 2.0 must receive, decrypt, persist and notify, and send inline replies, **while no Activity or WebView exists**.

Today, every byte from the radio flows through a Rust thread that calls the Kotlin `BlePlugin` through the Tauri `PluginHandle` (audit §3). That plugin is constructed with, and uses, the Activity. Session keys live in the Tauri-managed `AppState`. When the Activity goes away, there is no supported owner for the BLE link, the keys or the processing loop.

The RFC also forbids two independent crypto/protocol implementations (Rust + Kotlin).

## 2. Constraints

- **C1:** One implementation of the crypto, codec, dedup and delivery rules. `crates/meshguard-core` already exists and has no Tauri, tokio or Android dependency.
- **C2:** The Android `Service` must work when the Tauri runtime was never started in this process (for example, a restart via the service, or a reply from a notification after the process was reclaimed).
- **C3:** Desktop behaviour must not change.
- **C4:** Keep the build the way CI builds it today: `cargo tauri android init` → `patch-android.sh` → `cargo tauri android build` with NDK 27.
- **C5:** The UI and the service must observe **one** state (one outbox, one dedup cache, one conversation store) and must not each keep their own copy.

## 3. Options

### A. Keep processing in the Tauri runtime; add a service only to keep the process alive

The service just holds a foreground notification while Rust keeps polling through the `PluginHandle`.

- ✗ Violates C2. Tauri's mobile entry point is driven by the Activity's lifecycle (`WryActivity` → `create`). Nothing guarantees the Tauri app, its managed state or the plugin instance survive or exist without it.
- ✗ BLE stays bound to an Activity context.
- **Rejected.**

### B. Re-implement crypto and protocol in Kotlin inside the service

- ✗ Violates C1, giving two stacks that can drift (KDF, nonce handling, v1/v2 dual-read, dedup).
- ✓ Simplest to build.
- **Rejected.** It is acceptable only as a throw-away spike.

### C. Separate Rust library for the service (UniFFI + JNA), built with `cargo-ndk`

- ✓ Clean typed bindings.
- ✗ A second Rust `.so` in the same process gives a separate copy of all statics. The UI's Tauri code and the service would hold **separate engine state** unless one of them becomes a pure client of the other over IPC, which violates C5 or adds an IPC layer.
- ✗ Adds a second native build pipeline and the JNA `aar` dependency (C4).

### D. Same library, narrow JNI surface, process-wide engine (recommended)

Tauri already builds `libmeshguard_lib.so` (`crate-type` includes `cdylib`). We add an `android_bridge` module to that crate that exports a few `extern "system"` JNI functions (using the `jni` crate) for a Kotlin `CoreBridge` object. Both sides talk to one process-wide engine held in a `OnceLock`, backed by `meshguard-core`:

```
             ┌──────────────── app process (single) ────────────────┐
 Activity ──▶│ Tauri runtime ─ commands.rs ─┐                        │
 (optional)  │                              ├─▶ Engine (OnceLock) ──▶ meshguard-core
 Service  ──▶│ Kotlin CoreBridge ─ JNI ─────┘        │               │
 (FGS)       │   ▲ owns BluetoothGatt                ▼               │
             │   └─ bytes in / actions out      SQLite store         │
             └───────────────────────────────────────────────────────┘
```

- ✓ C1, C3 and C4: one crate, one `.so`, no new build tool. `System.loadLibrary("meshguard_lib")` from the service only loads the code. It does **not** start Tauri.
- ✓ C5: the Tauri commands and the service call the same engine singleton, in the same process.
- ✓ C2: the engine does not depend on `tauri::AppHandle`. The service initialises it with a data directory passed from Kotlin.
- ✗ Hand-written JNI glue. This is mitigated by keeping the surface **byte-oriented and small** (below), with Rust and Kotlin tests run against the shared fixtures in `crates/meshguard-core/tests/vectors/`.

**UniFFI vs hand-written JNI:** at fewer than about 10 byte-array functions, hand-written `jni` is less machinery than UniFFI + JNA. Revisit if the surface grows; UniFFI can generate bindings against the same `.so`.

## 4. Decision

Adopt **Option D**:

1. **Kotlin service owns the radio link.** `MeshGuardConnectionService` (type `connectedDevice`) owns `BluetoothGatt` with the **application** context, subscribes to FROMNUM notifications instead of polling, and runs reconnect with backoff.
2. **The Rust engine is sans-I/O.** It accepts raw `FromRadio` bytes and returns a list of actions: ToRadio bytes to write, notifications to post or update, and state changes. Kotlin performs the I/O; Rust makes the decisions. The desktop build drives the same engine from its tokio stream (btleplug, serial, TCP), so Meshtastic protobuf handling stays in Rust.
3. **The engine has no Tauri dependency.** It lives in `meshguard-core` plus a thin `android_bridge` (JNI) module in `src-tauri`. Tauri commands become clients of the engine instead of owners of `AppState.session_keys`.
4. **Single process.** The service declares no `android:process`, because a separate process would split the singleton.
5. **Kotlin sources move out of the heredocs** in `patch-android.sh` into real files, for example `android/src/main/kotlin/...`. The patch script copies them in idempotently, so they can be linted and unit-tested (audit D8).

### Proposed initial JNI surface

| Kotlin (`CoreBridge`) | Purpose |
|---|---|
| `init(dataDir: String): Int` | Open the store and load the dedup cache. Idempotent. |
| `onFromRadio(bytes: ByteArray): ByteArray` | Feed one FromRadio packet. Returns encoded actions. |
| `enqueueReply(conversationId: String, text: String): ByteArray` | Persist to the outbox *before* any radio write. Returns `{messageId, status}`. |
| `tick(nowSecs: Long): ByteArray` | Run retry, expiry and ACK timers. Returns actions. |
| `lockState(): Int` | `Unlocked`, `Locked` or `NoKey`, for the notification and Reply gating (ADR-002). |
| `version(): String` | Smoke test for the gate. |

Actions are a compact, versioned binary or JSON blob defined in Rust. Plaintext crosses JNI only for notification content and only when the key policy allows it. It is never logged on either side.

## 5. Consequences

- `AppState` gradually turns into a view over the engine. Until P2, the desktop path is unchanged.
- **Store ownership (decided):** a single Rust-owned SQLite store (`rusqlite`, SQLCipher if ADR-002 needs it), so desktop and Android share schema and migrations. No Room on the Kotlin side. The schema and migration plan still get their own ADR before they land.
- Requires `jni` (Android-only, `cfg(target_os = "android")`) in `src-tauri`.
- Needs a check that no Tauri/wry `JNI_OnLoad` or `ndk-context` initialisation is required before our JNI functions run. That check is part of the gate.

## 6. Gate (must pass before P2 feature work)

This is a minimal spike on a real Android device. No NDK is available in this environment, so it is **pending**:

1. Build the APK with a stub `CoreBridge` (`version`, `init`, and `onFromRadio` decoding `V2_TEXT_FRAME_HEX` from the shared vectors).
2. Start `MeshGuardConnectionService` from a user action, then swipe the Activity away.
3. Inject the fixture frame from the service, through a debug intent or a timer. Expected result: it decodes, a notification is posted, and an inline-reply `RemoteInput` is persisted via `enqueueReply`.
4. Repeat after `am kill` (process reclaimed) with the service restarted by the system or by the user.
5. Record the Android version, OEM and any `JNI_OnLoad`/context issues here.

If step 3 fails because the library cannot run without Tauri initialisation, fall back to Option C, with the service as the sole engine owner and the UI as its client.

## 7. Owner decisions (2026-11-14)

1. **Accepted:** on Android the Tauri UI becomes a client of the service-owned engine; it no longer owns BLE.
2. **Accepted:** `minSdk` ≥ 26.
3. **Accepted:** Rust-owned SQLite as the single store (see §5).
4. **Still open:** CompanionDeviceManager. Evaluate it during the P2 gate spike.

## 8. Implementation notes

- **2A:** `crates/meshguard-engine` has no Tauri, tokio or transport dependency (`meshtastic` with `default-features = false`, protobufs only). It holds settings and peers, the dedup file, diagnostics, the error type and the node model. `nodes::observe` updates nodes from a packet as a pure function.
- **2B:** `Engine` owns the shared state: chat keys and their owning radio, peers and settings, replay cache and rate limits, pending chat requests, mesh nodes and the history store. `Engine::on_from_radio(&FromRadio, my_node, now)` returns `Action`s: ACK to send, UI event, notification, own GPS fix, path-test reply, signal sample, peer ACK for the outbox, undecryptable frame (the app checks it against a QR offer, then calls `note_pair_request`), and feature probe. The Tauri app keeps it in a `std::sync::Mutex` that is never held across an `.await`. The listener and the commands are its clients.
  - Messages are handled one at a time under that lock. A new message's id is therefore written to `dedup.json` only after the message is in the history, with no other writer in between, so the earlier "uncommitted ids" bookkeeping is gone.
- **2C (next):** sending and the outbox move into the engine (`enqueue`, `tick`), together with ToRadio encoding for the Kotlin service.
