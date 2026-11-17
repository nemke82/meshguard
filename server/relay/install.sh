#!/usr/bin/env bash
# Install or update the MeshGuard relay on a Debian 13 host (ADR-007).
# Run as root from this folder. Safe to run again: it keeps existing
# secrets, accounts and certificates.
#
# It expects: DNS for relay.meshguard.net pointing here, and ports 80, 443
# and 8883 open in the firewall (see server/relay/README.md).
set -euo pipefail
cd "$(dirname "$0")"
DIR=/opt/meshguard-relay
HOST="${RELAY_HOST:-relay.meshguard.net}"

[ "$(id -u)" = 0 ] || { echo "run as root"; exit 1; }

echo "== packages"
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io docker-compose rsync >/dev/null
COMPOSE="docker compose"; $COMPOSE version >/dev/null 2>&1 || COMPOSE="docker-compose"

echo "== docker: host networking only, no firewall changes, bounded logs"
mkdir -p /etc/docker
cat > /etc/docker/daemon.json <<'J'
{
  "iptables": false,
  "ip6tables": false,
  "log-driver": "local",
  "log-opts": { "max-size": "10m", "max-file": "3" }
}
J
systemctl restart docker

echo "== no swap, no core dumps (queued ciphertext stays in RAM only)"
swapoff -a
sed -i -E 's@^([^#].*\sswap\s.*)$@# \1  # disabled for the MeshGuard relay@' /etc/fstab
cat > /etc/sysctl.d/90-meshguard-relay.conf <<'S'
kernel.core_pattern = |/bin/false
fs.suid_dumpable = 0
S
sysctl -q --system
mkdir -p /etc/systemd/coredump.conf.d
printf '[Coredump]\nStorage=none\nProcessSizeMax=0\n' > /etc/systemd/coredump.conf.d/90-meshguard-relay.conf

echo "== files"
mkdir -p "$DIR"
rsync -a --delete --exclude data --exclude .env --exclude enroll/target ./ "$DIR/"
cd "$DIR"
if [ ! -f .env ]; then
  umask 077
  printf 'DYNSEC_ADMIN_PASSWORD=%s\n' "$(openssl rand -hex 24)" > .env
fi
chmod 600 .env
mkdir -p data/caddy data/caddy-config data/mosquitto data/enroll data/certs
chown 1883:1883 data/mosquitto data/certs
chown 1884:1884 data/enroll
chmod 700 data/mosquitto data/enroll

echo "== build images"
$COMPOSE build -q

echo "== certificate (Caddy, Let's Encrypt)"
$COMPOSE up -d caddy
for _ in $(seq 1 90); do
  ./cert-sync.sh >/dev/null 2>&1 && break
  sleep 2
done
./cert-sync.sh || { echo "no certificate: check DNS for $HOST and ports 80/443"; exit 1; }

echo "== broker and enrollment"
$COMPOSE up -d mosquitto enroll

echo "== daily certificate sync"
cat > /etc/systemd/system/meshguard-cert-sync.service <<U
[Unit]
Description=Copy the relay certificate to Mosquitto
[Service]
Type=oneshot
ExecStart=$DIR/cert-sync.sh
U
cat > /etc/systemd/system/meshguard-cert-sync.timer <<'U'
[Unit]
Description=Daily relay certificate sync
[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true
[Install]
WantedBy=timers.target
U
systemctl daemon-reload
systemctl enable --now meshguard-cert-sync.timer >/dev/null

echo "== status"
$COMPOSE ps
echo "done: https://$HOST/v1/challenge and mqtts://$HOST:8883"
