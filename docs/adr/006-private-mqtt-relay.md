# ADR-006: Private Encrypted MQTT relay (Internet Relay)

**Status:** Proposed (2026-11-16)
**Source:** `MESHGUARD_PRIVATE_ENCRYPTED_MQTT_RFC.md` (2026-11-16)
**Scope:** MVP, app-to-app over a self-hosted broker. The LoRa↔MQTT
gateway (RFC phase 4) is out of scope and needs its own RFC.

Nothing described here is shipped yet. Section 9 tracks what is
implemented.

## 1. Audit: what exists today (main at 19be345)

| Concern | Where | What it means for MQTT |
|---|---|---|
| Transport contract | `meshguard-core/src/transport.rs` | Sync, poll-based `Transport` trait; `send()` returns a `SubmissionReceipt` that is explicitly *not* delivery. `TransportAddress::Opaque` exists. Only the mock implements it; the radio path (`mesh_radio.rs`) does not go through it. |
| Message identity | `meshguard-core/src/ids.rs` | `MessageId` (16 random bytes), reused on retries. `PeerIdentity::meshtastic_node(u32)` is the **only** identity; the AAD binds `sender → recipient` node numbers. |
| Envelope | `meshguard-core/src/envelope.rs` | MG2: `"MG2" ‖ nonce ‖ AES-256-GCM(inner) ‖ tag`, AAD = `"MG2" ‖ context(sender, recipient)`. Types 0x01–0x05, 0x10. Unknown types are dropped by older builds. |
| Inbound pipeline | `meshguard-core/src/inbound.rs`, `meshguard-engine/src/receive.rs` | `process_inbound` does decrypt, AAD, freshness, durable dedup, and builds the ACK. The engine entry point takes a Meshtastic `MeshPacket` (`on_private_app`); the frame-level logic does not depend on the radio. |
| Keys | `meshguard-engine/src/engine.rs`, `src-tauri/src/key_store.rs` | `keys: HashMap<peer_node, SessionKey>` belongs to one local radio (`keys_owner`). Keys are sealed with the vault key in `session_keys.json` and loaded when that radio connects. |
| Outbox | `meshguard-engine/src/outbox.rs`, `src-tauri/src/history.rs` | Seal, store, transmit, `finish_attempt`. A 5 s loop resends due messages *only when the chat's radio is live*. The store persists `last_transport`. |
| Delivery | `meshguard-core/src/delivery.rs` | Truthful states: `submitted` ≠ delivered; only `peer_ack`/`read` are delivered. Late ACKs after failed/expired are accepted. |
| ACKs | `receive.rs` → `Action::SendAck { to, frame }` | Always sent over the radio the frame came from. |
| Vault | `key_store.rs`, `android/kotlin/.../Vault.kt` | Android Keystore-wrapped key; desktop OS keyring. No vault means nothing secret is written to disk. |
| Android service | `MeshGuardConnectionService.kt` | `connectedDevice` foreground service, gate spike only (3A); it does not own the radio yet (3B). The engine is a process singleton (`process_engine`). |
| TLS stack | `src-tauri/Cargo.lock` | No TLS crate is in the tree today. |

**Gaps.**
- Nothing routes a message to more than one transport.
- The engine's receive path is tied to `MeshPacket`.
- ACKs cannot leave through anything but the radio.
- Keys are loaded only after a radio connects.
- There is no place for broker credentials or relay secrets.
- There is no MQTT client.

## 2. Decision summary

1. **Identity: RFC option A.**
   - The relay is only for contacts that are already paired, pinned to wire protocol v2, and confirmed, with a remembered chat key.
   - The MQTT frame is the *same* MG2 frame, sealed with the *same* key and the *same* AAD context `(my_node, peer_node)`.
   - No identity or AAD change.
   - Radio-free contacts need a stable identity ADR (future ADR-007).
2. **Per-conversation relay secret.** It is 256-bit, comes from the OS CSPRNG, and is independent of the chat key.
   - It is exchanged **in-band** over the existing E2EE chat, as new MG2 types (§4), with durable state and deterministic collision rules.
   - Both people must consent. Turning on MQTT globally never turns it on for any contact.
