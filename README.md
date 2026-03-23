# MeshGuard

Secure peer-to-peer encrypted messenger for Meshtastic devices. No internet, no servers, no third-party apps. MeshGuard replaces the official Meshtastic app entirely — it configures your device, pairs you with a specific peer, and provides private encrypted communication over the LoRa mesh.

```
   You (Phone/Desktop)              Peer (Phone/Desktop)
         │                                    │
    BLE ─┘                                    └─ BLE
         │                                    │
   ┌─────┴──────┐      LoRa Mesh       ┌─────┴──────┐
   │ Meshtastic │ ◄────────────────────►│ Meshtastic │
   │   Device   │   encrypted channel   │   Device   │
   └────────────┘     (up to 15km)      └────────────┘
```

**No scanning. No discovery. No key exchange over the air.**

Both peers enter each other's device name, serial number, and a passphrase they agreed on in person. MeshGuard derives identical encryption keys on both sides — nothing secret ever travels over the mesh.

## Downloads

Grab the latest build from [**Releases**](https://github.com/nemke82/meshguard/releases).

| Platform | File | Install |
|----------|------|---------|
| Android | `.apk` | Enable "Install from unknown sources", open the APK |
| Ubuntu / Debian | `.deb` | `sudo dpkg -i meshguard-*.deb` |
| RHEL / Fedora | `.rpm` | `sudo dnf install meshguard-*.rpm` |
| Linux (any) | `.AppImage` | `chmod +x meshguard-*.AppImage && ./meshguard-*.AppImage` |
| macOS | `.dmg` | Open the DMG, drag MeshGuard to Applications |

Verify integrity:
```bash
sha256sum -c SHA256SUMS.txt
```

---

## How to Connect Two Meshtastic Devices and Start Communicating

### What You Need

- **2 Meshtastic devices** (Sensecap P1000, T-Beam, Heltec, RAK, or any Meshtastic-compatible hardware)
- **2 phones or computers** running MeshGuard
- Bluetooth enabled on both
- Both devices must use the **same region frequency** (e.g., both EU868 or both US915)

### Step 1 — Find Your Device Info

Before launching MeshGuard, note down each device's info:

| Info | Where to find it |
|------|-----------------|
| **Device Name** | Printed on device, or shown on the device's screen / web interface |
| **Device Serial** | Printed on the device label or in the Meshtastic device info page |
| **BLE Address** | Shown in your phone's Bluetooth settings (e.g., `AA:BB:CC:DD:EE:FF`) |

Write these down for **both** devices. You'll need to share your info with your peer (in person, phone call, or any secure channel).

### Step 2 — Install MeshGuard on Both Devices

Download from the [Releases page](https://github.com/nemke82/meshguard/releases) and install on both phones/computers.

### Step 3 — Configure Your Local Device

Open MeshGuard. The first screen asks for **your** device configuration:

1. **Device Name** — enter your Meshtastic device's name (e.g., "Alice-P1000")
2. **Device Serial** — enter the serial number from the device
3. **BLE Address** — enter your device's Bluetooth address (you only do this once, it's saved)
4. **Region** — select your LoRa region (must match your peer's region)
5. **Modem Preset** — choose range vs. speed (Long Range recommended for maximum distance)
6. **TX Power** — transmit power in dBm (default 20)
7. **Hop Limit** — how many mesh hops allowed (1 = direct P2P only, 3 = default)

Click **"Save & Continue to Pairing"**.

MeshGuard writes these settings directly to your Meshtastic device via Bluetooth — no need for the official Meshtastic app.

### Step 4 — Set Up P2P Pairing

The pairing screen is where privacy begins. Both you and your peer must enter:

1. **Peer Device Name** — your peer's Meshtastic device name
2. **Peer Device Serial** — your peer's device serial number
3. **Shared Passphrase** — a secret passphrase you **both agreed on beforehand** (in person, phone call, etc.)

**Critical**: The passphrase must be identical on both sides. It is:
- Never stored on disk
- Never transmitted over the mesh
- Never sent to any server
- Cleared from memory after key derivation

Click **"Establish Secure Session"**. MeshGuard:
1. Derives an AES-256 encryption key from both device identities + passphrase
2. Derives a Meshtastic channel PSK from the same inputs
3. Connects to your local device via Bluetooth
4. Pushes the radio config and encrypted channel to the device
5. Opens the chat screen

### Step 5 — Start Communicating

You're now in the encrypted chat. Type your message and send.

**What happens when you send a message:**

```
Your Phone                  Your Device            Peer Device              Peer Phone
    │                           │                       │                       │
    │ 1. Type message           │                       │                       │
    │ 2. Encrypt (AES-256-GCM)  │                       │                       │
    │ 3. Send via BLE ─────────►│                       │                       │
    │                           │ 4. Encrypt again with │                       │
    │                           │    channel PSK        │                       │
    │                           │ 5. LoRa transmit ────►│                       │
    │                           │                       │ 6. Decrypt channel ──►│
    │                           │                       │ 7. Forward via BLE    │
    │                           │                       │                       │ 8. Decrypt AES-256
    │                           │                       │                       │ 9. Display message
```

Messages are **double-encrypted**:
- **Layer 1**: AES-256-GCM encryption by MeshGuard (your phone → peer's phone)
- **Layer 2**: Meshtastic channel PSK encryption (device → device over LoRa)

Even if someone captures the LoRa signal AND knows the channel PSK, they still can't read the messages without the AES-256 key (which requires knowing the passphrase).

### Step 6 — Reconnecting

Your device config and peer info are saved (passphrase is NOT saved). Next time you open MeshGuard:
- If you had a session: you'll go straight to chat (re-enter passphrase if app was restarted)
- If you had device config: you'll go to the pairing screen
- New install: you'll start at device setup

---

## Troubleshooting

| Problem | Solution |
|---------|----------|
| Can't connect to local device | Verify the BLE address is correct. Make sure Bluetooth is on and the device is powered up within ~10m. |
| Messages not arriving | Both devices must be on the same region and the same channel PSK. Re-enter the passphrase on both sides. |
| "No session" error | The passphrase hasn't been entered this session. Go to settings and re-pair. |
| Garbled messages | The passphrase doesn't match between peers. Both must enter the exact same passphrase. |
| Short range | Use Long Range modem preset. Place devices high with line of sight. Use external antenna if available. |

## Tips for Best Performance

- **Same passphrase** — triple-check both sides entered the identical passphrase
- **Elevation** — place Meshtastic devices as high as possible
- **Line of sight** — LoRa reaches 15+ km over water/flat terrain, 2-5 km in urban areas
- **External antenna** — dramatically improves range on supported devices
- **Hop limit 1** — for maximum privacy, set hop limit to 1 (direct only, no mesh relay)
- **Message length** — keep messages under 200 characters for reliable single-packet delivery

---

## Security Model

| Layer | Protection |
|-------|-----------|
| Message encryption | AES-256-GCM (authenticated encryption) |
| Key derivation | HKDF-SHA256 from device identities + passphrase |
| Channel encryption | Meshtastic PSK derived from same pairing inputs |
| Key exchange | **None over the air** — keys derived locally from shared secret |
| Memory safety | Rust (no buffer overflows); keys zeroized on drop |
| Passphrase handling | Never stored, never transmitted, cleared after use |
| Transport | LoRa mesh — no internet, no servers |
| MQTT/uplink | Disabled — no data leaves the mesh |

**Threat model**: An attacker who captures LoRa packets would need to:
1. Break the Meshtastic channel PSK (derived from identities + passphrase)
2. Break the AES-256-GCM encryption (requires the same passphrase)
3. Both are computationally infeasible without knowing the shared passphrase

---

## Building from Source

### Prerequisites

- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) 20+
- [Tauri CLI](https://tauri.app/) (`cargo install tauri-cli --version "^2"`)

**Linux (Ubuntu/Debian):**
```bash
sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libdbus-1-dev pkg-config libssl-dev
```

**Linux (RHEL/Fedora):**
```bash
sudo dnf install gcc gcc-c++ webkit2gtk4.1-devel libappindicator-gtk3-devel librsvg2-devel patchelf dbus-devel openssl-devel pkg-config rpm-build
```

**macOS:**
```bash
xcode-select --install
```

### Build & Run

```bash
cd ui && npm install && cd ..

# Desktop (dev)
cargo tauri dev

# Desktop (release)
cargo tauri build

# Android
cargo tauri android init
cargo tauri android dev
cargo tauri android build
```

### Creating a Release

Tag with a date version:
```bash
git tag v2026.03.23
git push origin v2026.03.23
```

The CI pipeline builds Android APK, .deb, .rpm, .AppImage, and macOS .dmg, then publishes them as a GitHub Release with SHA256 checksums.

## License

MIT
