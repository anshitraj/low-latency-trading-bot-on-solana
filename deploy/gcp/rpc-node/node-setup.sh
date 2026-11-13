#!/usr/bin/env bash
# One-time setup of a NON-VOTING Solana mainnet RPC node (test session).
# Run as root on trader-rpc-node. Idempotent where practical.
set -euo pipefail
AGAVE_VERSION="${AGAVE_VERSION:-v4.4.1}"

echo "== packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq mdadm xfsprogs curl bzip2 jq >/dev/null

echo "== local NVMe RAID0 -> /mnt/solana"
if ! mountpoint -q /mnt/solana; then
  mapfile -t DEVS < <(ls /dev/disk/by-id/google-local-nvme-ssd-* 2>/dev/null | grep -v part || true)
  if [ "${#DEVS[@]}" -eq 0 ]; then
    mapfile -t DEVS < <(lsblk -dpno NAME,MODEL | awk '/nvme_card|NVMe/ {print $1}' | grep -v nvme0n1 || true)
  fi
  echo "devices: ${#DEVS[@]}"
  [ "${#DEVS[@]}" -ge 2 ]
  mdadm --create /dev/md0 --level=0 --raid-devices="${#DEVS[@]}" "${DEVS[@]}" --force --run
  mkfs.xfs -f -q /dev/md0
  mkdir -p /mnt/solana
  mount -o noatime,discard /dev/md0 /mnt/solana
fi
df -h /mnt/solana | tail -1

echo "== kernel / network tuning (Agave system requirements)"
cat >/etc/sysctl.d/21-agave-validator.conf <<'EOF'
net.core.rmem_default = 134217728
net.core.rmem_max = 134217728
net.core.wmem_default = 134217728
net.core.wmem_max = 134217728
net.core.netdev_max_backlog = 30000
vm.max_map_count = 1000000
fs.nr_open = 1000000
vm.swappiness = 1
EOF
sysctl -q -p /etc/sysctl.d/21-agave-validator.conf
cat >/etc/security/limits.d/90-solana-nofiles.conf <<'EOF'
* - nofile 1000000
EOF

echo "== user + dirs"
id sol >/dev/null 2>&1 || useradd -m -s /bin/bash sol
mkdir -p /mnt/solana/{ledger,accounts,snapshots}
chown -R sol:sol /mnt/solana

echo "== agave ${AGAVE_VERSION} (official Anza release installer)"
sudo -iu sol bash -c "curl -sSfL https://release.anza.xyz/${AGAVE_VERSION}/install | sh -s - ${AGAVE_VERSION} >/dev/null 2>&1"
sudo -iu sol bash -lc 'export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"; agave-validator --version; solana-keygen --version'

echo "== throwaway node identity (no funds, never votes)"
sudo -iu sol bash -lc 'export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"; [ -f ~/node-identity.json ] || solana-keygen new --no-bip39-passphrase --silent -o ~/node-identity.json >/dev/null; solana-keygen pubkey ~/node-identity.json'
echo "setup done"
