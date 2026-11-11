#!/usr/bin/env bash
# Short-lived region comparison: creates a temporary e2-standard-2 VM in each
# region, measures RPC/sender HTTPS RTT (no transactions, no SOL), prints
# p50/p95, then DELETES the VMs. Expected cost: a few cents per run.
#   REGIONS="europe-west3-a asia-southeast1-b" TARGETS="https://api.mainnet-beta.solana.com https://mainnet.block-engine.jito.wtf" region-ab.sh
set -euo pipefail
source "$(dirname "$0")/common.sh"
REGIONS="${REGIONS:-europe-west3-a asia-southeast1-b}"
TARGETS="${TARGETS:-https://api.mainnet-beta.solana.com https://frankfurt.mainnet.block-engine.jito.wtf https://singapore.mainnet.block-engine.jito.wtf}"
confirm "Create temporary VMs in: $REGIONS (deleted at the end)?"
for z in $REGIONS; do
  n="trader-ab-${z//[^a-z0-9]/}"
  gc compute instances create "$n" --zone "$z" --machine-type e2-standard-2 --image-family debian-12 --image-project debian-cloud --labels app=trader,purpose=region-ab --quiet >/dev/null
  trap 'gc compute instances delete '"$n"' --zone '"$z"' --quiet >/dev/null 2>&1 || true' EXIT
  sleep 30
  echo "== $z"
  for t in $TARGETS; do
    gc compute ssh "$n" --zone "$z" --tunnel-through-iap --quiet -- "for i in \$(seq 1 30); do curl -s -o /dev/null -w '%{time_connect} %{time_starttransfer}\n' -X POST -H 'content-type: application/json' -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}' $t; done | sort -n | awk '{a[NR]=\$2} END {printf \"%s p50=%.1fms p95=%.1fms n=%d\n\", \"$t\", a[int(NR*0.5)]*1000, a[int(NR*0.95)]*1000, NR}'" || echo "$t unreachable"
  done
  gc compute instances delete "$n" --zone "$z" --quiet >/dev/null
  trap - EXIT
done
echo "Note: HTTPS RTT to an endpoint is not landing latency. Use trader-bench + smoke runs for that."
