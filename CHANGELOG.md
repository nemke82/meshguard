# Changelog

Versions are dates (`vYYYY.MM.DD`). Each release on [GitHub Releases](https://github.com/nemke82/meshguard/releases) has the downloads and the full list of pull requests.

## Unreleased

### Added
- **Terms of use.** The app shows the terms once before first use (no threats, harassment, sexual content involving minors, spam, impersonation, false SOS, or anything illegal). Full text at [meshguard.net/terms](https://www.meshguard.net/terms).
- **Block a node.** From the chat, a chat request or the node's details. A blocked node is removed as a contact, and its messages, chat requests, check-ins, SOS alerts and QR pairing requests are ignored. **Settings → Blocked nodes** lists them with **Unblock**.
- **Report a node.** From the chat or the node's details: opens a public report form on GitHub with the node filled in. Nothing is sent until you submit it, and messages are never copied into it.

### Changed
- "Notification Settings" is now **Settings**.

### Fixed
- Messages still waiting to be sent to a contact you block or remove are cancelled (shown as failed) instead of being sent later, for example after pairing with that node again.

## v2026.11.16

Message history, a resend queue, and the first piece of the Android background service.

### Added
- **Message history.** Conversations are saved on the device and kept until you delete them. Message bodies and contact names are encrypted with a key from the device vault (Android Keystore, or the OS keyring on desktop). See [ADR-005](docs/adr/005-message-store.md).
- **Chat list.** Shows the last message, the unread count, and chats with contacts that are no longer paired (read-only). Older messages load with **Load earlier**.
- **Delete conversation.** Removes the conversation, its messages and anything still waiting to be sent.
- **Unsent messages are retried.** A message written while the radio is away is kept in an outbox and sent when the radio is back, with back-off between attempts and at most one transmission every 5 seconds. A ✓✓ from the peer, a final failure, or the message's lifetime ends the retries.
- **Background service test (Android).** Notification Settings → **Background service (test)** starts a foreground service that keeps running when the app is closed and is restarted by Android after the process is killed. After about 15 seconds it posts a test conversation; a reply typed into the notification is saved to the outbox and sent the next time the app is connected to your radio. This is the device test from [ADR-001 §6](docs/adr/001-android-engine-boundary.md); the service does not hold the radio connection yet.
- Desktop now remembers chat keys too, sealed with a vault key kept in the OS keyring (Secret Service on Linux, Keychain on macOS).

### Changed
- Packet handling (receiving, sending, ACKs, chat requests) and the outbox moved into a new `meshguard-engine` crate with no Tauri dependency, so the Android service can use the same code through JNI.
- With history on, an incoming 2.0 message that fails to save is not acknowledged, so the sender sends it again instead of thinking it was delivered.
- A history file that cannot be read with this device's key is set aside (renamed), never deleted.

### Fixed
- **QR pairing no longer closes the app (Android).** The Back button now closes whatever is on top (the QR code, the camera, a sheet or the chat) instead of the whole app. On the first screen it sends the app to the background, so reopening is instant and the radio stays connected.
- **Scanning a code has its own controls.** The camera shows **Cancel** and **Type code** buttons; before, the only way out was Back.
- **"Loading mesh configuration" is much faster over Bluetooth (Android).** The app now asks the radio for large Bluetooth packets and a short connection interval while it loads the node list, then returns to the normal interval to save the radio's battery.
- A Bluetooth connection that drops while it is being set up is reported as failed instead of looking connected.

### Developer
- The Android build runs on pull requests that touch Rust, Kotlin or the Android patch script.
- Kotlin sources live in `android/kotlin/` and are copied in by `scripts/patch-android.sh`.

## v2026.11.15

### Added
- Wi-Fi radios on your network are found automatically (mDNS `_meshtastic._tcp`).
- Connected radios are remembered and listed for one-tap reconnects.

## v2026.11.14

MeshGuard 2.0.

### Added
- **Wire protocol v2:** every message has a unique ID, a timestamp and a lifetime, and is bound to its sender and recipient. See [ADR-003](docs/adr/003-wire-protocol-v2.md).
- **Delivery confirmations:** ✓ queued for transmission, ✓✓ received and decrypted by the peer (2.0 mode).
- **Duplicate and replay protection** that survives restarts (2.0 mode).
- **MeshGuard 2.0 mode** switch for chatting with older versions. A chat paired in 2.0 mode is never silently downgraded.
- **Offline QR pairing** with a random 128-bit key. See [ADR-004](docs/adr/004-qr-pairing.md).
- **Family check-in** ("I'm OK", "On my way", "I need help") and **SOS with location**, with a full-screen alert on the receiving side.
- **Mesh diagnostics:** signal history and path tests per node.
- Chat keys are remembered on Android and the app reconnects to the last radio automatically. See [ADR-002](docs/adr/002-key-persistence.md).
- Node presence and signal on the mesh list.

### Changed
- Message length is counted in bytes, with a counter of bytes left.
- ACKs and chat-request notifications are rate limited.
- Message contents are no longer written to logs.
- Automatic reconnects never start Bluetooth pairing.
- Android releases are signed with a permanent key and also built as an App Bundle for Google Play.

### Fixed
- Accepting a chat from a node with an unknown name.

## v2026.03.23 – v2026.03.29
- First releases: Bluetooth scanning and PIN pairing, mesh node discovery, passphrase-based encrypted chat (AES-256-GCM, HKDF-SHA256), compact binary wire format, notifications with sounds, Wi-Fi and USB serial connections, Android, Linux and macOS builds.
- Fixed the Bluetooth pairing handshake with Meshtastic radios and the Android notification build.
