#!/bin/bash
# ci-deploy-test.sh — Build RPM, deploy to server2/server3, run shell tests
#
# Full integration pipeline for mkube runners:
#   1. Build musl static binary + RPM
#   2. Deploy to server2.g10.lo and server3.g10.lo
#   3. Mount NFS from each server and run test suites
#   4. Collect and report results
#
# Usage: ./ci/ci-deploy-test.sh
#
# Requires: mkube build runner with SSH access to server2/server3

set -uo pipefail

# ── Find source ──────────────────────────────────────────────────────────────

if [ -f "/build/Cargo.toml" ]; then
    SRC="/build"
elif [ -f "$(cd "$(dirname "$0")/.." && pwd)/Cargo.toml" ]; then
    SRC="$(cd "$(dirname "$0")/.." && pwd)"
else
    echo "FATAL: Cannot find nextnfs source"
    exit 1
fi

cd "$SRC"

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
TARGETS="server2.g10.lo server3.g10.lo"
MOUNT_DIR="/mnt/nfs-test"
RESULTS_BASE="/tmp/nfs-ci-results"

# ── Colour output ────────────────────────────────────────────────────────────

if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'
else
    GREEN='' RED='' YELLOW='' CYAN='' BOLD='' RESET=''
fi

ok()   { echo -e "  ${GREEN}OK${RESET}: $1"; }
fail() { echo -e "  ${RED}FAIL${RESET}: $1"; }
skip() { echo -e "  ${YELLOW}SKIP${RESET}: $1"; }
info() { echo -e "  ${CYAN}INFO${RESET}: $1"; }

TOTAL_DEPLOY_OK=0
TOTAL_DEPLOY_FAIL=0
TOTAL_TEST_PASS=0
TOTAL_TEST_FAIL=0
TOTAL_TEST_SKIP=0

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          nextnfs Deploy + Test — v${VERSION}                      ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Date:    $(date)"
echo "  Arch:    $(uname -m)"
echo "  Kernel:  $(uname -r)"
echo "  Source:  $SRC"
echo "  Targets: $TARGETS"
echo ""

# ── Phase 1: Build RPM ──────────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 1: Build RPM"
echo "═══════════════════════════════════════════════════════════════"
echo ""

if [ -f "$SRC/ci-rpm.sh" ]; then
    bash "$SRC/ci-rpm.sh"
else
    fail "ci-rpm.sh not found"
    exit 1
fi

RPM_FILE=$(ls dist/nextnfs-${VERSION}-*.x86_64.rpm 2>/dev/null | head -1)
if [ -z "$RPM_FILE" ]; then
    fail "RPM not found in dist/"
    exit 1
fi

ok "RPM built: $RPM_FILE ($(ls -lh "$RPM_FILE" | awk '{print $5}'))"
echo ""

# ── Phase 2: Deploy to servers ───────────────────────────────────────────────

echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 2: Deploy"
echo "═══════════════════════════════════════════════════════════════"

# Install SSH + NFS client tools
dnf install -y openssh-clients nfs-utils 2>/dev/null || true

SSH_OPTS="-o StrictHostKeyChecking=no -o ConnectTimeout=10 -o BatchMode=yes"

for target in $TARGETS; do
    echo ""
    echo "── Deploying to: $target ──"

    if ! timeout 5 bash -c "echo >/dev/tcp/${target}/22" 2>/dev/null; then
        skip "$target not reachable on port 22"
        ((TOTAL_DEPLOY_FAIL++))
        continue
    fi

    # Copy RPM
    if ! scp $SSH_OPTS "$RPM_FILE" "root@${target}:/tmp/nextnfs.rpm" 2>&1; then
        fail "could not copy RPM to $target"
        ((TOTAL_DEPLOY_FAIL++))
        continue
    fi

    # Install and restart
    if ssh $SSH_OPTS "root@${target}" bash -s 2>&1 <<'REMOTE'; then
set -euo pipefail
systemctl stop nextnfs.service 2>/dev/null || true
rpm -Uvh --force /tmp/nextnfs.rpm
rm -f /tmp/nextnfs.rpm
mkdir -p /export
chmod 755 /export
mkdir -p /var/lib/nextnfs

