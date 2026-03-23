# MeshGuard — Secure P2P Mesh Messenger

## Project Overview
Android app for encrypted peer-to-peer communication between two Sensecap P1000 Meshtastic devices.

## Architecture
- **Tauri 2.0 Mobile** — Rust backend + web frontend, targeting Android
- **Rust core** (`src-tauri/src/`) — BLE, crypto, Meshtastic protocol
- **Web UI** (`ui/`) — vanilla JS + CSS, dark theme, mobile-first

## Key Design Decisions
- **Encryption**: X25519 key exchange → HKDF-SHA256 key derivation → AES-256-GCM
- **BLE**: btleplug for cross-platform Bluetooth Low Energy to Meshtastic devices
- **Protocol**: JSON-serialized messages over Meshtastic mesh (max ~228 bytes per payload)
- **No dependencies on Google services** — fully P2P via LoRa mesh

## Build Commands
```bash
# Dev (desktop preview)
cd src-tauri && cargo build

# Android
cargo tauri android init
cargo tauri android dev
cargo tauri android build
```

## Project Structure
```
meshguard/
├── src-tauri/          # Rust backend
│   ├── src/
│   │   ├── lib.rs       # Tauri app entry, module wiring
│   │   ├── main.rs      # Binary entry point
│   │   ├── ble.rs       # BLE manager for Meshtastic devices
│   │   ├── crypto.rs    # X25519 + AES-256-GCM encryption
│   │   ├── protocol.rs  # Message types and serialization
│   │   ├── commands.rs  # Tauri IPC commands (frontend ↔ backend)
│   │   ├── state.rs     # Shared app state
│   │   └── error.rs     # Error types
│   ├── Cargo.toml
│   └── tauri.conf.json
├── ui/                 # Web frontend
│   ├── src/
│   │   ├── main.js      # App logic, screen navigation, messaging
│   │   └── styles/
│   │       └── main.css # Dark theme, animations
│   ├── index.html
│   └── package.json
└── proto/              # Meshtastic protobuf definitions (future)
```
