#!/usr/bin/env bash
# Keep the persistent snapshot backup fresh (run by snapsync.timer, root).
# Copies the newest full + incremental snapshot archives the node produced or
# downloaded into /mnt/persist/snap, keeps 1 full + 2 incrementals there.
# Safe while agave runs (archives are written once, then only read).
set -euo pipefail
DST=/mnt/persist/snap
mountpoint -q /mnt/persist || exit 0
mkdir -p "$DST"
newest() { ls -t "$@" 2>/dev/null | head -1 || true; }
full=$(newest /mnt/solana/snapshots/snapshot-*.tar.zst /mnt/solana/snapshots/remote/snapshot-*.tar.zst)
inc=$(newest /mnt/solana/snapshots/incremental-snapshot-*.tar.zst /mnt/solana/snapshots/remote/incremental-snapshot-*.tar.zst)
copy() { # atomic: copy to .tmp then rename, skip if present
  local f=$1 d=$2
  [ -n "$f" ] || return 0
  local b; b=$(basename "$f")
  [ -e "$d/$b" ] && return 0
  # agave rotates incrementals every ~200 slots: a file may vanish mid-copy;
  # then skip it (the next run takes the newer one).
  if cp "$f" "$d/.$b.tmp" 2>/dev/null; then mv "$d/.$b.tmp" "$d/$b"; else rm -f "$d/.$b.tmp"; fi
}
copy "$full" "$DST"
copy "$inc" "$DST" || true
[ -n "$(ls "$DST"/incremental-snapshot-*.tar.zst 2>/dev/null)" ] || {
  inc=$(newest /mnt/solana/snapshots/incremental-snapshot-*.tar.zst); copy "$inc" "$DST"; }
# Prune: newest full only; newest 2 incrementals whose base matches a kept full.
ls -t "$DST"/snapshot-*.tar.zst "$DST"/remote/snapshot-*.tar.zst 2>/dev/null | tail -n +2 | xargs -r rm -f
ls -t "$DST"/incremental-snapshot-*.tar.zst 2>/dev/null | tail -n +3 | xargs -r rm -f
chown -R sol:sol "$DST"
echo "snapsync ok: full=$(basename "${full:-none}") inc=$(basename "${inc:-none}")"
