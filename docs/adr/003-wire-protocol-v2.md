# ADR-003: Wire protocol v2 — stable message id, direction-bound AEAD, dual-read

**Status:** Accepted (2026-11-14). The owner approved the format and the rollout in §3. The codec is in `crates/meshguard-core`. Rollout steps 1–3 are implemented in the live path: dual-read, a per-peer `protocol` pin, auto-upgrade on an authenticated v2 frame, "MeshGuard 2.0 mode" pairing, ACKs within an airtime budget, and rate-limited chat-request notifications.
**Date:** 2026-11-14
**Related:** RFC §4, ADR-001, `docs/audit/p0-baseline.md`

## 1. Context

v1 is `nonce(12) | AES-256-GCM([type][UTF-8])`, with no AAD. The receiver assigns a new UUID and timestamp on every receipt (`protocol.rs::from_wire`), so it cannot:

- deduplicate relayed copies,
- send an application ACK that refers to a specific message,
- reuse an id across retries or transports,
- distinguish "Bob → Alice" from a reflected "Alice → Bob" frame. Both peers derive the same symmetric key.

## 2. Decision

### Frame layout

```
frame = "MG2" (3) | nonce (12) | AES-256-GCM(inner) | tag (16)
inner = version (1) = 0x02 | type (1) | message_id (16)
      | created_at (u32 BE, unix s) | ttl_mins (u16 BE) | body (rest)
aad   = "MG2" | u16 len | sender_identity | u16 len | recipient_identity
```

| Decision | Choice | Reason |
|---|---|---|
| Discriminator | Clear-text `MG2` | Cheap routing between v1 and v2 before any crypto, and it is also bound into the AAD |
| Message id | 16 random bytes (OS CSPRNG) | RFC requirement. Negligible collision risk without coordination. 8 bytes would save airtime; that was considered and rejected as premature |
| Body length | Implied by the AEAD length | The RFC's varint is redundant: GCM authenticates the exact length, so this saves 1–2 bytes |
| TTL unit | **Minutes**, `u16` (max ~45 days) | Seconds would cap at ~18 h, too short for the P4 gateway's store-and-forward |
| Direction binding | `(sender, recipient)` identities in the AAD | Stops reflection. Turns the RFC's "bind routing identity" item into something concrete |
| Identity (interim) | `PeerIdentity::meshtastic_node(n)` = `'M' ‖ u32 BE` | Stable per radio for now. Replaced by the identity ADR, which needs a new AAD context version, not a new frame version |
| Version byte | Inside the ciphertext | Authenticated. An unknown version or type after successful authentication gives `Unsupported`, not `AuthFailed` |
| Retries | Same `message_id`, re-sealed with a fresh nonce | Never reuse a (key, nonce) pair. The receiver deduplicates by id |
| ACK | `type 0x10`; body = acknowledged id; has its own id; is **never** acknowledged | Rules out ACK-of-ACK loops. Only an authenticated ACK moves a message to `PEER_ACK` |

Fixed overhead is **55 bytes**. With the default 228-byte budget the maximum body is **173 bytes**, measured in UTF-8 bytes, not characters. v1 assumed 160 characters plus 29 bytes of overhead.

### Payload budget

`DEFAULT_FRAME_BUDGET = 228`, passed explicitly to `seal()`. `meshtastic` 0.1.8 defines `DATA_PAYLOAD_LEN = 233`, which excludes `Data` protobuf overhead. The real number for T1000-E firmware gets measured in P3 (RFC T11) before this is raised. Messages that are too large are rejected (`SealError::TooLarge`). There is no automatic fragmentation.

### Dual-read and failure taxonomy

`codec::decode_frame(frame, key, context)`:

1. Empty → `Malformed`. No key → `Locked` (keep the ciphertext and retry after unlock).
2. If the frame starts with `MG2` and is at least 55 bytes long, try v2. On success → v2. If it authenticates but the version or type is unknown → `Unsupported`. If it authenticates but the body is invalid → `Malformed`.
3. Otherwise, or if v2 authentication fails, try v1. A v1 nonce starts with `MG2` with probability 2⁻²⁴, so the marker alone never rules out v1.
4. If both fail → `AuthFailed`.

