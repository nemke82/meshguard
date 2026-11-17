# ADR-007: MeshGuard relay at relay.meshguard.net (managed broker, self-enrollment)

**Status:** Proposed (2026-11-17)
**Builds on:** [ADR-006](006-private-mqtt-relay.md) (Private Encrypted MQTT relay)

## Context

ADR-006 shipped the relay as bring-your-own broker: someone has to run Mosquitto, create accounts and type broker details into both phones. In practice that blocks most people.

A broker *inside* the app does not help on its own. Phones on mobile data sit behind carrier-grade NAT and cannot accept connections, so two phones cannot reach each other's built-in broker without a public machine in between.

The project owner will therefore run one public broker: **`relay.meshguard.net`**. The goals:

- **One tap:** the app enrolls itself, with no invites and nothing to type. Anyone who installed MeshGuard can use it, between contacts they paired over the radio.
- **Nothing kept:** messages self-destruct on the relay. The only lasting copy is the encrypted history on the phones.
- **LoRa unaffected:** LoRa keeps working whatever the relay does, and the relay stays off until the user turns it on.

ADR-006 still applies unchanged:
- the same MG2 frames and keys
- the offer/accept exchange
- topics and routing
- truthful delivery states
- PUBACK means *sent to broker* only

This ADR adds the managed broker, self-enrollment and retention, and changes the privacy statement.

## Decision

### 1. One-tap setup in the app

*Settings → Internet relay* gets two choices:
- **MeshGuard relay (relay.meshguard.net)**: the default choice, still off until the user turns it on.
- **Your own broker**: the ADR-006 fields, unchanged.

