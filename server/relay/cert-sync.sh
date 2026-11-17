#!/usr/bin/env bash
# Copy Caddy's Let's Encrypt certificate for the broker and make Mosquitto
# reload it (SIGHUP reloads TLS certificates; sessions are kept).
set -euo pipefail
DIR="${RELAY_DIR:-/opt/meshguard-relay}"
HOST="${RELAY_HOST:-relay.meshguard.net}"
SRC="$DIR/data/caddy/caddy/certificates/acme-v02.api.letsencrypt.org-directory/$HOST"
DST="$DIR/data/certs"
[ -f "$SRC/$HOST.crt" ] && [ -f "$SRC/$HOST.key" ] || { echo "no certificate yet"; exit 1; }
mkdir -p "$DST"
if cmp -s "$SRC/$HOST.crt" "$DST/fullchain.pem" && cmp -s "$SRC/$HOST.key" "$DST/privkey.pem"; then
  exit 0
fi
install -m 0444 -o 1883 -g 1883 "$SRC/$HOST.crt" "$DST/fullchain.pem"
install -m 0400 -o 1883 -g 1883 "$SRC/$HOST.key" "$DST/privkey.pem"
echo "certificate updated"
docker kill -s HUP relay-mosquitto >/dev/null 2>&1 || true