None of these errors carry plaintext.

### Receiver rules (engine, next slice)

- **Freshness:** reject a TTL above the receiver's maximum (`TtlTooLong`), a message created more than `skew` in the future, or one expired by more than `skew`. The maximum TTL must be ≤ the dedup retention − 2·skew, otherwise a long-TTL frame could be replayed after its id has been forgotten.
- **Dedup** on `(sender_identity, message_id)`, with retention ≥ max TTL + 2·skew. Entries are **never evicted before they expire**. If the cache is full of unexpired ids, new ids are `Refused`: the frame is dropped without being shown or ACKed, and the sender's bounded retry delivers it later. The cache is persisted across restarts (`DedupCache::entries`/`restore`).
- Show and notify only on the first observation.
- **ACK budget:** duplicates are re-ACKed so a lost ACK can recover, but at most `max_acks_per_message` times per id (`Seen::should_ack`). A replay flood therefore costs at most that many transmissions per captured frame. The engine slice must also add a **global outbound airtime budget** (token bucket per transport) covering ACKs and retries, so many distinct captured frames cannot exhaust the channel.

## 3. Rollout and negotiation (approved)

An old (v1-only) build that receives a v2 frame cannot decrypt it, and today it treats *any* undecryptable `PrivateApp` packet as a **pair request** and posts a "Chat Request" notification (audit D6). So v2 must never be sent to a peer that may still run v1.

Proposed rollout:

1. **Release N (next slice):** dual-read in `mesh_radio.rs::handle_incoming_packet`; still send v1. Add a `protocol` field (default `v1`) to `PeerConfig`. Incoming v2 messages carry their stable id to the UI.
2. **Release N+1:** a peer switches to v2 sending when (a) any v2 frame from it authenticates, or (b) the user pairs with the "MeshGuard 2.0" option, which sends the pairing frames as v2. Both of the owner's phones will run N+1 for the P3 test.
3. **Pin both directions.** When a peer switches to v2, persist `min_receive_version = 2` for it, and from then on reject v1 frames from that peer (`Unsupported`, with no pair-request fallback). Otherwise recorded v1 frames, which have no direction binding, no id and no freshness, could still be replayed against an upgraded build.
4. Never downgrade automatically from v2 to v1 (that would invite downgrade attacks). Re-pairing is the only way back, and it resets both pins.

## 4. Test evidence (this PR)

- `dedup.rs`: a full cache refuses rather than forgetting unexpired ids, restore keeps unexpired entries, the per-message ACK budget.
- `envelope.rs`: layout known-answer vector, exact budget boundary, multibyte byte counting, every single-bit flip rejected, every truncation rejected, wrong key or wrong direction rejected, `Unsupported` and `Malformed` after authentication, freshness window.
- `codec.rs`: v1 and v2 dual-read, v1 frame whose nonce begins with `MG2`, `Locked`, typed errors, garbage input never panics.
- `tests/fixtures.rs`: v1 and v2 vectors from an **independent Python implementation** (`tests/vectors/gen_vectors.py`) decode in Rust. CI regenerates them and fails on drift.
- `tests/engine_sim.rs`: two peers on a mock medium with a virtual clock. Covers relayed duplicates shown once, a replay flood with bounded ACKs, an over-long TTL rejected, lost ACK leading to a same-id retry, a link down then recovering, bounded attempts leading to `Failed`, TTL leading to `Expired`, no ACK-of-ACK, reflection rejected, a locked receiver, and dedup surviving a restart.
- `src-tauri/src/protocol.rs`: frames from the shipping v1 send path decode through the core dual-reader.

## 5. Not decided here

- Stable cryptographic peer identity and KDF migration. This is a separate ADR; the RFC calls for Argon2id or a QR-shared key.
- Key persistence and lock behaviour (ADR-002).
- Read receipts (`READ`) and Meshtastic-level ACK mapping (`MESH_ACK`) on the wire.
