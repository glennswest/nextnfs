#!/bin/bash
# ci-deploy.sh — Build RPM and deploy to target servers
#
# Builds the RPM first (calls ci-rpm.sh), then copies and installs
# on the specified target servers via SSH.
#
# Usage: ./ci-deploy.sh [server2 server3 ...]
# Default targets: server2.g10.lo server3.g10.lo

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
TARGETS="${@:-server2.g10.lo server3.g10.lo}"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          nextnfs Deploy — v${VERSION}                            ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# ── Build RPM ────────────────────────────────────────────────────────────────

RPM_FILE="dist/nextnfs-${VERSION}-1.*.x86_64.rpm"
# shellcheck disable=SC2086
if ! ls $RPM_FILE 1>/dev/null 2>&1; then
    echo "==> RPM not found, building..."
    bash "$SCRIPT_DIR/ci-rpm.sh"
fi

# Find the actual RPM file
RPM_FILE=$(ls dist/nextnfs-${VERSION}-*.x86_64.rpm 2>/dev/null | head -1)
if [ -z "$RPM_FILE" ]; then
    echo "FATAL: RPM not found in dist/"
    exit 1
fi

echo "==> RPM: $RPM_FILE"
echo ""

# ── Deploy to each target ────────────────────────────────────────────────────

for target in $TARGETS; do
    echo "════════════════════════════════════════════════════════════"
    echo "  Deploying to: $target"
    echo "════════════════════════════════════════════════════════════"

    # Copy RPM
    echo "  ==> Copying RPM..."
    scp -o StrictHostKeyChecking=no "$RPM_FILE" "root@${target}:/tmp/nextnfs.rpm"

    # Install and start
    echo "  ==> Installing and starting..."
    # shellcheck disable=SC2087
    ssh -o StrictHostKeyChecking=no "root@${target}" <<'REMOTE'
set -euo pipefail

# Stop existing service if running
systemctl stop nextnfs.service 2>/dev/null || true

# Install RPM (upgrade if already installed)
rpm -Uvh --force /tmp/nextnfs.rpm

# Create export directory
mkdir -p /export
chmod 755 /export

# Ensure state directory exists
mkdir -p /var/lib/nextnfs

# Reload and start
systemctl daemon-reload
systemctl enable nextnfs.service
systemctl start nextnfs.service

# Wait for service to be ready
sleep 2

# Show status
echo ""
echo "=== Service Status ==="
systemctl status nextnfs.service --no-pager || true

echo ""
echo "=== Listening Ports ==="
ss -tlnp | grep -E '(2049|8080)' || echo "  (no NFS/API ports found)"

echo ""
echo "=== Binary Version ==="
/usr/bin/nextnfs --help 2>&1 | head -3 || true

echo ""
echo "=== Config ==="
cat /etc/nextnfs/nextnfs.toml

# Quick health check via API
echo ""
echo "=== Health Check ==="
sleep 1
curl -sf http://127.0.0.1:8080/health 2>/dev/null && echo " OK" || echo "  API not ready yet (may need a moment)"
REMOTE

    echo ""
    echo "  Done: $target"
    echo ""
done

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                   Deploy Complete                           ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Targets: $TARGETS"
echo "  Version: $VERSION"
echo ""
echo "  Test mount from client:"
echo "    mount -t nfs4 -o vers=4.0 server2.g10.lo:/ /mnt/test"
echo ""
