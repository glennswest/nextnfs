#!/bin/bash
# Per-export project quota test (#146). Needs root (mount, project ids,
# quotactl) and xfsprogs; run from the source tree:
#   sudo tests/quota.sh
# Makes a loopback XFS mounted with prjquota, points the privileged tests at
# it (NEXTNFS_PRJQUOTA_DIR) and runs them; everything is removed afterwards.
set -euo pipefail

if [ "$(id -u)" != 0 ]; then
    echo "tests/quota.sh: needs root (loop mount, project quotas)" >&2
    exit 2
fi
command -v mkfs.xfs >/dev/null || { echo "tests/quota.sh: mkfs.xfs missing (xfsprogs)" >&2; exit 2; }

src="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d /var/tmp/nextnfs-quota.XXXXXX)"
mnt="$work/mnt"
mkdir -p "$mnt"
cleanup() {
    umount "$mnt" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

truncate -s 512M "$work/xfs.img"
mkfs.xfs -q "$work/xfs.img"
mount -o loop,prjquota "$work/xfs.img" "$mnt"

cd "$src"
NEXTNFS_PRJQUOTA_DIR="$mnt" cargo test --locked -p nextnfs-server quota -- --nocapture --test-threads=1
# The API path too, with the export (TMPDIR) on the quota filesystem.
TMPDIR="$mnt" cargo test --locked -p nextnfs quota -- --nocapture --test-threads=1
echo "tests/quota.sh: PASS"
