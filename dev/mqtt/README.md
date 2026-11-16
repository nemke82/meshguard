# Local MQTT broker for relay tests

**For development and CI only. Never expose this broker or reuse its configuration in production.**

`run-tests.sh` starts a throw-away Mosquitto 2 broker in a container (podman or docker) and runs the `meshguard-mqtt` integration tests against it. The tests cover:
- delivery between two clients
- PUBACK being only a submission
- offline session resume
- no retained messages
- ACLs
- unsubscribe
- oversize frames
- wrong password
- unknown CA
- public roots
- hostname mismatch

```bash
dev/mqtt/run-tests.sh
```

Each run generates everything into a temp folder that is removed on exit:
- a test CA
- a server certificate for `localhost` only
- an unrelated CA
- random passwords for three users

The broker listens on `127.0.0.1` only (port 18883, or `MG_MQTT_PORT`).

- `mosquitto.conf`: TLS-only listener, no anonymous access, password file, ACL file, persistence.
- `acl`: the *family* ACL from ADR-006 §5. A user reads only `mg/r1/<self>/#` and writes only `mg/r1/+/<self>/#`.

A production broker needs at least the following (see ADR-006 §5):
- a real certificate
- per-person accounts with the app-generated `mg-…` usernames
- `allow_anonymous false`
- a firewall
- updates