Choosing the MeshGuard relay:
1. The app generates its username (`mg-` plus 20 base32 characters, as in ADR-006) and a random 128-bit password from the OS CSPRNG.
2. It enrolls (§3) over HTTPS, then stores the settings with the password sealed in the relay book.
3. It connects with `mqtts://relay.meshguard.net:8883` and full certificate checks against the public roots (Let's Encrypt).

The broker tag (ADR-006 §4) of everyone on the MeshGuard relay is the same, so contacts who both chose it can accept each other's offers.

### 2. Broker (`server/relay/`)

- **Mosquitto 2.1 or newer, pinned to an exact image tag.**
  - It has the **Dynamic Security** plugin, so clients are added at runtime without a restart. Version 2.1 also adds `%u` substitution in dynsec roles, which 2.0 lacks.
- **Listeners:**
  - The TLS listener `8883` is the only public MQTT listener.
  - A local admin listener on `127.0.0.1` is used by the enrollment service.
  - No anonymous access.
- **Role `meshguard-user`.** Its ACL rules have explicit types, and every default is *deny*:
    - `subscribePattern mg/r1/%u/#` allow
  - `unsubscribePattern mg/r1/%u/#` allow (revoked topics must be removable from a resumed session)
  - `publishClientReceive mg/r1/%u/#` allow
  - `publishClientSend mg/r1/+/%u/#` allow
    - Tested: a user cannot read other inboxes, cannot use wildcards outside its own inbox, and cannot publish under another sender name, but can unsubscribe.
- **One session per account.** Each dynsec client is bound to a single MQTT client id, equal to its username. One account therefore has at most one session and one queue; a second connection takes the first one over. The app uses that id for the managed relay. *Test connection* is not offered there, because enrollment proves the account.
- **`meshguard-limits` plugin.** It is required before public rollout. It is a small Mosquitto plugin in `server/relay/plugin`, tested against the pinned version. It enforces what the configuration cannot:
  - **Lifetime clamp:** every incoming publication's message expiry is clamped to **at most 24 hours**, and set to 24 hours when missing. So no frame is ever *delivered* more than 24 hours after it was published.
  - **Subscriptions:** at most **64 subscriptions per client**. More SUBSCRIBEs are refused.
  - **Publish rate:** at most **one publish per second** per client, with a small burst (e.g. 20). Excess publishes are refused.
- **What the broker keeps, and for how long:**
  - **No retained messages** (`retain_available false`).
  - **No message database on disk** (`persistence false`).
  - **Online recipient:** a frame is forwarded and not kept.
      - **Offline recipient:** the frame waits **in memory only**. It stops being *deliverable* at the first of these, and never later than 24 hours (the plugin's clamp):
    - it is delivered
        - its message expiry passes (set by the sender's app, 1 h text, 6 h SOS, 24 h relay controls; never more than the 24-hour clamp)
    - the recipient's session expires, 3 days after its last disconnection (`persistent_client_expiration 3d`)
    - the per-client queue limit pushes it out
    - the broker restarts
    - A queue full of unexpired frames refuses new ones; it does not evict older ones.
    - Expiry ends *delivery*, not memory use. An expired frame can stay allocated until Mosquitto next processes that client's queue (at reconnection) or drops the session, at most 3 days after the client's last disconnection. Freed memory is not wiped. It is never written to disk.
- **Resource limits:**
  - `max_packet_size 4096`
  - `max_queued_messages 200` per client
  - `max_inflight_messages 20`
    - `global_max_connections`
  - the plugin's subscription and rate limits above
- **Host hardening:**
  - The firewall (nftables/ufw) allows only 22, 80 (certificate renewal), **443 (enrollment)** and 8883.
  - Swap is off or encrypted, and core dumps are off.
  - The reverse proxy writes no access logs.
  - The broker logs errors and warnings only; it has no connection log.
- **Certificate:** Let's Encrypt for `relay.meshguard.net`, renewed automatically. Mosquitto reloads it after renewal.

### 3. Enrollment service

This is a small HTTPS service on the same host, `https://relay.meshguard.net/v1/`, behind the TLS reverse proxy on 443.

- `GET /v1/challenge` → `{nonce, difficulty, expires}`. The nonce is random, the challenge is valid for 2 minutes and can be used once.
- `POST /v1/enroll {username, password, nonce, solution}`:
  - **Proof of work:** `SHA-256(nonce ‖ username ‖ solution)` must have `difficulty` leading zero bits. The default of 20 bits is about a second on a phone.
  - **Username:** it must have the app form.
  - **Password:** it must be 128 random bits, i.e. 32 hex characters, generated by the app.
    - **New username:** the service creates the account as described under *Crash safety* below, with role `meshguard-user` and the client id bound. It stores a verifier `SHA-256("meshguard-relay-v1" ‖ password)`; the password has 128 random bits, so a plain hash is enough. Dynsec keeps its own salted hash for MQTT logins.
  - **Existing username:** the request counts as a *renewal* only if the verifier matches; otherwise it is refused (409). An existing account can never have its password reset through this endpoint.
- `DELETE /v1/account {username, password}` is idempotent:
  - If the account exists, it is deleted only if the verifier matches; otherwise the answer is 403.
  - If the username is unknown (already deleted, a lost success response, or expired), the answer is **200 `gone`**.
  - A wrong password on an existing account never reaches `gone`. A `gone` answer reveals only that a random username does not exist.
- **Crash safety.** Each account has a durable state in SQLite: `creating` → `active` → `deleting`. Every change runs under one lock: create, renew, delete and the expiry sweep.
  - **Create:**
    1. Insert the row as `creating`, with the verifier.
    2. Create the dynsec client, then mark the row `active`.
    3. Only then answer.
  - **Delete:**
    1. Mark the row `deleting`.
    2. Delete the dynsec client, then delete the row.
  - **Reconciliation at start and hourly:**
    - A `creating` row is rolled back, together with its dynsec client if it exists. The service cannot finish it without the password, and the app's retry of the same enrollment simply creates it again.
    - A `deleting` row is finished.
    - Any dynsec client in the `meshguard-user` role without an `active` row is deleted.
  - Tests cover crashes between the steps, lost responses, and expiry before a retried delete.
- **Limits:** a few enrollments per IP per hour, a global cap per hour, and a maximum number of accounts. Over a limit the answer is 429 and the app shows *Try again later*.
- **Expiry:** accounts not renewed for 90 days are deleted. The app renews quietly about once a month while the relay is on.
- **Stored on the server:** for each account, the username, the dynsec salted password hash and role, the verifier and the last renewal date. IP addresses are counted in memory for the rate limits and never written.

### 4. Threat model, delta to ADR-006 §3

| Adversary | New exposure | Defense |
|---|---|---|
| Relay operator (the MeshGuard project) | Sees IPs, online times, random usernames, which usernames exchange packets, sizes and timing; holds queued ciphertext in memory | E2EE content (unchanged); random usernames carry no names; no message database on disk; no connection or access logs; documented in the privacy policy |
| Mass sign-up / bots | Account and memory exhaustion | Proof-of-work, per-IP and global limits, account cap, one session per account, 90-day expiry, small queues and packets, global connection cap |
| Another relay user | Junk into my inbox under their own name | The app subscribes only to exact topics of its contacts; frames fail AEAD; the operator can delete the account |
| Stolen verifier database | Cannot log in or derive passwords | The verifier is a hash of 128 random bits; the login hash is separate (dynsec) |
| Relay down or blocked | No internet delivery | LoRa continues; outbox retries; "Your own broker" still possible |
| Seized or compromised server | Live metadata, queued ciphertext in memory (hours) | No message database on disk; content needs the chat keys, which never leave the phones |

**Residual risk.** The relay is one operator and one place on the internet. It can see who talks to whom, by random name and IP, while they are online, and it can be blocked or go down. The privacy policy and the in-app text say so.

### 5. Privacy and store listing

- **Privacy policy.** It changes from "we run no server" to: "We run an optional relay (relay.meshguard.net) that you can turn on. It forwards end-to-end encrypted packets between contacts. Message packets are never written to disk. A packet for a phone that is offline waits in the relay's memory and is never delivered more than 24 hours after it was sent. An expired packet can stay in memory until the phone reconnects, or for at most 3 days if it stays offline. Packets are encrypted end to end throughout. While you use the relay, it sees your IP address and connection times, your random relay username, and which relay usernames exchange packets. It never sees message content. It stores your random username, password hashes and the date of your last renewal. Removing the relay in the app deletes that account; unused accounts are deleted after 90 days."
- **Google Play Data safety.** It declares IP address and device/other IDs (the random relay username), processed for app functionality and not shared, encrypted in transit, with deletion through *Remove*.

### 6. App changes and account lifecycle

- **Settings choice:** MeshGuard relay or your own broker.
- **Enable (first time):**
  1. Generate a username and password, and seal them in the relay book first.
  2. Challenge, proof-of-work on a background thread, enroll.
  3. Save, connect.
  - If enrollment fails, the relay stays off with the reason shown, and the credentials are kept for the next try.
- **Renewal:** about once a month while enabled, with the same credentials.
- **Turn off (switch):** disconnects only. The account and the relay contacts stay, as in ADR-006, so turning it back on needs nothing new.
- **Remove:**
  1. Contacts are revoked (ADR-006).
  2. The account is marked `pending_delete`, keeping its username and password in the sealed book.
  3. `DELETE /v1/account` is retried until the relay confirms.
  4. Only then are the credentials wiped.
  - The UI shows *Deleting relay account…* until the relay confirms. If it never does, the 90-day expiry deletes the account anyway.
- **Enable after Remove:** a new username and password; contacts need a new offer and accept.
- **Broker tag:** contacts on the MeshGuard relay share it (same host and port), so offer and accept work as before.

## Consequences

- Most people get internet range with one tap and no setup, and LoRa stays the fallback.
- The project now runs a public service: uptime, abuse handling, updates and costs.
- With no message persistence, a broker restart loses queued messages. The senders' outboxes retry within the message lifetime, and the phones keep the history.
- The managed relay depends on Mosquitto 2.1 features (dynsec `%u`, client-id binding). Upgrades are pinned and tested with `dev/mqtt`-style integration tests against the same configuration.

## Not decided here

- High availability or a second region.
- Enrollment without proof-of-work (Play Integrity, etc.), rejected for now for privacy.
- Invites or private groups on the managed relay.
