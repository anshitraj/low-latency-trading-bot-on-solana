#!/usr/bin/env bash
# Boot-time preparation for the test RPC node (run by node-prep.service, root).
# - Local NVMe (wiped whenever the VM stops): RAID0 -> /mnt/solana (ledger, accounts).
# - Persistent disk "snapshots" -> /mnt/persist (snapshot archives survive a stop,
#   so a restart reuses them instead of downloading ~116 GB).
# Idempotent.
set -euo pipefail

# Persistent snapshot disk (attached with --device-name=snapshots).
PD=/dev/disk/by-id/google-snapshots
if [ -e "$PD" ]; then
  mkdir -p /mnt/persist
  if ! blkid "$PD" >/dev/null 2>&1; then mkfs.xfs -q -L solsnap "$PD"; fi
  mountpoint -q /mnt/persist || mount -o noatime,discard "$PD" /mnt/persist
  mkdir -p /mnt/persist/snap
  chown -R sol:sol /mnt/persist/snap
fi

# Local NVMe RAID0.
if ! mountpoint -q /mnt/solana; then
  if [ ! -e /dev/md0 ]; then
    mapfile -t DEVS < <(ls /dev/disk/by-id/google-local-nvme-ssd-* 2>/dev/null | grep -v part || true)
    [ "${#DEVS[@]}" -ge 2 ]
    mdadm --create /dev/md0 --level=0 --raid-devices="${#DEVS[@]}" "${DEVS[@]}" --force --run
  fi
  blkid /dev/md0 >/dev/null 2>&1 || mkfs.xfs -f -q /dev/md0
  mkdir -p /mnt/solana
  mount -o noatime,discard /dev/md0 /mnt/solana
fi
mkdir -p /mnt/solana/{ledger,accounts,snapshots}
chown -R sol:sol /mnt/solana
sysctl -q -p /etc/sysctl.d/21-agave-validator.conf || true
echo "node-prep done"
