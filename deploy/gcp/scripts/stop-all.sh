#!/usr/bin/env bash
# EMERGENCY / COST STOP. Stops every VM labeled app=trader (compute billing
# stops; disk, static IP and snapshots keep billing small amounts).
#   stop-all.sh            stop VMs
#   stop-all.sh --delete   snapshot disks, then DELETE VMs and release the static IP
# Pending signed transactions may still land on-chain; on next start the
# engine reconciles them (docs/RUNBOOK.md).
set -euo pipefail
source "$(dirname "$0")/common.sh"
mapfile -t VMS < <(gc compute instances list --filter="labels.app=trader" --format="value(name,zone.basename())")
if [[ ${#VMS[@]} -eq 0 ]]; then echo "no trader VMs found"; exit 0; fi
printf '  %s\n' "${VMS[@]}"
if [[ "${1:-}" == "--delete" ]]; then
  confirm "SNAPSHOT then DELETE the VMs above and release the static IP? This is irreversible for the VMs."
  for row in "${VMS[@]}"; do
    read -r name zone <<<"$row"
    gc compute disks snapshot "$name" --zone "$zone" --snapshot-names "${name}-final-$(date +%Y%m%d%H%M)" || true
    gc compute instances delete "$name" --zone "$zone" --quiet
  done
  gc compute addresses delete trader-ip --region "${GCP_ZONE%-*}" --quiet || true
else
  confirm "STOP the VMs above?"
  for row in "${VMS[@]}"; do
    read -r name zone <<<"$row"
    gc compute instances stop "$name" --zone "$zone"
  done
fi
echo "done. Verify in Billing > Reports within 24h (billing data lags)."
