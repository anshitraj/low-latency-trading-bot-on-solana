#!/usr/bin/env bash
# Build the image locally, ship it to the VM over IAP, install unit + Caddy,
# restart. Rollback: `ROLLBACK=1 deploy.sh` re-tags the previous image.
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$(dirname "$0")/../../.."
TAG="trader-api:$(git rev-parse --short HEAD)"
if [[ "${ROLLBACK:-}" == "1" ]]; then
  confirm "Roll back $VM to the previous image (trader-api:previous)?"
  gc compute ssh "$VM" --zone "$GCP_ZONE" --tunnel-through-iap -- "sudo docker tag trader-api:previous trader-api:latest && sudo systemctl restart trader-api"
  exit 0
fi
confirm "Build $TAG and deploy to $VM ($GCP_ZONE)? Orders in flight are reconciled on restart."
docker build -t "$TAG" .
docker save "$TAG" | gzip > /tmp/trader-image.tgz
gc compute scp --zone "$GCP_ZONE" --tunnel-through-iap /tmp/trader-image.tgz deploy/gcp/vm/docker-compose.prod.yml deploy/gcp/vm/trader-api.service deploy/gcp/vm/fetch-secrets.sh deploy/gcp/vm/Caddyfile "$VM":/tmp/
gc compute ssh "$VM" --zone "$GCP_ZONE" --tunnel-through-iap -- "set -e
  sudo mkdir -p /opt/trader && sudo cp /tmp/docker-compose.prod.yml /tmp/fetch-secrets.sh /opt/trader/ && sudo chmod 700 /opt/trader/fetch-secrets.sh
  sudo cp /tmp/trader-api.service /etc/systemd/system/ && sudo systemctl daemon-reload
  [ -f /etc/caddy/Caddyfile.custom ] || sudo cp /tmp/Caddyfile /etc/caddy/Caddyfile
  (sudo docker image inspect trader-api:latest >/dev/null 2>&1 && sudo docker tag trader-api:latest trader-api:previous) || true
  gunzip -c /tmp/trader-image.tgz | sudo docker load && sudo docker tag $TAG trader-api:latest
  sudo systemctl enable trader-api && sudo systemctl restart trader-api && sudo systemctl reload caddy || true
  sleep 5 && curl -fsS http://127.0.0.1:8080/v1/health"
echo "deployed $TAG"
