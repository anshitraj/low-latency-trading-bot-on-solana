#!/usr/bin/env bash
# Install systemd units so `gcloud compute instances start trader-rpc-node`
# brings everything up with no manual steps: node-prep (disks) -> sol (agave)
# -> shred-observer. Run once as root. Idempotent.
set -euo pipefail
install -m 755 "$(dirname "$0")/node-prep.sh" /usr/local/bin/node-prep.sh

cat >/etc/systemd/system/node-prep.service <<'EOF'
[Unit]
Description=Prepare disks for the Solana RPC node
Before=sol.service
After=local-fs.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/node-prep.sh

[Install]
WantedBy=multi-user.target
EOF

# sol.service waits for node-prep.
mkdir -p /etc/systemd/system/sol.service.d
cat >/etc/systemd/system/sol.service.d/10-prep.conf <<'EOF'
[Unit]
Requires=node-prep.service
After=node-prep.service
EOF

cat >/etc/systemd/system/shred-observer.service <<'EOF'
[Unit]
Description=trader shred observer (read-only shred copy -> signature feed)
After=sol.service

[Service]
Type=simple
ExecStart=/usr/local/bin/run-observer.sh
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF

install -m 755 "$(dirname "$0")/snapsync.sh" /usr/local/bin/snapsync.sh
cat >/etc/systemd/system/snapsync.service <<'UNIT'
[Unit]
Description=Copy newest snapshot archives to the persistent disk

[Service]
Type=oneshot
ExecStart=/usr/local/bin/snapsync.sh
UNIT
cat >/etc/systemd/system/snapsync.timer <<'UNIT'
[Unit]
Description=Keep the persistent snapshot backup fresh

[Timer]
OnBootSec=15min
OnUnitActiveSec=10min

[Install]
WantedBy=timers.target
UNIT

systemctl daemon-reload
systemctl enable node-prep.service sol.service shred-observer.service
systemctl enable --now snapsync.timer
echo "units installed"
