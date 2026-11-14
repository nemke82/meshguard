# ADR-004: Offline QR pairing

**Status:** Accepted (2026-11-14)

## Context

Pairing needed both people to type the same passphrase. Short passphrases
are weak (the key can be brute-forced offline from one captured frame) and
long ones are tedious, which pushed users toward weak ones. The key also
depended on both device names, which broke pairing when a name was not
known yet or changed.

## Decision

1. The device that shows the code generates a random **128-bit secret** and
   displays `meshguard:pair?v=1&n=<its node hex>&k=<secret base32>` as a QR
   code and as text (for devices without a camera).
2. Chat key = `HKDF-SHA256(salt "meshguard-qr-v1", ikm secret,
   info min(node) BE ‖ max(node) BE)`. No device names, no passphrase.
3. The scanning device derives the key, installs it and sends a v2
   `PairRequest`. While the code is shown (10 minutes, or until *Done*), the
   showing device tries the code's key on undecryptable frames; a fresh
   (freshness + durable replay check) v2 `PairRequest` is **accepted
   automatically** and both sides remember the key (ADR-002).
4. Several family members may scan the same code while it is shown.
5. The camera is used only to read the code (Tauri barcode-scanner plugin,
   mobile only); no image is stored.

## Consequences

- The secret travels only optically, in person. Anyone who photographs the
  code while it is valid can pair; the short validity and the explicit
  *Done* limit that.
- Passphrase pairing stays available (older versions, remote setup).
- Both sides need this version; older builds cannot read the code.