# Clean export dir for test isolation
rm -rf /export/*

# Open firewall
if systemctl is-active firewalld >/dev/null 2>&1; then
    firewall-cmd --permanent --add-service=nfs 2>/dev/null || true
    firewall-cmd --permanent --add-port=8080/tcp 2>/dev/null || true
    firewall-cmd --reload 2>/dev/null || true
fi

systemctl daemon-reload
systemctl enable nextnfs.service
systemctl start nextnfs.service
sleep 3

# Verify
systemctl is-active nextnfs.service || exit 1
curl -sf http://127.0.0.1:8080/health >/dev/null 2>&1 || {
    echo "Health check failed — waiting 5 more seconds..."
    sleep 5
    curl -sf http://127.0.0.1:8080/health >/dev/null 2>&1 || exit 1
}
echo "Service running and healthy"
REMOTE

        ok "$target deployed"
        ((TOTAL_DEPLOY_OK++))
    else
        fail "$target deploy failed"
        ((TOTAL_DEPLOY_FAIL++))
    fi
done

echo ""
echo "  Deployed: $TOTAL_DEPLOY_OK   Failed: $TOTAL_DEPLOY_FAIL"

if [ "$TOTAL_DEPLOY_OK" -eq 0 ]; then
    fail "No servers deployed — cannot run tests"
    exit 1
fi

# ── Phase 3: Run shell tests against each server ─────────────────────────────

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 3: Shell Tests"
echo "═══════════════════════════════════════════════════════════════"

mkdir -p "$MOUNT_DIR"

run_test_suite() {
    local server="$1"
    local suite="$2"
    local results_dir="$3"

    echo ""
    echo "  ── $suite on $server ──"

    export NFS_HOST="$server"
    export NFS_PORT=2049
    export NFS_MOUNT="$MOUNT_DIR"
    export NFS_EXPORT="/"
    export NFS_VERS="4.0"
    export RESULTS_DIR="$results_dir"
    mkdir -p "$results_dir"

    # Mount
    umount -f "$MOUNT_DIR" 2>/dev/null || true
    if ! mount -t nfs4 -o vers=4.0,soft,timeo=50,retrans=3 "${server}:/" "$MOUNT_DIR" 2>&1; then
        fail "could not mount ${server}:/ on $MOUNT_DIR"
        return 1
    fi

    # Run test script
    local test_script="$SRC/tests/${suite}.sh"
    if [ ! -f "$test_script" ]; then
        fail "test script not found: $test_script"
        umount -f "$MOUNT_DIR" 2>/dev/null || true
        return 1
    fi

    local exit_code=0
    bash "$test_script" 2>&1 || exit_code=$?

    # Unmount
    umount -f "$MOUNT_DIR" 2>/dev/null || true

    return $exit_code
}

for target in $TARGETS; do
    # Check if this server was deployed
    if ! timeout 5 bash -c "echo >/dev/tcp/${target}/2049" 2>/dev/null; then
        skip "$target NFS port not reachable — skipping tests"
        continue
    fi

    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║  Testing: $target"
    echo "╚══════════════════════════════════════════════════════════════╝"

    RESULTS_SERVER="$RESULTS_BASE/$target"
    mkdir -p "$RESULTS_SERVER"

    for suite in nfs4_basic nfs4_edge nfs4_stress; do
        run_test_suite "$target" "$suite" "$RESULTS_SERVER" 2>&1 | tee "$RESULTS_SERVER/${suite}.log"

        # Parse results from the log
        pass=$(grep -cE 'PASS \(' "$RESULTS_SERVER/${suite}.log" 2>/dev/null || true)
        failed=$(grep -cE 'FAIL \(' "$RESULTS_SERVER/${suite}.log" 2>/dev/null || true)
        skipped=$(grep -cE 'SKIP$|SKIP \(' "$RESULTS_SERVER/${suite}.log" 2>/dev/null || true)

        TOTAL_TEST_PASS=$((TOTAL_TEST_PASS + pass))
        TOTAL_TEST_FAIL=$((TOTAL_TEST_FAIL + failed))
        TOTAL_TEST_SKIP=$((TOTAL_TEST_SKIP + skipped))

        echo "    $suite: pass=$pass fail=$failed skip=$skipped"

        # Clean export dir between suites for isolation
        ssh $SSH_OPTS "root@${target}" 'rm -rf /export/* 2>/dev/null' || true
    done

    # Collect server logs
    ssh $SSH_OPTS "root@${target}" 'journalctl -u nextnfs.service --no-pager -n 200' \
        > "$RESULTS_SERVER/nextnfs-journal.log" 2>&1 || true
done

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                   Test Summary — v${VERSION}                      ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Deploy:  $TOTAL_DEPLOY_OK ok / $TOTAL_DEPLOY_FAIL fail"
echo "  Tests:   $TOTAL_TEST_PASS pass / $TOTAL_TEST_FAIL fail / $TOTAL_TEST_SKIP skip"
echo "  Results: $RESULTS_BASE/"
echo ""

# List result files
if [ -d "$RESULTS_BASE" ]; then
    find "$RESULTS_BASE" -name "*.log" -o -name "*.json" | sort | while read -r f; do
        echo "    $(basename "$(dirname "$f")")/$(basename "$f") ($(wc -l < "$f") lines)"
    done
fi

echo ""

if [ "$TOTAL_TEST_FAIL" -gt 0 ]; then
    fail "Some tests failed"
    echo ""
    echo "  Failed test details:"
    for logfile in "$RESULTS_BASE"/*/nfs4_*.log; do
        [ -f "$logfile" ] || continue
        fails=$(grep 'FAIL:' "$logfile" 2>/dev/null || true)
        if [ -n "$fails" ]; then
            echo ""
            echo "  $(basename "$(dirname "$logfile")")/$(basename "$logfile"):"
            echo "$fails" | sed 's/^/    /'
        fi
    done
    exit 1
fi

ok "All tests passed"
