# MeshGuard — Secure P2P Mesh Messenger

## Project Overview
Privacy-first encrypted P2P messenger for Meshtastic devices. Replaces the official Meshtastic app — configures devices, establishes encrypted peer channels, and provides secure chat. No scanning, no discovery, no key exchange over the air.

## Architecture
- **Tauri 2.0 Mobile** — Rust backend + web frontend, targeting Android/desktop
- **Rust core** (`src-tauri/src/`) — BLE, crypto, device config, Meshtastic protocol
- **Web UI** (`ui/`) — vanilla JS + CSS, dark theme, mobile-first, 3-screen flow

## Key Design Decisions
- **No scanning/discovery** — user enters known device info directly
- **P2P pairing** — both peers enter each other's device name + serial + shared passphrase
- **Deterministic key derivation** — HKDF-SHA256 from sorted identities + passphrase; no key exchange over mesh
- **Double encryption** — AES-256-GCM (app layer) + Meshtastic channel PSK (LoRa layer)
- **Passphrase never stored** — cleared from memory after key derivation
- **Full device config** — replaces official Meshtastic app (region, modem, channel, PSK, power, hops)

## Build Commands
```bash
cd ui && npm install && cd ..
cargo tauri dev          # desktop dev
cargo tauri build        # desktop release
cargo tauri android init && cargo tauri android build  # Android
```

## Project Structure
```
meshguard/
├── src-tauri/
│   ├── src/
│   │   ├── lib.rs            # Tauri app entry
│   │   ├── main.rs           # Binary entry
│   │   ├── ble.rs            # BLE — direct connect to known address
│   │   ├── crypto.rs         # P2P key derivation + AES-256-GCM
│   │   ├── device_config.rs  # Full Meshtastic config (radio, channel, PSK)
│   │   ├── protocol.rs       # Encrypted message types
│   │   ├── commands.rs       # Tauri IPC — setup, pairing, messaging
│   │   ├── state.rs          # Shared app state
│   │   └── error.rs          # Error types
│   ├── Cargo.toml
│   └── tauri.conf.json
├── ui/
│   ├── src/
│   │   ├── main.js           # 3-screen flow: Setup → Pairing → Chat
│   │   └── styles/main.css   # Dark theme
│   ├── index.html
│   └── package.json
└── .github/workflows/
    ├── ci.yml                # Check + test + clippy on push/PR
    └── release.yml           # Build all platforms on tag push
```

## Release Versioning
Date-based: `v2026.03.23`
