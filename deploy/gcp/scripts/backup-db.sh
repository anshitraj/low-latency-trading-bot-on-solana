#!/usr/bin/env bash
# Encrypted logical backup of Postgres to a local file (age/gpg optional) and
# restore drill. Disk snapshots (daily, 7 days) are the second layer.
#   backup-db.sh            -> ./backups/trader-<ts>.sql.gz.enc
#   backup-db.sh --restore FILE   restore into a scratch DB and run sanity checks
set -euo pipefail
source "$(dirname "$0")/common.sh"
mkdir -p backups
: "${BACKUP_PASSPHRASE_FILE:?set BACKUP_PASSPHRASE_FILE (file with a strong passphrase, kept offline)}"
if [[ "${1:-}" == "--restore" ]]; then
  f="${2:?file}"
  confirm "Restore $f into scratch database trader_restore on $VM (production DB untouched)?"
  openssl enc -d -aes-256-cbc -pbkdf2 -pass file:"$BACKUP_PASSPHRASE_FILE" -in "$f" | gc compute ssh "$VM" --zone "$GCP_ZONE" --tunnel-through-iap -- \
    "sudo docker compose -f /opt/trader/docker-compose.prod.yml exec -T postgres sh -c 'dropdb -U trader --if-exists trader_restore && createdb -U trader trader_restore && gunzip | psql -q -U trader trader_restore && psql -U trader trader_restore -c \"select count(*) orders from orders; select count(*) events from order_events;\"'"
  exit 0
fi
ts=$(date +%Y%m%dT%H%M%S)
gc compute ssh "$VM" --zone "$GCP_ZONE" --tunnel-through-iap -- \
  "sudo docker compose -f /opt/trader/docker-compose.prod.yml exec -T postgres pg_dump -U trader trader | gzip" \
  | openssl enc -aes-256-cbc -pbkdf2 -salt -pass file:"$BACKUP_PASSPHRASE_FILE" -out "backups/trader-$ts.sql.gz.enc"
echo "backup: backups/trader-$ts.sql.gz.enc ($(du -h "backups/trader-$ts.sql.gz.enc" | cut -f1))"
