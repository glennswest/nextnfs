#!/bin/bash
# ci-stress.sh — Build RPM, deploy to server2/server3, run nextnfs-stress
#
# End-to-end stress pipeline for mkube runners:
#   1. Build the RPM (which now bundles /usr/bin/nextnfs-stress)
#   2. rpm -Uvh --force on each target, restart the service
#   3. Mount NFS from each target and run the 1000-file stress
#   4. Report per-server pass/fail with phase breakdown
#
# Usage: ./ci/ci-stress.sh
#
# Env overrides:
#   TARGETS  — space-separated list of hosts (default: server2/server3)
#   COUNT    — files per phase (default 1000)
#   PARALLEL — concurrent workers (default 32)
#   SIZE     — file payload size (default 4096)

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
TARGETS="${TARGETS:-server2.g10.lo server3.g10.lo}"
MOUNT_DIR="/mnt/nfs-stress"
RESULTS_BASE="/tmp/nfs-stress-results"
COUNT="${COUNT:-1000}"
PARALLEL="${PARALLEL:-32}"
SIZE="${SIZE:-4096}"

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

DEPLOYS_OK=0
DEPLOYS_FAIL=0
STRESS_OK=0
STRESS_FAIL=0

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          nextnfs-stress pipeline — v${VERSION}                   ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Date:     $(date)"
echo "  Source:   $SRC"
echo "  Targets:  $TARGETS"
echo "  Count:    $COUNT files / phase"
echo "  Parallel: $PARALLEL workers"
echo "  Size:     $SIZE bytes / file"
echo ""

# ── Phase 1: build RPM ──────────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 1: Build RPM"
echo "═══════════════════════════════════════════════════════════════"

if [ ! -f "$SRC/ci-rpm.sh" ]; then
    fail "ci-rpm.sh not found"
    exit 1
fi
bash "$SRC/ci-rpm.sh" || { fail "RPM build failed"; exit 1; }

RPM_FILE=$(ls "$SRC/dist/nextnfs-${VERSION}-"*.x86_64.rpm 2>/dev/null | head -1)
if [ -z "$RPM_FILE" ]; then
    fail "RPM not found in dist/"
    exit 1
fi
ok "RPM: $RPM_FILE ($(ls -lh "$RPM_FILE" | awk '{print $5}'))"

# ── Phase 2: deploy ──────────────────────────────────────────────────────────

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 2: Deploy"
echo "═══════════════════════════════════════════════════════════════"

dnf install -y openssh-clients nfs-utils 2>/dev/null || true
mkdir -p "$MOUNT_DIR" "$RESULTS_BASE"

SSH_OPTS="-o StrictHostKeyChecking=no -o ConnectTimeout=10 -o BatchMode=yes"

for target in $TARGETS; do
    echo ""
    echo "── Deploying to: $target ──"

    if ! bash -c "exec 3<>/dev/tcp/${target}/22" 2>/dev/null; then
        skip "$target not reachable on port 22"
        DEPLOYS_FAIL=$((DEPLOYS_FAIL + 1))
        continue
    fi

    if ! scp $SSH_OPTS "$RPM_FILE" "root@${target}:/tmp/nextnfs.rpm" 2>&1; then
        fail "scp RPM to $target failed"
        DEPLOYS_FAIL=$((DEPLOYS_FAIL + 1))
        continue
    fi

    if ssh $SSH_OPTS "root@${target}" bash -s 2>&1 <<'REMOTE'; then
set -uo pipefail
umount -f /mnt/nfs-stress 2>/dev/null || true
systemctl stop nextnfs.service 2>/dev/null || true
rpm -Uvh --force /tmp/nextnfs.rpm
rm -f /tmp/nextnfs.rpm
mkdir -p /export
chmod 755 /export
rm -rf /export/* 2>/dev/null || true

systemctl daemon-reload
systemctl enable nextnfs.service
systemctl start nextnfs.service
sleep 3

systemctl is-active nextnfs.service || exit 1
curl -sf --max-time 5 http://127.0.0.1:8080/health >/dev/null || {
    sleep 5
    curl -sf --max-time 5 http://127.0.0.1:8080/health >/dev/null || exit 1
}
echo "service running"
test -x /usr/bin/nextnfs-stress
echo "stress binary present: $(/usr/bin/nextnfs-stress --version)"
REMOTE
        ok "$target deployed"
        DEPLOYS_OK=$((DEPLOYS_OK + 1))
    else
        fail "$target deploy failed"
        DEPLOYS_FAIL=$((DEPLOYS_FAIL + 1))
    fi
done

if [ "$DEPLOYS_OK" -eq 0 ]; then
    fail "No servers deployed — aborting"
    exit 1
fi

# ── Phase 3: stress ──────────────────────────────────────────────────────────

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 3: Stress (count=$COUNT parallel=$PARALLEL size=$SIZE)"
echo "═══════════════════════════════════════════════════════════════"

for target in $TARGETS; do
    if ! bash -c "exec 3<>/dev/tcp/${target}/2049" 2>/dev/null; then
        skip "$target NFS port not reachable — skipping"
        continue
    fi

    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║  Stress: $target"
    echo "╚══════════════════════════════════════════════════════════════╝"

    LOG="$RESULTS_BASE/${target}.stress.log"
    SERVER_JOURNAL="$RESULTS_BASE/${target}.journal.log"

    # Run the stress harness ON the target itself: it has the binary, the
    # mount, and the local NFS export. Easier than mounting from this runner
    # (which may not have NFS client tooling on every pool).
    if ssh $SSH_OPTS "root@${target}" bash -s "$COUNT" "$PARALLEL" "$SIZE" 2>&1 \
        <<'REMOTE' | tee "$LOG"; then
COUNT="$1"; PARALLEL="$2"; SIZE="$3"
set -uo pipefail
mkdir -p /mnt/nfs-stress
umount -f /mnt/nfs-stress 2>/dev/null || true
mount -t nfs4 -o vers=4.0,soft,timeo=50,retrans=3 \
    "$(hostname):/" /mnt/nfs-stress
trap 'umount -f /mnt/nfs-stress 2>/dev/null || true' EXIT
/usr/bin/nextnfs-stress \
    --target /mnt/nfs-stress \
    --count "$COUNT" \
    --parallel "$PARALLEL" \
    --size "$SIZE"
REMOTE
        ok "$target: stress PASS"
        STRESS_OK=$((STRESS_OK + 1))
    else
        fail "$target: stress FAIL"
        STRESS_FAIL=$((STRESS_FAIL + 1))
    fi

    ssh $SSH_OPTS "root@${target}" \
        'journalctl -u nextnfs.service --no-pager -n 300' \
        > "$SERVER_JOURNAL" 2>&1 || true
done

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          Stress Summary — v${VERSION}                            ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Deploy:  $DEPLOYS_OK ok / $DEPLOYS_FAIL fail"
echo "  Stress:  $STRESS_OK ok / $STRESS_FAIL fail"
echo "  Logs:    $RESULTS_BASE/"
echo ""

if [ "$STRESS_FAIL" -gt 0 ] || [ "$STRESS_OK" -eq 0 ]; then
    fail "Stress run did not fully pass"
    exit 1
fi

ok "Stress passed on all reachable servers"
