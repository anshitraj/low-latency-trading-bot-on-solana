#!/usr/bin/env bash
# VM bootstrap (Debian 12). Idempotent. Installs Docker + Caddy, enables NTP,
# pulls secrets at service start (never written to disk in plaintext beyond tmpfs).
set -euo pipefail
if ! command -v docker >/dev/null; then
  apt-get update
  apt-get install -y --no-install-recommends ca-certificates curl gnupg chrony debian-keyring debian-archive-keyring apt-transport-https
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" > /etc/apt/sources.list.d/docker.list
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update
  apt-get install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin caddy
fi
systemctl enable --now chrony docker
# Bound local logs
mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=500M\nMaxRetentionSec=7day\n' > /etc/systemd/journald.conf.d/trader.conf
systemctl restart systemd-journald
mkdir -p /opt/trader
