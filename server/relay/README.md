# MeshGuard relay server (relay.meshguard.net)

This is the server side of [ADR-007](../../docs/adr/007-managed-relay.md). It is a public MQTT broker that MeshGuard apps enroll with in one tap.

- **Messages are never written to disk.** Payloads are end-to-end encrypted by the app (ADR-006); the server never has the keys.
- **Retention:** no message is delivered more than 24 hours after it was sent.

| Part | What it does |
|---|---|
| `mosquitto/` | Mosquitto **2.1.2** built from the release tarball, checked by hash. It has two additions: `0001-meshguard-clamp-message-expiry.patch` caps every message's lifetime at 24 h (also when none is set), and the `meshguard_limits.c` plugin enforces 64 subscriptions per client and 1 publish/s with a burst of 20. Dynamic Security holds the accounts. Retained messages and persistence are off. |
| `enroll/` | The enrollment service (Rust): `GET /v1/challenge`, `POST /v1/enroll`, `DELETE /v1/account`. Proof-of-work, per-IP and global limits, an account cap, crash-safe account states and 90-day expiry. It logs no IPs or usernames. |
| `caddy/` | HTTPS for enrollment and the Let's Encrypt certificate (also used by the broker). No access logs. |
| `cert-sync.sh` | Copies the certificate to the broker and reloads it (SIGHUP). Runs daily. |
| `install.sh` | Installs or updates everything on Debian 13. |

## Requirements

- Debian 13, as root (or with sudo).
- DNS: `relay.meshguard.net` → the server.
- Firewall: 80 (certificate renewal), 443 (enrollment) and 8883 (MQTT over TLS) open; SSH only from where you administer.

## Install / update

```bash
sudo ./install.sh
```

The script:
- installs Docker
- configures it for host networking with no firewall changes
- turns swap and core dumps off
- copies the kit to `/opt/meshguard-relay`
- creates a random admin password in `.env` on the first run
- gets the certificate
- starts everything
- enables the daily certificate sync

Running it again updates the kit and keeps accounts, secrets and certificates.

## What is stored on the server

| Where | What |
|---|---|
| `data/mosquitto/dynsec.json` | Usernames (random, `mg-…`), salted password hashes, roles |
| `data/enroll/accounts.db` | Usernames, a hash of each password (to authorize renewal and deletion), the last renewal date |
| `data/caddy` | The TLS certificate |

Messages exist in memory only, and only for offline recipients. An expired message can stay in memory until its recipient reconnects, or at most 3 days. No connection or access logs are kept.

## Tests

```bash
server/relay/test/run-tests.sh
```

This builds the broker, runs it and the enrollment service on `127.0.0.1` with a throw-away certificate, and checks:
- inbox isolation and sender spoofing
- client-ID binding and wrong passwords
- renewal, conflict and deletion (which kicks the client off)
- the 24-hour cap
- the per-client limits

## Operating

```bash
cd /opt/meshguard-relay
docker compose ps
docker compose logs --tail 50 mosquitto enroll   # errors and counts only
```

**Delete an abusive account:** `docker exec relay-mosquitto` cannot run tools, so use the dynsec admin over the local listener (`127.0.0.1:1884`, user `admin`, password in `.env`), or remove its row and let reconciliation delete the client within the hour.
