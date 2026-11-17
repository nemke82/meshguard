# Internet relay: running your own MQTT broker

MeshGuard's internet relay (ADR-006) is **off by default**. It sends the same end-to-end encrypted packets as LoRa through an MQTT broker.

- **One tap:** the **MeshGuard relay** at `relay.meshguard.net` ([ADR-007](adr/007-managed-relay.md); server kit in [`server/relay`](../server/relay)).
- **Your own broker:** this guide.

This guide sets up Mosquitto 2 for a family or a small group.

## What the broker can and cannot see

- **Cannot:** read messages, check-ins, SOS or locations. They are encrypted on the phone with the chat key, exactly as on LoRa.
- **Can see:**
  - client IP addresses and connection times
  - the broker usernames
  - which usernames exchange packets
  - packet sizes and timing
- **Can store:** encrypted packets for offline phones, until they expire. The offline period set in the app is 1–7 days.

## Requirements

- A host reachable from the internet, with a DNS name.
- A **TLS certificate** for that name, for example from Let's Encrypt. MeshGuard refuses plain `mqtt://` and does not skip certificate checks. A private CA works too: paste its certificate in the app.
- **One account per person.** Never share one account between people.
- **Anonymous access off**, and only the TLS port open.

## Accounts

Each phone shows a generated username in **Settings → Internet relay → Generate**, such as `mg-k3v7q2x9p4m8n6r5t2wz`. It is random on purpose, so topic names never carry a name. Create the account with that exact name:

```bash
mosquitto_passwd /etc/mosquitto/passwd mg-k3v7q2x9p4m8n6r5t2wz
```

Give the person their password over a secure channel. They type it into the app, which stores it encrypted on the device.

## Configuration

`/etc/mosquitto/conf.d/meshguard.conf`:

```
per_listener_settings false
allow_anonymous false
password_file /etc/mosquitto/passwd
acl_file /etc/mosquitto/meshguard.acl

listener 8883
certfile /etc/letsencrypt/live/broker.example.org/fullchain.pem
keyfile  /etc/letsencrypt/live/broker.example.org/privkey.pem
tls_version tlsv1.2

persistence true
max_packet_size 4096
max_queued_messages 1000
persistent_client_expiration 7d
```

### Access rules

**Family ACL (default).** This needs no change on the broker when two people start using the relay.

```
# /etc/mosquitto/meshguard.acl
pattern read  mg/r1/%u/#
pattern write mg/r1/+/%u/#
```

- Each user can read only their own inbox (`mg/r1/<their name>/…`).
- Each user can publish only with their own name as the sender.
- A member of the broker could still drop junk into another member's inbox under their own name. The app ignores it, and the broker log shows who sent it.
- This is a deliberate trade-off against per-conversation rules (ADR-006 §5). Use it for people you trust.

**Strict ACL.** List the exact topics of each conversation. When the relay is on with a contact, the app can show the three lines for that user (`relay_acl_lines`), for example:

```
user mg-aaaaaaaaaaaaaaaaaaaa
topic write mg/r1/mg-bbbbbbbbbbbbbbbbbbbb/mg-aaaaaaaaaaaaaaaaaaaa/<alias>
topic read  mg/r1/mg-aaaaaaaaaaaaaaaaaaaa/mg-bbbbbbbbbbbbbbbbbbbb/<alias>
```

An administrator then has to add lines for every new relay conversation.

## Using it

1. Both people enter the **same broker** and their own username and password under **Settings → Internet relay**.
2. Tap **Test connection**, then turn **Use internet relay** on.
3. Pair over the radio as usual (QR code or passphrase), if you haven't already.
4. In the chat, tap the globe and offer the relay. The other person taps the globe and accepts. Both phones must use the same broker.
5. Send a test message.
   - ☁ means the broker accepted it.
   - ✓✓ means your contact's app received and decrypted it.

## Limits

- **Internet needed:** the relay needs internet on both phones. During an outage, MeshGuard keeps using LoRa wherever there is radio coverage.
- **Background on Android:** the relay runs while the app process runs. Android may stop it when the app is in the background or swiped away. Messages then wait at the broker (up to the offline period) and in the sender's outbox.
- **Pairing:** the relay only extends chats that were paired over the radio. It is not a way to find or pair contacts over the internet.

## Testing locally

`dev/mqtt/run-tests.sh` starts a throw-away Mosquitto with TLS, accounts and the family ACL on `127.0.0.1`. It runs the client's integration tests. **Never expose that broker or reuse its settings.**
