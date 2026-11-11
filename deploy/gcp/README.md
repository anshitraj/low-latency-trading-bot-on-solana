# deploy/gcp

Single-VM private-beta deployment. Read `docs/GCP_DEPLOYMENT.md` and
`docs/COST_AND_SHUTDOWN.md` first. Nothing here has been applied.

```
terraform/            plan-only until the owner approves apply
vm/                   startup script, systemd unit, compose file, Caddyfile, secret fetcher
scripts/deploy.sh     build + ship image over IAP (ROLLBACK=1 to roll back)
scripts/stop-all.sh   stop (or --delete) every app=trader VM — emergency/cost stop
scripts/backup-db.sh  encrypted pg_dump; --restore drill
scripts/region-ab.sh  temporary two-region RTT comparison (deletes its VMs)
```
All scripts require `GCP_PROJECT` and ask for typed confirmation (`ASSUME_YES=1` to skip in automation).
