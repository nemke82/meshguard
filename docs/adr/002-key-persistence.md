# ADR-002: Remembering chat keys on the device

**Status:** Accepted (2026-11-14)
**Context:** first two-device field test; users had to re-enter the passphrase
for every chat after each app restart.

## Decision

1. **What is stored:** the derived 32-byte session key per `(local radio, peer)`.
   The passphrase is never stored.
2. **How:** each key is sealed with AES-256-GCM under a random local *vault
   key*. The associated data is `"meshguard-keystore-v1" ‖ my_node ‖ peer_node`,
   so a blob only opens for the radio and peer it was made for. Blobs live in
   `session_keys.json` next to `meshguard.json` (format in
   `meshguard-core::keystore`).
3. **Vault key protection:**
   - **Android:** wrapped by a non-exportable AES-256-GCM key in the Android
     Keystore (alias `meshguard_vault_v1`). `vault.key` holds only
     `iv ‖ ciphertext`. The Keystore key is **not** bound to user
     authentication, so the app can reconnect and decrypt in the background
     (ADR-001 service work).
   - **Desktop:** no supported key store yet → no vault → keys stay in memory
     only (previous behaviour). OS keychain support is a follow-up.
4. **Lifecycle:**
   - saved after `start_chat` / `accept_chat`;
   - restored into memory after every successful connect, only for peers still
     in the config and only for the connected radio;
   - deleted with *Remove peer*;
   - if the vault key cannot be unwrapped (Keystore reset, data restored to a
     new phone), stored keys are discarded and a new vault is created. Users
     then pair again. Auto Backup is disabled, so blobs do not travel to other
     devices in the first place.
5. **App lock:** a 6-digit PIN or biometric unlock is a separate, local UI
   gate (follow-up). It never derives or weakens the chat encryption key: a
   6-digit secret over the air could be brute-forced offline in about a
   second.

## Consequences

- Anyone who can use the unlocked phone can read and send in remembered chats.
  This is the same model as mainstream messengers; the PIN/biometric app lock
  addresses it.
- A rooted device or an attacker with code execution in the app process can
  ask the Keystore to unwrap the vault key. Hardware-backed storage prevents
  extracting it for offline use.
- Renaming a Meshtastic device no longer breaks an existing chat, because the
  stored key is used instead of re-deriving from the names.