3. **Topics and ACL.**
   - Format: `mg/r1/<recipient>/<sender>/<alias>`. `<recipient>` and `<sender>` are broker usernames.
   - Usernames must be opaque: the app generates them (`mg-` plus 20 random base32 characters) and accepts no other form. Topic paths therefore never carry a name.
   - The alias is a direction-bound HKDF output of the relay secret.
   - The broker enforces two rules: a user reads only its own inbox, and writes only under its own sender segment (§5).
   - A strict mode with exact per-conversation ACL lines is documented. It is the mode that matches RFC §4.2 to the letter; the default mode is a recorded deviation (§5) and **needs the product owner's sign-off**.
4. **Client.**
   - Library: `rumqttc` 0.25, `v5` module (MQTT 5).
   - TLS: rustls with the `ring` provider (cross-compiles to Android without cmake), Mozilla roots (`webpki-roots`). Alternatively the user pastes a private CA, which then replaces the public roots.
   - The `mqtts://` scheme only. There is no plaintext mode and no "skip verification" option, in any build.
5. **One MQTT session per installation**, shared by all chats.
   - Client id: random, generated once and persisted (`mg-<32 hex>`).
   - Session: `clean_start = false`, session expiry 1–7 days (default 3).
   - Publishing: QoS 1, `retain = false`, message expiry set to the message's remaining TTL. One publish is in flight at a time, so PUBACKs match publishes.
6. **Delivery truth.**
   - A broker PUBACK becomes `submitted` with `last_transport = "mqtt"`. The UI calls it *Sent to broker*.
   - Only an authenticated MG2 `AppAck` (on either transport) becomes `peer_ack`, which the UI calls *Delivered*.
7. **Routing.** It is a pure function of the preference, *per-route* attempt counts, relay health and radio health (§6). Each attempt uses exactly one transport.
8. **Secrets.**
   - The broker password, relay secrets and relay exchange state are sealed with the vault key in `relay_secrets.json`, in the same format as `session_keys.json`.
   - Non-secret settings go in `meshguard.json`: host, port, username, CA PEM, preference, client id.
   - Without a vault, the relay cannot be enabled.
9. **LoRa independence.**
   - The relay task is separate from the radio link.
   - No relay error (TLS, auth, DNS, broker down) touches the radio or the engine's keys.
   - ACK allowances and rate limits are separate per transport (§6), so relay traffic cannot use up LoRa's.
10. **Revocation cancels sends.**
    - Blocking or unpairing a contact also cancels its queued messages on *every* transport. They become `failed` and stay in history; this also fixes the existing gap where a re-paired contact would get old queued messages.
    - A per-conversation generation counter is checked immediately before each dispatch, so nothing cancelled is handed to a transport afterwards.
    - Residual: the relay client keeps at most one publish in flight and refuses publishes while disconnected, so at most one frame handed over *before* the block can still reach the broker (including a retransmission after reconnect). This is the same as ciphertext already transmitted, which cannot be recalled.

## 3. Threat model

Assets:
- message bodies, status/SOS/location
- chat keys and relay secrets
- broker credentials
- who-talks-to-whom metadata
- availability of LoRa chat

| Adversary / failure | Can | Cannot (defense) |
|---|---|---|
| Broker operator | See client IPs, connect times, usernames, topic names (including which usernames talk), frame sizes and timing; store ciphertext for the session/message expiry; drop or delay frames | Read or forge content (MG2 AEAD under the chat key); replay old frames (durable dedup and freshness); learn names, node numbers or GPS (usernames are opaque when the suggested ones are used; aliases are random) |
| Other broker user (family ACL) | Publish into my inbox *under its own username*, which identifies it to the broker | Subscribe to my inbox (read ACL); publish as someone else (write ACL is bound to its username); have a frame accepted (I subscribe only to exact topics of my contacts, AEAD binds sender and recipient); flood me without limit (payload cap, separate relay ACK budget, broker quotas) |
| Other broker user (strict ACL) | Nothing on my topics | Publish or subscribe to any topic not listed for it |
| Network attacker | See TLS metadata | Read or modify traffic (TLS with hostname and chain validation; fails closed); downgrade to plaintext (no `mqtt://`) |
| Stolen phone (locked) | — | Read relay secrets or the broker password (vault-sealed at rest) |
| Blocked / unpaired contact | Keep a copy of the old relay secret and publish into my inbox | Have its frames accepted (topic unsubscribed and dropped locally, secret wiped, engine refuses blocked senders); receive my queued messages (cancelled, §2.10) |
| Malicious peer | Send relay control messages | Make my app use the relay without my explicit consent; make me activate a relay on a different broker (broker tags must match, §4) |
| Broker replaying copies | Deliver one frame many times | Use up the LoRa ACK allowance (per-transport ACK accounting, §6); show it twice (shared dedup) |
| Broker or internet outage | Stop internet delivery | Stop LoRa (independent task; per-route attempts make sure a live radio gets bounded attempts, §6) |

