# ADR-004: Offline QR pairing

**Status:** Accepted (2026-11-14)

## Context

Pairing needed both people to type the same passphrase. Short passphrases
are weak (the key can be brute-forced offline from one captured frame) and
long ones are tedious, which pushed users toward weak ones. The key also
depended on both device names, which broke pairing when a name was not
known yet or changed.

## Decision

1. **Offer.** The device that shows the code (A) creates a random 128-bit
   secret and an X25519 key pair, and shows
   `meshguard:pair?v=1&n=<node hex>&k=<secret base32>&p=<public key base32>`
   as a QR code and as text (for devices without a camera). Valid for 10
   minutes or until *Done*.
2. **Scan.** The scanning device (B) creates its own X25519 key pair and
   derives the **chat key** = `HKDF-SHA256(salt "meshguard-qr-v1",
   ikm = X25519(B, A) ‖ secret, info = min(node) BE ‖ max(node) BE)`.
   It installs that key, then sends a v2 `PairRequest` sealed with the
   **request key** = `HKDF-SHA256(salt "meshguard-qr-v1-request",
   ikm = secret, info = nodes)`. The body is `mgqr1:<B public key hex>:<name>`.
3. **Accept.** While the code is shown, A tries the request key on
   undecryptable frames. A fresh request (freshness and durable replay
   checks) gives A B's public key; A derives the same chat key and answers
   with a `PairAccept` sealed with it. Both sides remember the key (ADR-002).
4. **Several family members** may scan the same code; each gets an
   independent chat key, because it depends on their own X25519 key.
5. **No silent replacement.** A request from a node that is already a
   contact is refused (shown to the user): anyone who saw the code could
   otherwise claim a known node number with their own key. This includes a
   node paired a minute ago under the same offer: to re-pair (e.g. the
   accept was lost), remove the contact first.
7. Pairing operations (passphrase, QR scan, QR accept) run one at a time,
   and saving a contact is transactional, so a failure never leaves a
   half-paired contact or a lost previous key.
6. **Camera** use is only to read the code, mobile only. The page opens the
   camera itself (`getUserMedia`; the WebView asks for the permission) and
   decodes frames with jsQR, so scanning works offline, without Google Play
   Services, and without a native camera view. No image is stored. Typing
   the code is always possible. (Until 2026-11-16 this used the Tauri
   barcode-scanner plugin, which closed the app on some phones.)

## Consequences

- Someone who photographs the code learns the secret and A's public key,
  and can read B's request (B's public key), but cannot derive the chat key
  without a private key. An *active* attacker who saw the code could race
  to pair as a new contact while it is shown; the short validity, *Done*
  and the visible "Paired with …" list limit that.
- Device names are no longer part of the key.
- Passphrase pairing stays available (older versions, remote setup).
- Both sides need this version.
