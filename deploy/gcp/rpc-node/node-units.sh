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

# Optional second observer for a third-party shred feed on UDP 20000 (enabled only
# when run-observer-feed.sh is present next to this script).
if [ -f "$(dirname "$0")/run-observer-feed.sh" ]; then
  install -m 755 "$(dirname "$0")/run-observer-feed.sh" /usr/local/bin/run-observer-feed.sh
  cat >/etc/systemd/system/shred-observer-feed.service <<'UNIT'
[Unit]
Description=trader shred observer for an external shred feed (UDP 20000 -> 9902)
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/run-observer-feed.sh
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT
  FEED_UNIT=shred-observer-feed.service
fi

# Optional third observer for Everstake ShredStream on UDP 20001.
if [ -f "$(dirname "$0")/run-observer-everstake.sh" ]; then
  install -m 755 "$(dirname "$0")/run-observer-everstake.sh" /usr/local/bin/run-observer-everstake.sh
  cat >/etc/systemd/system/shred-observer-everstake.service <<'UNIT'
[Unit]
Description=trader shred observer for Everstake ShredStream (UDP 20001 -> 9904)
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/run-observer-everstake.sh
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT
  FEED2_UNIT=shred-observer-everstake.service
fi

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
systemctl enable node-prep.service sol.service shred-observer.service ${FEED_UNIT:-} ${FEED2_UNIT:-}
systemctl enable --now snapsync.timer
echo "units installed"