**Residual risk, documented to users.**
- The broker sees metadata, including which accounts talk to each other.
- Ciphertext may sit in the broker's queue until session or message expiry, even after a contact is revoked.
- A weak *passphrase*-derived chat key protects the relay-secret exchange too. QR-paired contacts use an X25519-derived key, so this applies only to passphrase pairings. The topic secret does not change the content security, which was already bounded by that key.

## 4. Relay secret exchange (new MG2 message types)

These are new v2 inner types, all `requires_ack`, sealed with the chat key under the unchanged AAD. Older builds drop unknown types.

| Type | Value | Body |
|---|---|---|
| `RelayOffer` | 0x20 | `ver=1` ‖ `secret (32)` ‖ `broker_tag (8)` ‖ `inbox_len (1)` ‖ `inbox (1–64)` |
| `RelayAccept` | 0x21 | `ver=1` ‖ `confirm (16)` ‖ `broker_tag (8)` ‖ `inbox_len (1)` ‖ `inbox (1–64)` |
| `RelayRevoke` | 0x22 | `ver=1` ‖ `confirm (16)` |

Field definitions:
- `inbox`: the sender's broker username.
- `broker_tag`: `SHA-256("meshguard-relay-broker-v1" ‖ lowercase host ‖ ":" ‖ port)[0..8]`.
- `confirm`: `HKDF-SHA256(salt "meshguard-relay-v1", ikm = secret, info "confirm")[0..16]`. It identifies the secret (its *generation*) and shows that the sender holds it.

The largest body is 106 bytes, under the 173-byte v2 body budget.

**Capability discovery.**
- The relay bit is `FEATURE_RELAY = 1 << 1`, and only an authenticated `Caps` from the peer sets it.
- The `Caps` body gains an optional trailing flags byte, which older builds ignore. Bit 0 means "please send your `Caps` back". A receiver answers at most once per peer per hour.
- A build whose `SUPPORTED_FEATURES` grew announces `Caps` (with the reply bit) once to every confirmed v2 contact. So does *Enable internet relay* on a contact whose bit is unknown.
- An offer is sent only after the peer's relay bit is known.

**State per contact** (sealed in `relay_secrets.json`, written *before* any frame is sent or acknowledged):

| State | Meaning |
|---|---|
| `none` | No relay. |
| `offered{secret, attempts, next_at}` | We sent an offer and are waiting for the accept. |
| `incoming{secret, peer_inbox, peer_tag}` | The peer offered and is waiting for our user. |
| `active{secret, peer_inbox, accept_pending}` | Both sides use the relay. On the accepting side, `accept_pending{message_id, attempts, next_at}` is set until the `RelayAccept` is acknowledged. |

Every side also keeps **tombstones**: the `confirm` of each revoked or replaced generation. Each is kept for `InboundPolicy::dedup_retention_secs()` (today 7 days plus 10 minutes of clock skew), inclusive of the boundary, so no frame that still passes freshness checks can outlive its tombstone. An offer or accept matching a tombstone is ignored, so a revoke that overtakes its offer cannot be undone by the late offer.

**Retries.**
- Control messages are retried by the relay state machine, not the message outbox. A retry reuses the original `MessageId`, so the receiver's dedup turns copies into duplicates, which are only re-acknowledged.
- Back-off runs from 1 minute up to 1 hour, at most 8 times, over any available transport.
- Retries stop at the authenticated AppAck of that message.
- Retry state is persisted, so retries resume after a restart.

**Transitions:**
1. **Enable** (user, `none`): requires a configured relay and a known relay bit. Then create a secret, persist `offered`, and send `RelayOffer` (retried as above).
   - If the offer is never acknowledged, the state returns to `none` and the UI says *No answer*.
   - Once acknowledged, it stays `offered` until an accept, a revoke, or the user cancels (which is a revoke).
