# MeshGuard — Secure P2P Mesh Messenger

## Project Overview
Privacy-first encrypted P2P messenger for Meshtastic devices. Connects via BLE, discovers mesh nodes, and provides encrypted chat using passphrase-derived keys. No key exchange over the air.

## Architecture
- **Tauri 2.0 Mobile** — Rust backend + web frontend, targeting Android/desktop
- **Rust core** (`src-tauri/src/`) — BLE, crypto, Meshtastic protocol, mesh radio
- **Web UI** (`ui/`) — vanilla JS + CSS, dark theme, mobile-first
- **meshtastic crate** — official Rust library for Meshtastic protobuf + StreamApi

## Key Design Decisions
- **BLE scanning + discovery** — scan for nearby Meshtastic devices, filter by service UUID
- **PIN-based BLE pairing** — handles Bluetooth pairing PIN (default 123456)
- **Mesh node discovery** — reads NodeDB from connected device, shows all mesh nodes
- **Passphrase-based chat** — both peers enter same passphrase to derive identical AES-256 keys
- **Deterministic key derivation** — HKDF-SHA256 from sorted device names + passphrase
- **Single-layer app encryption** — AES-256-GCM on compact binary payload (+ Meshtastic channel PSK on LoRa layer)
- **Compact binary wire format** — `[1 byte type][UTF-8 payload]`, encrypted once; fits in ~228-byte LoRa limit
- **Passphrase never stored** — cleared from memory after key derivation
- **Platform-specific BLE** — btleplug+bluer on Linux, btleplug on macOS, native Kotlin plugin on Android

## Build Commands
```bash
cd ui && npm install && cd ..
cargo tauri dev # desktop dev
cargo tauri build # desktop release
cargo tauri android init && bash scripts/patch-android.sh && cargo tauri android build # Android
```

## Project Structure
```
meshguard/
├── src-tauri/
│   ├── src/
│   │   ├── lib.rs             # Tauri app entry
│   │   ├── main.rs            # Binary entry
│   │   ├── mesh_radio.rs      # BLE scan, connect, polling stream, message I/O
│   │   ├── ble_plugin.rs      # Native Android BLE plugin (Kotlin bridge)
│   │   ├── crypto.rs          # AES-256-GCM + HKDF-SHA256 key derivation
│   │   ├── protocol.rs        # Compact binary wire format for mesh messages
│   │   ├── commands.rs        # Tauri IPC commands (scan, connect, chat, pair)
│   │   ├── device_config.rs   # Saved device/peer configuration
│   │   ├── state.rs           # Shared app state (nodes, keys, radio)
│   │   └── error.rs           # Error types
│   ├── Cargo.toml
│   └── tauri.conf.json
├── ui/
│   ├── src/
│   │   ├── main.js            # UI: Connect → Mesh → Passphrase → Chat
│   │   └── styles/main.css    # Dark theme
│   ├── index.html
│   └── package.json
├── scripts/
│   └── patch-android.sh       # Android BLE permissions + native Kotlin plugin
└── .github/workflows/
    ├── ci.yml                 # Check + test + clippy on push/PR
    └── release.yml            # Build all platforms on tag push
```

## Wire Protocol
Messages use compact binary format to fit Meshtastic's LoRa payload limit (~228 bytes):
- Type 0x01 (Text): `[0x01][UTF-8 text]` → max 160 chars
- Type 0x02 (PairRequest): `[0x02][UTF-8 sender name]`
- Type 0x03 (PairAccept): `[0x03][UTF-8 responder name]`

Entire binary blob is AES-256-GCM encrypted (single layer). Sent as `PortNum::PrivateApp` (256).

## Release Versioning
Date-based: `v2026.03.29`
