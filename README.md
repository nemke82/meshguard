# MeshGuard

Secure peer-to-peer encrypted messenger built on the Meshtastic LoRa mesh network. Connect two Sensecap P1000 devices and communicate privately — no internet, no servers, no third parties.

```
   You (Phone/Desktop)          Peer (Phone/Desktop)
         │                              │
    BLE ─┘                              └─ BLE
         │                              │
   ┌─────┴─────┐    LoRa Mesh    ┌─────┴─────┐
   │ Sensecap  │ ◄──────────────►│ Sensecap  │
   │   P1000   │   (up to 15km)  │   P1000   │
   └───────────┘                 └───────────┘
```

**Encryption**: X25519 key exchange + HKDF-SHA256 + AES-256-GCM. Keys are generated per session, never stored unencrypted, and zeroized from memory on disconnect.

## Downloads

Grab the latest build from [**Releases**](https://github.com/nemke82/meshguard/releases).

| Platform | File | Install |
|----------|------|---------|
| Android | `.apk` | Enable "Install from unknown sources", open the APK |
| Ubuntu / Debian | `.deb` | `sudo dpkg -i meshguard-*.deb` |
| RHEL / Fedora | `.rpm` | `sudo dnf install meshguard-*.rpm` |
| Linux (any) | `.AppImage` | `chmod +x meshguard-*.AppImage && ./meshguard-*.AppImage` |
| macOS | `.dmg` | Open the DMG, drag MeshGuard to Applications |

After installing, verify the download integrity:
```bash
sha256sum -c SHA256SUMS.txt
```

## How to Connect Two Meshtastic Devices and Start Communicating

### What You Need

- **2x Sensecap P1000** Meshtastic devices (or any Meshtastic-compatible hardware)
- **2x phones or computers** — each running MeshGuard
- Bluetooth enabled on both phones/computers

### Step 1 — Power On the Sensecap P1000 Devices

Turn on both Sensecap P1000 devices. Wait for the LED to show a steady blink — this means the device has booted and is advertising over Bluetooth.

- The P1000 uses **LoRa** for device-to-device communication (range up to 15+ km line-of-sight)
- It uses **Bluetooth Low Energy (BLE)** to connect to your phone or computer

Make sure both devices are on the **same Meshtastic region and channel settings**. By default they ship with matching settings out of the box.

### Step 2 — Install MeshGuard on Both Phones/Computers

Download MeshGuard from the [Releases page](https://github.com/nemke82/meshguard/releases) and install it on both devices. See the table above for platform-specific instructions.

### Step 3 — Pair Device A with Phone A

1. Open MeshGuard on **Phone A**
2. Make sure Bluetooth is turned on
3. Tap **"Scan for Devices"**
4. Your Sensecap P1000 will appear in the list (shown as "Meshtastic" or "P1000" with signal strength)
5. Tap the device to connect
6. The app will show **"Secure connection established"** with an encryption banner

### Step 4 — Pair Device B with Phone B

Repeat Step 3 on **Phone B** with the second Sensecap P1000 device.

### Step 5 — Start a Secure Session

Once both phones are connected to their respective P1000 devices:

1. MeshGuard automatically initiates a **key exchange** (X25519 Diffie-Hellman) through the mesh
2. Both devices derive a shared encryption key — this key never travels over the air in plain form
3. The encryption banner shows: **"End-to-end encrypted with AES-256-GCM + X25519"**
4. You're ready to chat

### Step 6 — Send Messages

- Type your message (up to 228 characters — this is the Meshtastic payload limit)
- Press **Enter** or tap the **Send** button
- Messages are encrypted on your phone before being sent to the P1000 via Bluetooth
- The P1000 transmits the encrypted payload over LoRa to the other P1000
- The receiving P1000 forwards it via Bluetooth to the other phone
- MeshGuard decrypts and displays the message
- You'll see delivery checkmarks: **✓** sent, **✓✓** delivered

### Communication Flow

```
Phone A                    P1000 A              P1000 B                    Phone B
   │                          │                    │                          │
   │ 1. Type message          │                    │                          │
   │ 2. Encrypt (AES-256)     │                    │                          │
   │ 3. Send via BLE ────────►│                    │                          │
   │                          │ 4. LoRa transmit ─►│                          │
   │                          │                    │ 5. Forward via BLE ─────►│
   │                          │                    │                          │ 6. Decrypt
   │                          │                    │                          │ 7. Display
   │                          │                    │◄─ 8. Receipt (LoRa) ─────│
   │◄── 9. Receipt (BLE) ────│                    │                          │
   │ ✓✓ Delivered             │                    │                          │
```

### Troubleshooting

| Problem | Solution |
|---------|----------|
| Device not found during scan | Make sure Bluetooth is on and the P1000 LED is blinking. Move closer (BLE range is ~10m). |
| Connection drops | The P1000 may have timed out. Power cycle the device and reconnect. |
| Messages not delivering | Check that both P1000s are on the same Meshtastic channel and region. LoRa range depends on terrain — try line-of-sight. |
| "No active session" error | The key exchange hasn't completed. Disconnect and reconnect both sides. |
| Slow message delivery | LoRa is low-bandwidth by design. Messages may take 2-10 seconds depending on mesh conditions. |

### Tips for Best Range

- **Elevation matters** — place P1000 devices as high as possible
- **Line of sight** — LoRa can reach 15+ km over water or flat terrain, 2-5 km in urban areas
- **External antenna** — the P1000 supports an external antenna for dramatically better range
- **Mesh hopping** — if you have additional Meshtastic devices between the two endpoints, messages will automatically hop through them

## Security Model

| Layer | Protection |
|-------|-----------|
| Key exchange | X25519 Elliptic Curve Diffie-Hellman |
| Key derivation | HKDF-SHA256 with application-specific context |
| Message encryption | AES-256-GCM (authenticated encryption) |
| Memory safety | Rust (no buffer overflows); keys zeroized on drop |
| Transport | LoRa mesh — no internet, no servers |
| Forward secrecy | New keypair generated per session |

No message content ever exists in plaintext outside of the sender's and receiver's devices.

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
# Install frontend dependencies
cd ui && npm install && cd ..

# Desktop (dev mode)
cargo tauri dev

# Desktop (release)
cargo tauri build

# Android
cargo tauri android init
cargo tauri android dev     # dev on connected device
cargo tauri android build   # release APK
```

### Creating a Release

Tag a version to trigger the release pipeline:
```bash
git tag v0.1.0
git push origin v0.1.0
```

This builds all platforms (Android APK, .deb, .rpm, .AppImage, macOS .dmg) and publishes them as a GitHub Release with SHA256 checksums.

## License

MIT
