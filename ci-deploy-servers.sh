#!/bin/bash
# ci-deploy-servers.sh — Build RPM and deploy to server2/server3
#
# Runs on mkube build runner. Builds the RPM, then copies and installs
# on server2 and server3 via SSH.
#
# Usage: ./ci-deploy-servers.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
TARGETS="server2.g10.lo server3.g10.lo"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          nextnfs Build + Deploy — v${VERSION}                    ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Date:    $(date)"
echo "  Arch:    $(uname -m)"
echo "  Kernel:  $(uname -r)"
echo "  Targets: $TARGETS"
echo ""

# ── Phase 1: Build RPM ──────────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 1: Build RPM"
echo "═══════════════════════════════════════════════════════════════"
echo ""

bash "$SCRIPT_DIR/ci-rpm.sh"

RPM_FILE=$(ls dist/nextnfs-${VERSION}-*.x86_64.rpm 2>/dev/null | head -1)
if [ -z "$RPM_FILE" ]; then
    echo "FATAL: RPM not found in dist/"
    exit 1
fi

echo ""
echo "  RPM: $RPM_FILE ($(ls -lh "$RPM_FILE" | awk '{print $5}'))"
echo ""

# ── Phase 2: Deploy to targets ──────────────────────────────────────────────

echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 2: Deploy"
echo "═══════════════════════════════════════════════════════════════"

# Install SSH client
dnf install -y openssh-clients 2>/dev/null || true

DEPLOY_SUCCESS=0
DEPLOY_FAIL=0

for target in $TARGETS; do
    echo ""
    echo "── Deploying to: $target ──"
    echo ""

    # Check if target is reachable
    if ! timeout 5 bash -c "echo >/dev/tcp/${target}/22" 2>/dev/null; then
        echo "  SKIP: $target not reachable on port 22"
        ((DEPLOY_FAIL++))
        continue
    fi

    # Copy RPM
    echo "  Copying RPM..."
    if ! scp -o StrictHostKeyChecking=no -o ConnectTimeout=10 "$RPM_FILE" "root@${target}:/tmp/nextnfs.rpm" 2>&1; then
        echo "  FAIL: could not copy RPM to $target"
        ((DEPLOY_FAIL++))
        continue
    fi

    # Install and start
    echo "  Installing and starting..."
    if ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 "root@${target}" bash -s 2>&1 <<'REMOTE'; then
set -euo pipefail

# Stop existing service if running
systemctl stop nextnfs.service 2>/dev/null || true

# Install RPM (upgrade if already installed)
rpm -Uvh --force /tmp/nextnfs.rpm
rm -f /tmp/nextnfs.rpm

# Create export directory
mkdir -p /export
chmod 755 /export

# Ensure state directory exists
mkdir -p /var/lib/nextnfs

# Open firewall port if firewalld is running
if systemctl is-active firewalld >/dev/null 2>&1; then
    firewall-cmd --permanent --add-service=nfs 2>/dev/null || true
    firewall-cmd --permanent --add-port=8080/tcp 2>/dev/null || true
    firewall-cmd --reload 2>/dev/null || true
    echo "  Firewall: NFS and API ports opened"
fi

# Reload and start
systemctl daemon-reload
systemctl enable nextnfs.service
systemctl start nextnfs.service

# Wait for service to be ready
sleep 3

# Show status
echo ""
echo "=== Service Status ==="
systemctl is-active nextnfs.service && echo "  Active: yes" || echo "  Active: no"
systemctl status nextnfs.service --no-pager -l 2>&1 | tail -15

echo ""
echo "=== Listening Ports ==="
ss -tlnp | grep -E '(2049|8080)' || echo "  (waiting for ports...)"

echo ""
echo "=== Health Check ==="
curl -sf http://127.0.0.1:8080/health 2>/dev/null && echo "  OK" || echo "  Pending (service may need a moment)"
REMOTE

        echo "  OK: $target deployed successfully"
        ((DEPLOY_SUCCESS++))
    else
        echo "  FAIL: deployment to $target failed"
        ((DEPLOY_FAIL++))
    fi
done

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                   Deploy Summary                            ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Version:   $VERSION"
echo "  RPM:       $RPM_FILE"
echo "  Success:   $DEPLOY_SUCCESS"
echo "  Failed:    $DEPLOY_FAIL"
echo ""
echo "  Test from client:"
echo "    mount -t nfs4 -o vers=4.0 server2.g10.lo:/ /mnt/test"
echo ""

if [ "$DEPLOY_FAIL" -gt 0 ]; then
    exit 1
fi
