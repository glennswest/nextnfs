#!/bin/bash
# ci-nfs-test.sh — Mount and test nextnfs on server2/server3 (no deploy)
#
# Assumes nextnfs is already installed and running on the target servers.
# Mounts NFS from each server and runs the shell test suites.
#
# Usage: ./ci/ci-nfs-test.sh
#
# Designed for mkube build runners on g10 network.

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

TOTAL_TEST_PASS=0
TOTAL_TEST_FAIL=0
TOTAL_TEST_SKIP=0
SERVERS_TESTED=0

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          nextnfs NFS Test — v${VERSION}                          ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Date:    $(date)"
echo "  Kernel:  $(uname -r)"
echo "  Source:  $SRC"
echo "  Targets: $TARGETS"
echo ""

# ── Install NFS client ───────────────────────────────────────────────────────

dnf install -y nfs-utils 2>/dev/null || true
mkdir -p "$MOUNT_DIR"

# ── Test each server ─────────────────────────────────────────────────────────

for target in $TARGETS; do
    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║  Testing: $target"
    echo "╚══════════════════════════════════════════════════════════════╝"

    # Check NFS port
    if ! timeout 5 bash -c "echo >/dev/tcp/${target}/2049" 2>/dev/null; then
        skip "$target NFS port not reachable"
        continue
    fi

    # Health check
    health=$(curl -sf "http://${target}:8080/health" 2>/dev/null || echo "unreachable")
    info "Health: $health"

    RESULTS_SERVER="$RESULTS_BASE/$target"
    mkdir -p "$RESULTS_SERVER"

    for suite in nfs4_basic nfs4_edge nfs4_stress; do
        echo ""
        echo "── $suite on $target ──"

        # Mount
        umount -f "$MOUNT_DIR" 2>/dev/null || true
        if ! mount -t nfs4 -o vers=4.0,soft,timeo=50,retrans=3 "${target}:/" "$MOUNT_DIR" 2>&1; then
            fail "could not mount ${target}:/"
            TOTAL_TEST_FAIL=$((TOTAL_TEST_FAIL + 1))
            continue
        fi
        ok "mounted ${target}:/ on $MOUNT_DIR"

        # Run test suite
        export NFS_HOST="$target"
        export NFS_PORT=2049
        export NFS_MOUNT="$MOUNT_DIR"
        export NFS_EXPORT="/"
        export NFS_VERS="4.0"
        export RESULTS_DIR="$RESULTS_SERVER"

        bash "$SRC/tests/${suite}.sh" 2>&1 | tee "$RESULTS_SERVER/${suite}.log"

        # Parse results
        pass=$(grep -cE 'PASS \(' "$RESULTS_SERVER/${suite}.log" 2>/dev/null || echo 0)
        failed=$(grep -cE 'FAIL \(' "$RESULTS_SERVER/${suite}.log" 2>/dev/null || echo 0)
        skipped=$(grep -cE 'SKIP' "$RESULTS_SERVER/${suite}.log" 2>/dev/null || echo 0)

        TOTAL_TEST_PASS=$((TOTAL_TEST_PASS + pass))
        TOTAL_TEST_FAIL=$((TOTAL_TEST_FAIL + failed))
        TOTAL_TEST_SKIP=$((TOTAL_TEST_SKIP + skipped))

        echo "    $suite: pass=$pass fail=$failed skip=$skipped"

        # Unmount
        umount -f "$MOUNT_DIR" 2>/dev/null || true

        # Clean export between suites
        ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -o BatchMode=yes \
            "root@${target}" 'rm -rf /export/* 2>/dev/null' 2>/dev/null || true
    done

    SERVERS_TESTED=$((SERVERS_TESTED + 1))
done

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                   Test Summary — v${VERSION}                      ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Servers tested: $SERVERS_TESTED"
echo "  Tests:  $TOTAL_TEST_PASS pass / $TOTAL_TEST_FAIL fail / $TOTAL_TEST_SKIP skip"
echo ""

if [ "$SERVERS_TESTED" -eq 0 ]; then
    fail "No servers reachable — no tests ran"
    exit 1
fi

if [ "$TOTAL_TEST_FAIL" -gt 0 ]; then
    fail "Some tests failed"
    exit 1
fi

ok "All tests passed"
