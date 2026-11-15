# ADR-005: Message store: conversation history, inbox and outbox

**Status:** Accepted (2026-11-15). The owner chose per-field encryption, the OS keyring on desktop, and keep-until-deleted retention.
**Context docs:** RFC §7 (durable outbox), ADR-001 §5 (one Rust-owned SQLite store), ADR-002 (vault key)

## Context

Messages exist only in the WebView's memory (`state.messages` in `main.js`). Closing the app loses the history. A send that fails because the radio is away is lost too, and nothing retries it. ADR-001 needs a store that the Android background service can use without Tauri: a reply from a notification must be written to an outbox **before** any radio write (RFC §6). The only durable message state today is the dedup cache in `dedup.json`.

## Decision

### 1. One store, owned by Rust

- A new crate, `crates/meshguard-store`, with `rusqlite` (`bundled` SQLite) and `meshguard-core`. It has no Tauri, tokio or Android dependency, so the Tauri commands and the future JNI engine (ADR-001 option D) use the same code.
- One file, `store.db`, in the app config directory. WAL journal, `foreign_keys = ON`, `secure_delete = ON` (freed pages are zeroed; see §6 for the limits).
- One connection per process behind a mutex. The engine is a process singleton (ADR-001), so there is no cross-process access.

### 2. Encryption: per field, from the vault key

- **Store key** = `HKDF-SHA256(salt "meshguard-store-v1", ikm = vault key, info "store key")`. The vault key is the random key from ADR-002 (wrapped by Android Keystore).
- Encrypted columns hold `nonce(12) ‖ AES-256-GCM(store key, plaintext) ‖ tag(16)`. The AAD binds every value to its row and column (`"mgs1" ‖ table ‖ column ‖ row key`), so a value cannot be moved to another row or column without failing to decrypt.
- **Encrypted:** message bodies and peer display names.
- **Plaintext:** ids, node numbers, timestamps, direction, delivery state and counters. Sorting, unread counts and retry scheduling need them, and they reveal no content. The trade-off is that someone holding the unlocked file can see *who and when*, but not *what*. Full-database encryption (SQLCipher) was rejected: it would bring OpenSSL into the NDK build for this metadata only.
- **No vault, no store.** If the vault cannot be opened, nothing is written to disk and history stays in memory for the session, as it does today. The UI says history is not being saved.

### 3. Desktop vault in the OS keyring

- Desktop keeps a random 32-byte vault key in the OS keyring (`keyring` crate: Secret Service on Linux, Keychain on macOS) under service `net.meshguard.app`, account `vault-v1`.
- With no keyring available (for example a headless Linux session without Secret Service), the desktop falls back to memory only, as above.
- Side effect: desktop now also remembers chat keys (ADR-002), because ADR-002 seals them with the same vault.

### 4. Schema (version 1)

```sql
CREATE TABLE conversations (
  conversation_id TEXT PRIMARY KEY,     -- "c:<my node hex8>:<peer node hex8>"
  my_node         INTEGER NOT NULL,     -- local radio; chat keys are per local radio (ADR-002)
  peer_node       INTEGER NOT NULL,
  title_enc       BLOB,                 -- encrypted display name
  last_message_at INTEGER NOT NULL DEFAULT 0,
  unread_count    INTEGER NOT NULL DEFAULT 0,
  UNIQUE (my_node, peer_node)
);

CREATE TABLE messages (
  conversation_id TEXT NOT NULL REFERENCES conversations ON DELETE CASCADE,
  message_id      BLOB NOT NULL,        -- 16 bytes; v2 id, or random local id for v1 peers
  direction       INTEGER NOT NULL,     -- 0 = in, 1 = out
  kind            INTEGER NOT NULL,     -- 1 text, 4 status (check-in/SOS)
  body_enc        BLOB NOT NULL,
  created_at      INTEGER NOT NULL,     -- sender's time (v2 created_at) or local time
  stored_at       INTEGER NOT NULL,
  state           TEXT NOT NULL,        -- out: DeliveryState::as_str(); in: "received" | "read"
  PRIMARY KEY (conversation_id, message_id, direction)
);
CREATE INDEX messages_by_time ON messages (conversation_id, created_at);

CREATE TABLE outbox (                   -- one row per unsettled outgoing message
  conversation_id TEXT NOT NULL,
  message_id      BLOB NOT NULL,
  direction       INTEGER NOT NULL DEFAULT 1 CHECK (direction = 1),
  attempts        INTEGER NOT NULL DEFAULT 0,
  next_deadline   INTEGER NOT NULL,     -- next send, or ACK deadline
  expires_at      INTEGER NOT NULL,
  last_transport  TEXT,
  PRIMARY KEY (conversation_id, message_id),
  FOREIGN KEY (conversation_id, message_id, direction)
    REFERENCES messages (conversation_id, message_id, direction) ON DELETE CASCADE
);
```

- The outbox holds no copy of the text. A retry decrypts the body from `messages` and seals it again with a fresh nonce and the **same** `message_id` (ADR-003), so the recipient's dedup recognises it.
- Outbox rows follow `meshguard_core::delivery::OutboundMessage`. The state machine and retry policy stay in the core crate; the store only persists them.
- Writing an outgoing message and its outbox row is one transaction. The radio write happens only after the commit.
- An incoming message is inserted with `INSERT OR IGNORE` on its key, so a relayed duplicate never creates a second bubble, even if the dedup cache was lost.

### 5. Migrations

- `PRAGMA user_version` holds the schema version. Migrations are forward-only, numbered, and each runs in one transaction.
- A database newer than the app (a downgrade) is not modified: the version is checked before any write, including the WAL journal-mode switch. The store is unavailable and history stays in memory, so an older build never damages newer data.
- Tests open every earlier version and migrate it.

### 6. Retention and deletion

- History is kept until the user deletes it. **Delete conversation** removes the conversation, its messages and its outbox rows.
- Deletion is logical plus best-effort overwriting, not guaranteed erasure: `secure_delete` zeroes the freed pages, and a `wal_checkpoint(TRUNCATE)` after the delete moves them into the main file and empties the WAL, which would otherwise keep older copies of the rows. Flash storage (wear levelling) and backups made earlier may still hold old blocks. Those copies are ciphertext under the store key, so the encryption, not the overwrite, is the protection.
- Unsent messages leave the outbox when they are delivered, fail, or pass their TTL (`DeliveryState::Expired`); the message row keeps the final state.
- Forgetting a peer does not delete its history. The UI offers that as a separate action.

### 7. Scope of the first implementation

1. The `meshguard-store` crate with schema, migrations, encryption and tests (desktop CI).
2. The desktop keyring vault.
3. The Tauri commands write incoming and outgoing messages, and the UI loads history from the store: a chat list sorted by last message, with unread counts.
4. The retry loop drives the outbox through `meshguard_core::delivery`.

Moving `dedup.json` into the store (`received_ids`) is a later migration, not part of version 1.

## Consequences

- The app keeps message content on the device for the first time. The privacy policy (app and meshguard.net) must say so before release: history is stored encrypted, kept until deleted, never uploaded.
- Content is protected by the vault: Android Keystore on Android, the OS keyring on desktop. A device unlocked by an attacker who can also use the Keystore or keyring can read history. The planned app lock (PIN/biometric, ADR-002 follow-up) is the next protection layer.
- The background service (ADR-001) gets its outbox and history from this crate, with no second store.
