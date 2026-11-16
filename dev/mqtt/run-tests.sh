#!/usr/bin/env bash
# Run the MQTT relay integration tests against a throw-away local
# Mosquitto 2 broker (ADR-006 §8).
#
# Everything secret is generated here, at run time, into a temp folder:
# a test CA, a server certificate for "localhost" only, a second unrelated
# CA (for the bad-CA test) and random passwords for three users. Nothing is
# committed, nothing listens on anything but 127.0.0.1, and the container
# and folder are removed on exit.
#
# Needs: podman or docker, openssl, cargo.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
IMAGE="docker.io/library/eclipse-mosquitto:2.0.20"
PORT="${MG_MQTT_PORT:-18883}"
NAME="meshguard-mqtt-test-$$"

if command -v podman >/dev/null 2>&1; then RUN=podman
elif command -v docker >/dev/null 2>&1; then RUN=docker
else echo "podman or docker is required" >&2; exit 1; fi

GEN="$(mktemp -d)"
chmod 0755 "$GEN"
cleanup() {
  "$RUN" rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$GEN"
}
trap cleanup EXIT

# ── Test PKI (throw-away) ─────────────────────────────────────
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=MeshGuard test CA" \
  -keyout "$GEN/ca.key" -out "$GEN/ca.crt" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout "$GEN/server.key" -out "$GEN/server.csr" 2>/dev/null
# DNS name only: connecting by IP must fail the hostname check.
printf 'subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > "$GEN/ext"
openssl x509 -req -in "$GEN/server.csr" -CA "$GEN/ca.crt" -CAkey "$GEN/ca.key" \
  -CAcreateserial -days 1 -extfile "$GEN/ext" -out "$GEN/server.crt" 2>/dev/null
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=Unrelated CA" \
  -keyout "$GEN/other.key" -out "$GEN/other.crt" 2>/dev/null
chmod 0644 "$GEN"/*.key

# ── Users with random passwords ───────────────────────────────
pw() { openssl rand -hex 16; }
export MG_MQTT_USER_A="mg-testa" MG_MQTT_PASS_A="$(pw)"
export MG_MQTT_USER_B="mg-testb" MG_MQTT_PASS_B="$(pw)"
export MG_MQTT_USER_C="mg-testc" MG_MQTT_PASS_C="$(pw)"
: > "$GEN/passwd"
chmod 0644 "$GEN/passwd"
for u in A B C; do
  user="MG_MQTT_USER_$u"; pass="MG_MQTT_PASS_$u"
  "$RUN" run --rm -v "$GEN:/gen:Z" "$IMAGE" \
    mosquitto_passwd -b /gen/passwd "${!user}" "${!pass}" >/dev/null 2>&1
done

# ── Broker ────────────────────────────────────────────────────
"$RUN" run -d --name "$NAME" -p "127.0.0.1:$PORT:8883" \
  -v "$HERE/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro,Z" \
  -v "$HERE/acl:/mosquitto/config/acl:ro,Z" \
  -v "$GEN:/mosquitto/gen:ro,Z" \
  "$IMAGE" >/dev/null
for _ in $(seq 1 50); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then break; fi
  sleep 0.2
done

export MG_MQTT_PORT="$PORT" MG_MQTT_CA="$GEN/ca.crt" MG_MQTT_OTHER_CA="$GEN/other.crt"
cd "$ROOT/crates/meshguard-mqtt"
status=0
cargo test --test broker -- --ignored --test-threads=1 "$@" || status=$?
if [ "$status" -ne 0 ]; then
  echo "── broker log ──"; "$RUN" logs "$NAME" 2>&1 | tail -40
fi
exit "$status"
