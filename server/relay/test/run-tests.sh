#!/usr/bin/env bash
# Live tests of the relay server kit (ADR-007): builds the patched Mosquitto
# image, runs it with the real mosquitto.conf and a throw-away certificate for
# "localhost", runs the enrollment service, then the tests in
# enroll/tests/live.rs. Everything listens on 127.0.0.1 only and is removed
# on exit. Needs podman or docker (MG_CONTAINER picks), openssl, cargo.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
KIT="$(cd "$HERE/.." && pwd)"
if [ -n "${MG_CONTAINER:-}" ]; then RUN="$MG_CONTAINER"
elif command -v podman >/dev/null; then RUN=podman; else RUN=docker; fi

GEN="$(mktemp -d)"; chmod 0755 "$GEN"
NAME="meshguard-relay-test-$$"
ENROLL_PID=""
cleanup() {
  [ -n "$ENROLL_PID" ] && kill "$ENROLL_PID" 2>/dev/null || true
  "$RUN" rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$GEN"
}
trap cleanup EXIT

# Throw-away PKI: a CA and a certificate for "localhost".
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=relay test CA" \
  -keyout "$GEN/ca.key" -out "$GEN/ca.crt" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout "$GEN/privkey.pem" -out "$GEN/server.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > "$GEN/ext"
openssl x509 -req -in "$GEN/server.csr" -CA "$GEN/ca.crt" -CAkey "$GEN/ca.key" \
  -CAcreateserial -days 1 -extfile "$GEN/ext" -out "$GEN/fullchain.pem" 2>/dev/null
mkdir -p "$GEN/certs" "$GEN/data" && chmod 0777 "$GEN/data"
cp "$GEN/fullchain.pem" "$GEN/privkey.pem" "$GEN/certs/" && chmod 0644 "$GEN/certs/"*
# The production config, with both listeners on loopback and test ports.
sed -e 's/^listener 8883$/listener 18883 127.0.0.1/' \
    -e 's/^listener 1884 127.0.0.1$/listener 11884 127.0.0.1/' \
  "$KIT/mosquitto/mosquitto.conf" > "$GEN/mosquitto.conf"
grep -q '^listener 18883 127.0.0.1$' "$GEN/mosquitto.conf" && grep -q '^listener 11884 127.0.0.1$' "$GEN/mosquitto.conf" \
  || { echo "test config did not rebind the listeners"; exit 1; }
chmod 0644 "$GEN/mosquitto.conf"
export DYNSEC_ADMIN_PASSWORD="$(openssl rand -hex 24)"

"$RUN" build -q -t meshguard-relay-mosquitto:test "$KIT/mosquitto" >/dev/null
"$RUN" run -d --name "$NAME" --network host \
  -e MOSQUITTO_DYNSEC_PASSWORD="$DYNSEC_ADMIN_PASSWORD" \
  -v "$GEN/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro,Z" \
  -v "$GEN/data:/mosquitto/data:Z" \
  -v "$GEN/certs:/mosquitto/certs:ro,Z" \
  meshguard-relay-mosquitto:test >/dev/null
for _ in $(seq 1 50); do (exec 3<>/dev/tcp/127.0.0.1/11884) 2>/dev/null && break; sleep 0.2; done
"$RUN" inspect -f '{{.State.Running}}' "$NAME" | grep -q true || { "$RUN" logs "$NAME"; exit 1; }

cd "$KIT/enroll"
cargo build -q
DB_PATH="$GEN/accounts.db" LISTEN=127.0.0.1:18080 POW_BITS=8 RUST_LOG=info BROKER_PORT=11884 \
  ENROLL_PER_IP_PER_HOUR=1000 ./target/debug/meshguard-relay-enroll >"$GEN/enroll.log" 2>&1 &
ENROLL_PID=$!
for _ in $(seq 1 300); do (exec 3<>/dev/tcp/127.0.0.1/18080) 2>/dev/null && break; sleep 0.2; done

export ENROLL_URL=http://127.0.0.1:18080 MQTT_PORT=18883 MQTT_CA="$GEN/ca.crt"
status=0
cargo test -q --test live -- --ignored --test-threads=1 "$@" || status=$?
if [ "$status" -ne 0 ]; then echo "── broker log ──"; "$RUN" logs "$NAME" 2>&1 | tail -30; echo "── enroll log ──"; tail -30 "$GEN/enroll.log"; fi
exit "$status"