2. **Offer received** (after authentication and dedup; persisted before the dedup commit and ACK, using the engine's common commit path):
   - In `none`: go to `incoming`. Ask the user.
   - In `incoming`: if it is the same secret, ignore it (idempotent). Otherwise replace (the newest offer wins) and ask again.
   - In `offered` (crossed offers): the offer from the *lower node number* wins. The lower side ignores the peer's offer. The higher side tombstones its own offer and treats the peer's as accepted by its user, who already consented by enabling. It still goes through step 3's checks, and if they fail the offer stays `incoming` with the reason shown.
   - In `active`: a different secret is a rotation and needs the user's accept again. Until then the old secret stays active.
3. **Accept** (`incoming`, by the user or automatically as above): allowed only if this device's relay is configured *and* its broker tag equals `peer_tag`. Otherwise the UI says *Use the same broker as X* and the offer stays pending.
   - Then persist `active` with `accept_pending` and subscribe.
   - Send `RelayAccept`, retried until its AppAck.
   - A duplicate of the accepted offer is re-acknowledged by dedup, and the pending accept retry delivers the accept.
4. **Accept received:** applied only in `offered`, when `confirm` matches our pending secret *and* `broker_tag` equals ours. Then persist `active` with the peer inbox and subscribe. An accept for another generation, or in any other state, is ignored.
5. **Revoke** (user disables relay for the contact, cancels an offer, or *remove settings*): persist `none` and a tombstone, wipe the secret, unsubscribe, and send `RelayRevoke` (retried as above).
6. **Revoke received:** always store a tombstone for its `confirm`. If it matches the current `active`, `offered` or `incoming` secret, wipe and return to `none`.
7. **Block or unpair:** wipe and return to `none` with a tombstone, without sending anything, and cancel queued sends (§2.10).

All transitions are idempotent under duplicates and reordering, because each is keyed by the `confirm` generation.

## 5. Topics and broker ACL

```
alias(sender, recipient) = base32_lower_nopad(
    HKDF-SHA256(salt "meshguard-relay-v1", ikm = secret,
                info = "topic" ‖ sender_node BE32 ‖ recipient_node BE32)[0..20])
topic(sender → recipient) = "mg/r1/" ‖ recipient_user ‖ "/" ‖ sender_user ‖ "/" ‖ alias(sender, recipient)
```

- A publishes only to `topic(A → B)` and subscribes only to `topic(B → A)`. Both are exact topics with no wildcards. Incoming frames on any other topic are dropped by the client.
- The direction comes from node numbers, which are the stable identities under option A.
- The alias is 160 bits rendered as 32 base32 characters. It reveals nothing without the secret, and the directions differ.
- Usernames must have the app's opaque form, `mg-` plus 20 base32 characters (`[a-z2-7]`). The app generates one for the broker administrator to create, so topics carry no names. The wire format allows 1–64 bytes of `[A-Za-z0-9._-]`, so the rule can be relaxed later without a protocol change.

**Family ACL (default, deviation from RFC §4.2).** No per-chat administration is needed: a new conversation never requires a broker change.

```
# every MeshGuard user (Mosquitto acl_file)
pattern read  mg/r1/%u/#
pattern write mg/r1/+/%u/#
```

- A user reads only its own inbox.
- A user can only publish under its own username as sender, so the broker attributes every frame.
- An authenticated user can still drop junk into another user's inbox under its own name. The client ignores it (not subscribed), and the broker can revoke that account.
- **Deviation.** RFC §4.2 asks that publishing be limited to the exact outbound topics of each client. This mode limits it to topics *signed with the client's own username* instead. The accepted risk: a member of the same broker can fill another member's broker queue (bounded by broker quotas) and is identified by the broker when doing so. Content, reading and sender spoofing stay protected as in the strict mode. **Accepting this deviation is a product-owner decision**; without it, strict mode becomes mandatory and every new relay conversation needs a broker-admin step.

**Strict ACL (optional).** For brokers shared with people outside the family. Per *Settings → Internet Relay → contact → Broker access lines*, the app shows the two exact lines an administrator adds for each conversation:

```
user <me>
topic write mg/r1/<peer>/<me>/<alias me→peer>
topic read  mg/r1/<me>/<peer>/<alias peer→me>
```

This requires an admin step for every new conversation. The UI says so, and the relay status shows *Not authorised* on PUBACK/SUBACK refusal.

Both modes require `allow_anonymous false`, a TLS-only listener, and per-person accounts. A shared app-wide credential is never acceptable.

## 6. Routing and delivery

The preference applies to relay-enabled (`active`) contacts. Contacts without relay keep exactly today's LoRa behaviour.

Each message keeps per-route attempt counts in memory, in the engine's in-flight bookkeeping. After a restart they start at zero again, which stays bounded because total attempts are bounded.

| Preference | Next attempt uses |
|---|---|
| **Off** (default) | LoRa only; the relay task does not run, so there is no MQTT traffic |
| **Auto** | If both routes are up: the route *not* used by the previous attempt (the first attempt uses the relay). If one is up: that one. Otherwise wait |
| **LoRa preferred** | LoRa if live and its attempts so far < 2. Otherwise the relay if connected. Otherwise LoRa if live. Otherwise wait |

- A radio that comes back is always reconsidered. In both modes a live radio gets attempts even after several relay attempts. Each attempt goes to one transport.
- The existing `RetryPolicy` (bounded attempts, back-off, ACK timeout) and the message TTL bound the total. SOS gets no dual-send in the MVP.
- The outbox loop picks a due message when *any* route for it is available, not only when its radio is live.
- **ACKs leave on the transport the frame arrived on.** `Action::SendAck` gains a route.
  - The ACK allowance per message is counted *per route*.
  - The global ACK budget is a separate token bucket per route: the LoRa one is unchanged, and the relay one allows a burst of 20 and then 1 per second.
  - Relay replays cannot use up LoRa ACKs.
- **Inbound relay frames:**
  1. The topic must be one of our exact subscribed topics, which maps to `(my_node, peer)`.
  2. `my_node` must equal `keys_owner`.
  3. The payload is at most 512 bytes and must look like MG2. v1 is rejected (never a downgrade).
  4. The peer must not be blocked.
  5. The frame then goes through the same engine path as LoRa (`process_inbound`, the common record/commit, then the ACK). Dedup is shared, so one `MessageId` gives one entry and one notification.
  6. `PairRequest`/`PairAccept` and undecryptable frames from the relay are dropped. There is no pairing over the internet.
- **Keys without a radio.** When the relay is on and no radio is connected, the app adopts the keys of the last connected radio (`Engine::last_radio`), so the relay works before or without a radio link. A different radio later replaces them, as today.
- **Cancellation.** Every conversation has a generation counter, incremented on block, unpair or relay revoke. A `Transmit` carries the generation it was sealed under, and a result for an older generation changes nothing. The relay client drops publishes whose PUBACK arrives after cancellation (they are already at the broker; this residual is documented).

```
Outgoing message (relay-enabled contact)
 queued ──attempt via R──▶ submitted[R] ──AppAck (any route)──▶ peer_ack (Delivered)
   ▲            │ fail          │ ack timeout                       ▲
   └─ retrying ◀┘               └──▶ retrying ─▶ … ─▶ failed/expired ┘ (late ACK still counts)
 any unsettled ──block/unpair──▶ failed (cancelled)

Relay connection
 off ─enable─▶ connecting ─CONNACK ok─▶ connected ─io error─▶ disconnected ─backoff─▶ connecting
                   │ bad cert ─▶ tls_error (no retry until settings change or network change)
                   │ bad user/pass ─▶ auth_failed (no retry until settings change)
 any ─disable─▶ off (disconnect, no reconnect)
```

Examples:
- **LoRa only.** The relay is off, or the contact is not relay-enabled. Behaviour is exactly as today.
- **MQTT only.**
  1. Auto, relay connected, and the radio is out of range or off.
  2. Attempt 0 goes to the relay. PUBACK gives *Sent to broker*.
  3. B's app (subscribed, persistent session) gets the frame and sends an AppAck on `topic(B → A)`.
  4. A marks the message *Delivered*.
- **Mixed.**
  1. Attempt 0 goes to the relay; B is offline from the internet.
  2. After the ACK timeout, attempt 1 goes over LoRa (alternation) and B (in range) acks over LoRa: *Delivered*.
  3. B's broker session later replays attempt 0. Dedup drops it as a duplicate, and the ACK is resent from the relay allowance.

## 7. Lifecycle

- **One relay task per process.** It is created next to `process_engine`, owned by the Rust runtime, and fed by settings. The Tauri UI and the Android service share it, so there are never two MQTT connections.
- **Desktop.** The relay runs while the app is open.
- **Android.** The relay runs while the app process lives. The connectedDevice foreground service keeps the process alive only while it runs (3A/3B). Whether to add the `remoteMessaging` foreground service type for MQTT-only use is decided in phase 3. **No promise of background delivery** after a swipe-away, force stop or battery restriction, and the UI says so.
- **Disable.** Disconnects, stops reconnects and keeps settings. *Remove settings* also wipes the broker password and all relay secrets, and sends a `RelayRevoke` to active contacts where a transport is up.

## 8. Test plan

- **Core unit tests:**
  - topic/alias vectors (stable across releases, computed independently)
  - the two directions differ
  - offer/accept/revoke body round trip and rejection of bad lengths and usernames
  - `confirm` mismatch
  - `Caps` with and without the reply flag, read by old and new decoders
- **Engine tests (mock transports):**
  - relay frame accepted only on known topics
  - wrong topic, wrong key or swapped direction rejected
  - LoRa + relay copies give one message and one notification
  - PUBACK never reaches `peer_ack`
  - a forged or v1 AppAck cannot mark delivered
  - a blocked peer's topic is unsubscribed and its frames dropped
  - routing table (§6) exhaustively, including a radio that comes back after relay attempts
  - per-route ACK allowance: relay replays leave the LoRa ACK allowance intact
  - exchange state machine: crossed offers, reordered and duplicate accept/revoke, accept for an old generation, broker tag mismatch, crash between persist and ACK (re-delivery is idempotent)
  - block/unpair cancels queued messages on both routes; a late attempt result for an old generation changes nothing; re-pairing does not send old messages
- **Integration (`dev/mqtt/`, Docker Compose, Mosquitto 2):**
  - setup: a test-only CA generated at run time, two users with the family ACL from §5, TLS listener on localhost only, persistence on
  - a user cannot subscribe to another inbox, or publish under another sender segment
  - publish/subscribe and QoS 1
  - `retain = false`
  - session resume while offline
  - bad CA and wrong hostname fail closed
  - bad password gives `auth_failed`
  - oversize payload rejected
  - These tests are `#[ignore]` and run with `dev/mqtt/run-tests.sh`. No broker is deployed anywhere else.
- **Devices** (user, phase 3): two phones and two T1000-E, covering LoRa only, MQTT only (radios off), mixed, airplane mode, broker down, and a regression of plain LoRa chat.

## 9. Milestones

| Phase | PR | Content | Status |
|---|---|---|---|
| P0 | this | ADR-006, audit, threat model | Proposed |
| P1a | — | `meshguard-core::relay`: secrets, topic derivation, control message bodies, vectors (no wire use yet) | Planned |
| P1b | — | MQTT client (`rumqttc` v5 + rustls/ring), settings validation, status, Mosquitto harness and integration tests | Planned |
| P2 | — | Engine: cancel queued sends on block/unpair (generation), relay settings and vault-sealed state, `Caps` reply flag, types 0x20–0x22 and the exchange state machine, inbound relay path, per-route ACKs, router in the outbox | Planned |
| P3 | — | UI (Settings → Internet Relay, per-contact control, *Sent to broker* vs *Delivered*), Android lifecycle, docs and website (*Experimental*), device tests | Planned |
| P4 | separate RFC | LoRa↔MQTT gateway | Not started |

## 10. Open questions

**Needs a decision before P2:**
- The default ACL mode (family, with the §5 deviation, or strict only).

**For later ADRs:**

- A stable app identity for radio-free contacts (ADR-007). It would change the AAD context through a versioned envelope.
- Relay-secret rotation without a full re-offer.
- Client-certificate (mTLS) authentication.
- WebSocket (`wss://`) transport for networks that block port 8883.
- Android foreground service type for MQTT-only operation (`remoteMessaging`), and whether to require it.
