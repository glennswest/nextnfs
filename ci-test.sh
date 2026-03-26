#!/bin/bash
# ci-test.sh — Unified CI orchestrator for nextnfs
#
# Runs the complete test suite:
#   Phase 1: Build all workspace crates (debug + release)
#   Phase 2: Cargo unit tests + clippy
#   Phase 3: Baseline — start knfsd, run wire + shell tests against it
#   Phase 4: nextnfs — start server, run wire + shell tests against it
#   Phase 5: Performance — fio benchmarks against both
#   Phase 6: Comparison report — side-by-side knfsd vs nextnfs
#   Phase 7: Cleanup
#
# Requirements: Linux, root, nfs-utils, fio, python3

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTS_DIR="$SCRIPT_DIR/tests"
RESULTS_DIR="/tmp/nfs-ci-results"

# Ports — use different ports so knfsd (2049) and nextnfs (2050) don't conflict
KNFSD_PORT=2049
NEXTNFS_PORT=2050

# Mount points
KNFSD_MOUNT="/mnt/nfs-baseline"
NEXTNFS_MOUNT="/mnt/nfs-test"

# Export directories
KNFSD_EXPORT_DIR="/tmp/nfs-knfsd-export"
NEXTNFS_EXPORT_DIR="/tmp/nfs-nextnfs-export"

# Binaries
NEXTNFS_BIN="$SCRIPT_DIR/target/debug/nextnfs"
NEXTNFSTEST_BIN="$SCRIPT_DIR/target/debug/nextnfstest"

# Counters
PHASE_FAILURES=0
TOTAL_FAILURES=0

# ── Colour output ────────────────────────────────────────────────────────────

if [ -t 1 ]; then
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    YELLOW='\033[0;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    RESET='\033[0m'
else
    GREEN='' RED='' YELLOW='' CYAN='' BOLD='' RESET=''
fi

phase() {
    echo ""
    echo -e "${BOLD}${CYAN}════════════════════════════════════════════════════════════════${RESET}"
    echo -e "${BOLD}${CYAN}  Phase $1: $2${RESET}"
    echo -e "${BOLD}${CYAN}════════════════════════════════════════════════════════════════${RESET}"
    echo ""
    PHASE_FAILURES=0
}

section() {
    echo ""
    echo -e "${BOLD}── $1 ──${RESET}"
    echo ""
}

ok() {
    echo -e "  ${GREEN}OK${RESET}: $1"
}

fail() {
    echo -e "  ${RED}FAIL${RESET}: $1"
    ((PHASE_FAILURES++))
    ((TOTAL_FAILURES++))
}

skip() {
    echo -e "  ${YELLOW}SKIP${RESET}: $1"
}

# ── Prerequisites ────────────────────────────────────────────────────────────

install_prereqs() {
    section "Installing prerequisites"

    if command -v dnf >/dev/null 2>&1; then
        dnf install -y nfs-utils fio python3 2>/dev/null || true
    elif command -v apt-get >/dev/null 2>&1; then
        apt-get install -y nfs-common fio python3 2>/dev/null || true
    fi

    # Verify required tools
    local missing=0
    for cmd in mount umount fio python3 exportfs; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            echo "  WARNING: $cmd not found"
            ((missing++))
        fi
    done

    if [ "$missing" -gt 0 ]; then
        echo "  Some tools missing — shell tests may be limited"
    else
        ok "all prerequisites installed"
    fi
}

# ── Cleanup ──────────────────────────────────────────────────────────────────

cleanup() {
    section "Cleanup"

    # Unmount everything
    for mp in "$KNFSD_MOUNT" "$NEXTNFS_MOUNT" "/mnt/nfs-test2" "/mnt/nfs-test-41" "/mnt/nfs-test-41b"; do
        if mountpoint -q "$mp" 2>/dev/null; then
            umount -l "$mp" 2>/dev/null || umount -f "$mp" 2>/dev/null || true
        fi
    done

    # Stop nextnfs
    if [ -f /tmp/nextnfs-test.pid ]; then
        local pid
        pid=$(cat /tmp/nextnfs-test.pid 2>/dev/null)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            sleep 1
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f /tmp/nextnfs-test.pid
    fi

    # Stop knfsd
    exportfs -ua 2>/dev/null || true
    systemctl stop nfs-server 2>/dev/null || true

    # Remove export dirs
    rm -rf "$KNFSD_EXPORT_DIR" "$NEXTNFS_EXPORT_DIR" 2>/dev/null || true

    ok "cleanup complete"
}

# Cleanup on exit
trap cleanup EXIT

# ── Wait for TCP port ────────────────────────────────────────────────────────

wait_for_port() {
    local host="$1" port="$2" timeout="${3:-10}" label="${4:-service}"
    local deadline=$(( $(date +%s) + timeout ))
    while ! bash -c "echo >/dev/tcp/$host/$port" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            fail "$label: timeout waiting for $host:$port after ${timeout}s"
            return 1
        fi
        sleep 0.2
    done
    ok "$label listening on $host:$port"
}

# ── Run shell test suite ────────────────────────────────────────────────────

run_shell_tests() {
    local mount_point="$1"
    local nfs_port="$2"
    local label="$3"
    local results_prefix="$4"

    section "Shell functional tests ($label)"

    # Export env vars for helpers.sh
    export NFS_HOST=127.0.0.1
    export NFS_PORT="$nfs_port"
    export NFS_MOUNT="$mount_point"
    export NFS_EXPORT="/"
    export NFS_VERS="4.0"
    export RESULTS_DIR="$RESULTS_DIR/$results_prefix"
    export NEXTNFS_BIN
    export NEXTNFS_PID_FILE=/tmp/nextnfs-test.pid

    mkdir -p "$RESULTS_DIR"

    local suite rc
    for suite in nfs4_basic nfs4_edge nfs4_stress nfs41_session; do
        local script="$TESTS_DIR/${suite}.sh"
        if [ ! -f "$script" ]; then
            skip "$suite — script not found"
            continue
        fi

        echo ""
        echo -e "  ${BOLD}Running $suite against $label...${RESET}"
        rc=0
        bash "$script" 2>&1 || rc=$?

        if [ "$rc" -eq 0 ]; then
            ok "$suite"
        else
            fail "$suite (exit code $rc)"
        fi
    done
}

# ── Run wire-level tests ────────────────────────────────────────────────────

run_wire_tests() {
    local host="$1"
    local port="$2"
    local label="$3"
    local output_dir="$4"

    section "Wire-level protocol tests ($label)"

    if [ ! -f "$NEXTNFSTEST_BIN" ]; then
        skip "nextnfstest binary not found"
        return
    fi

    mkdir -p "$output_dir"

    local rc=0
    # Run NFSv4.0 wire tests (nextnfs is NFSv4-only; v3 tests need mountd)
    "$NEXTNFSTEST_BIN" run \
        --server "$host" \
        --port "$port" \
        --version 4.0 \
        --output "$output_dir" \
        2>&1 || rc=$?

    if [ "$rc" -eq 0 ]; then
        ok "wire tests passed"
    else
        fail "wire tests ($rc failures)"
    fi
}

# ── Run performance benchmarks ──────────────────────────────────────────────

run_perf_tests() {
    local mount_point="$1"
    local nfs_port="$2"
    local label="$3"
    local results_prefix="$4"

    section "Performance benchmarks ($label)"

    if ! command -v fio >/dev/null 2>&1; then
        skip "fio not installed — skipping performance benchmarks"
        return
    fi

    export NFS_HOST=127.0.0.1
    export NFS_PORT="$nfs_port"
    export NFS_MOUNT="$mount_point"
    export NFS_EXPORT="/"
    export NFS_VERS="4.0"
    export RESULTS_DIR="$RESULTS_DIR/$results_prefix"

    mkdir -p "$RESULTS_DIR"

    local script="$TESTS_DIR/nfs_performance.sh"
    if [ ! -f "$script" ]; then
        skip "nfs_performance.sh not found"
        return
    fi

    local rc=0
    bash "$script" 2>&1 || rc=$?

    if [ "$rc" -eq 0 ]; then
        ok "performance benchmarks complete"
    else
        fail "performance benchmarks (exit code $rc)"
    fi
}

# ── Start knfsd ──────────────────────────────────────────────────────────────

start_knfsd() {
    section "Starting kernel NFS server (knfsd)"

    mkdir -p "$KNFSD_EXPORT_DIR"
    chmod 777 "$KNFSD_EXPORT_DIR"

    # Configure exports
    echo "$KNFSD_EXPORT_DIR *(rw,sync,no_subtree_check,no_root_squash,fsid=0)" > /etc/exports

    # Enable and start NFS services
    modprobe nfsd 2>/dev/null || true
    systemctl start rpcbind 2>/dev/null || rpcbind 2>/dev/null || true
    sleep 1

    if systemctl start nfs-server 2>&1; then
        ok "nfs-server.service started"
    else
        echo "  systemctl start nfs-server failed, trying manual start..."
        rpc.nfsd 8 2>&1 || true
        rpc.mountd 2>&1 || true
    fi
    exportfs -ra 2>&1
    sleep 2

    if wait_for_port 127.0.0.1 "$KNFSD_PORT" 15 "knfsd"; then
        ok "knfsd started, exporting $KNFSD_EXPORT_DIR"
    else
        fail "knfsd failed to start"
        echo "  rpcinfo output:"
        rpcinfo -p 2>&1 || true
        return 1
    fi

    # Mount — nolock to avoid NLM issues, use fsid=0 export path
    mkdir -p "$KNFSD_MOUNT"
    echo "  Mounting: mount -t nfs4 127.0.0.1:$KNFSD_EXPORT_DIR $KNFSD_MOUNT"
    local mount_out
    mount_out=$(timeout 30 mount -t nfs4 -o vers=4.0,proto=tcp,nolock,soft,timeo=50,retrans=2 \
        "127.0.0.1:$KNFSD_EXPORT_DIR" "$KNFSD_MOUNT" 2>&1) || true
    echo "  mount output: $mount_out"
    # If that didn't work, try mounting / (some knfsd configs use fsid=0 as root)
    if ! mountpoint -q "$KNFSD_MOUNT" 2>/dev/null; then
        echo "  Retrying with 127.0.0.1:/ ..."
        mount_out=$(timeout 30 mount -t nfs4 -o vers=4.0,proto=tcp,nolock,soft,timeo=50,retrans=2 \
            "127.0.0.1:/" "$KNFSD_MOUNT" 2>&1) || true
        echo "  mount output: $mount_out"
    fi

    if mountpoint -q "$KNFSD_MOUNT"; then
        ok "knfsd mounted at $KNFSD_MOUNT"
    else
        fail "failed to mount knfsd"
        echo "  rpcinfo:"
        rpcinfo -p 2>&1 || true
        echo "  dmesg (last 10 NFS lines):"
        dmesg 2>/dev/null | grep -i nfs | tail -10 || true
        return 1
    fi
}

stop_knfsd() {
    if mountpoint -q "$KNFSD_MOUNT" 2>/dev/null; then
        umount -l "$KNFSD_MOUNT" 2>/dev/null || true
    fi
    exportfs -ua 2>/dev/null || true
    systemctl stop nfs-server 2>/dev/null || true
}

# ── Start nextnfs ────────────────────────────────────────────────────────────

start_nextnfs() {
    section "Starting nextnfs server"

    mkdir -p "$NEXTNFS_EXPORT_DIR"
    chmod 777 "$NEXTNFS_EXPORT_DIR"

    if [ ! -f "$NEXTNFS_BIN" ]; then
        fail "nextnfs binary not found at $NEXTNFS_BIN"
        return 1
    fi

    echo "  Binary: $NEXTNFS_BIN"
    echo "  Export: $NEXTNFS_EXPORT_DIR"
    echo "  Listen: 127.0.0.1:$NEXTNFS_PORT"

    RUST_LOG=info "$NEXTNFS_BIN" \
        --export "$NEXTNFS_EXPORT_DIR" \
        --listen "127.0.0.1:$NEXTNFS_PORT" \
        --api-listen "127.0.0.1:8090" \
        > /tmp/nextnfs-test.log 2>&1 &

    echo $! > /tmp/nextnfs-test.pid
    local pid
    pid=$(cat /tmp/nextnfs-test.pid)
    echo "  PID: $pid"

    if wait_for_port 127.0.0.1 "$NEXTNFS_PORT" 10 "nextnfs"; then
        ok "nextnfs started"
    else
        fail "nextnfs failed to start"
        echo "--- nextnfs log ---"
        cat /tmp/nextnfs-test.log 2>/dev/null || true
        echo "--- end log ---"
        return 1
    fi

    # Mount — nolock avoids NLM/rpcbind dependency, port= for non-standard port
    mkdir -p "$NEXTNFS_MOUNT"
    echo "  Mounting: mount -t nfs4 -o vers=4.0,proto=tcp,port=${NEXTNFS_PORT},nolock,soft,timeo=50,retrans=2 127.0.0.1:/ $NEXTNFS_MOUNT"
    local mount_out
    mount_out=$(timeout 30 mount -t nfs4 -o "vers=4.0,proto=tcp,port=${NEXTNFS_PORT},nolock,soft,timeo=50,retrans=2" \
        "127.0.0.1:/" "$NEXTNFS_MOUNT" 2>&1) || true
    echo "  mount result: $mount_out"

    if mountpoint -q "$NEXTNFS_MOUNT"; then
        ok "nextnfs mounted at $NEXTNFS_MOUNT"
    else
        fail "failed to mount nextnfs"
        echo "--- mount debug ---"
        echo "  port: $NEXTNFS_PORT"
        sleep 2  # Wait for server to flush logs
        echo "  server log (last 60 lines):"
        tail -60 /tmp/nextnfs-test.log 2>/dev/null || true
        echo "--- end ---"
        # Don't return — try wire tests directly
        echo "  Attempting wire tests without mount..."
    fi
}

stop_nextnfs() {
    if mountpoint -q "$NEXTNFS_MOUNT" 2>/dev/null; then
        umount -l "$NEXTNFS_MOUNT" 2>/dev/null || true
    fi
    if [ -f /tmp/nextnfs-test.pid ]; then
        local pid
        pid=$(cat /tmp/nextnfs-test.pid 2>/dev/null)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            sleep 1
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f /tmp/nextnfs-test.pid
    fi
}

# ── Comparison report ────────────────────────────────────────────────────────

generate_comparison_report() {
    section "Comparison Report: knfsd vs nextnfs"

    local knfsd_dir="$RESULTS_DIR/knfsd"
    local nextnfs_dir="$RESULTS_DIR/nextnfs"
    local report_file="$RESULTS_DIR/comparison.md"

    python3 - "$knfsd_dir" "$nextnfs_dir" "$report_file" << 'PYEOF'
import json
import sys
import os
from pathlib import Path

knfsd_dir = Path(sys.argv[1])
nextnfs_dir = Path(sys.argv[2])
report_file = Path(sys.argv[3])

def load_json(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def load_perf(path):
    data = load_json(path)
    if not data:
        return {}
    results = {}
    for r in data.get("results", []):
        key = f"{r['name']}/{r['metric']}"
        results[key] = r
    return results

# Load functional test results
suites = ["nfs4_basic", "nfs4_edge", "nfs4_stress", "nfs41_session"]
knfsd_func = {}
nextnfs_func = {}

for suite in suites:
    k = load_json(knfsd_dir / f"{suite}.json")
    n = load_json(nextnfs_dir / f"{suite}.json")
    if k:
        knfsd_func[suite] = k
    if n:
        nextnfs_func[suite] = n

# Load wire test results
knfsd_wire = load_json(knfsd_dir / "wire" / "report.json") if (knfsd_dir / "wire").exists() else None
nextnfs_wire = load_json(nextnfs_dir / "wire" / "report.json") if (nextnfs_dir / "wire").exists() else None

# Load performance results
knfsd_perf = load_perf(knfsd_dir / "nfs_performance.json")
nextnfs_perf = load_perf(nextnfs_dir / "nfs_performance.json")

lines = []
lines.append("# NFS Test Comparison Report: knfsd vs nextnfs")
lines.append("")
lines.append("## Functional Tests")
lines.append("")
lines.append("| Suite | knfsd Pass/Total | nextnfs Pass/Total | Delta |")
lines.append("|-------|------------------|---------------------|-------|")

total_k_pass = 0
total_k_total = 0
total_n_pass = 0
total_n_total = 0

for suite in suites:
    k = knfsd_func.get(suite, {})
    n = nextnfs_func.get(suite, {})
    kp = k.get("passed", 0)
    kt = k.get("total", 0)
    np_ = n.get("passed", 0)
    nt = n.get("total", 0)
    total_k_pass += kp
    total_k_total += kt
    total_n_pass += np_
    total_n_total += nt
    delta = np_ - kp
    delta_str = f"+{delta}" if delta > 0 else str(delta) if delta < 0 else "="
    lines.append(f"| {suite} | {kp}/{kt} | {np_}/{nt} | {delta_str} |")

delta_total = total_n_pass - total_k_pass
delta_str = f"+{delta_total}" if delta_total > 0 else str(delta_total) if delta_total < 0 else "="
lines.append(f"| **Total** | **{total_k_pass}/{total_k_total}** | **{total_n_pass}/{total_n_total}** | **{delta_str}** |")

# Wire tests
lines.append("")
lines.append("## Wire-Level Protocol Tests")
lines.append("")
if knfsd_wire or nextnfs_wire:
    kw = knfsd_wire or {}
    nw = nextnfs_wire or {}
    ks = kw.get("summary", {})
    ns = nw.get("summary", {})
    lines.append("| Metric | knfsd | nextnfs |")
    lines.append("|--------|-------|---------|")
    lines.append(f"| Total | {ks.get('total', 'N/A')} | {ns.get('total', 'N/A')} |")
    lines.append(f"| Passed | {ks.get('passed', 'N/A')} | {ns.get('passed', 'N/A')} |")
    lines.append(f"| Failed | {ks.get('failed', 'N/A')} | {ns.get('failed', 'N/A')} |")
    lines.append(f"| Skipped | {ks.get('skipped', 'N/A')} | {ns.get('skipped', 'N/A')} |")

    # List tests that pass on knfsd but fail on nextnfs
    if nextnfs_wire and knfsd_wire:
        kw_pass = {r["id"] for r in kw.get("results", []) if r.get("status") == "Pass"}
        nw_fail = {r["id"]: r.get("message", "") for r in nw.get("results", []) if r.get("status") in ("Fail", "Error")}
        regressions = [(tid, nw_fail[tid]) for tid in kw_pass if tid in nw_fail]
        if regressions:
            lines.append("")
            lines.append("### Regressions (pass on knfsd, fail on nextnfs)")
            lines.append("")
            for tid, msg in regressions:
                lines.append(f"- **{tid}**: {msg}")
else:
    lines.append("Wire test results not available.")

# Performance comparison
lines.append("")
lines.append("## Performance Comparison")
lines.append("")

if knfsd_perf or nextnfs_perf:
    all_keys = sorted(set(list(knfsd_perf.keys()) + list(nextnfs_perf.keys())))

    lines.append("| Benchmark | knfsd | nextnfs | Ratio | Unit |")
    lines.append("|-----------|-------|---------|-------|------|")

    for key in all_keys:
        kr = knfsd_perf.get(key, {})
        nr = nextnfs_perf.get(key, {})
        kv = kr.get("value", "N/A")
        nv = nr.get("value", "N/A")
        unit = kr.get("unit", nr.get("unit", ""))

        ratio = ""
        try:
            kv_f = float(kv)
            nv_f = float(nv)
            if kv_f > 0:
                pct = (nv_f / kv_f) * 100
                ratio = f"{pct:.0f}%"
        except (ValueError, TypeError):
            pass

        lines.append(f"| {key} | {kv} | {nv} | {ratio} | {unit} |")
else:
    lines.append("Performance results not available.")

# Summary
lines.append("")
lines.append("## Summary")
lines.append("")
if total_n_total > 0:
    pass_rate = (total_n_pass / total_n_total * 100) if total_n_total > 0 else 0
    lines.append(f"- nextnfs functional pass rate: **{pass_rate:.1f}%** ({total_n_pass}/{total_n_total})")
if total_k_total > 0:
    pass_rate = (total_k_pass / total_k_total * 100) if total_k_total > 0 else 0
    lines.append(f"- knfsd functional pass rate: **{pass_rate:.1f}%** ({total_k_pass}/{total_k_total})")

# Highlight key perf metrics
for key in ["seq_read/bandwidth", "seq_write/bandwidth", "rand_read/iops", "rand_write/iops"]:
    kr = knfsd_perf.get(key, {})
    nr = nextnfs_perf.get(key, {})
    try:
        kv_f = float(kr.get("value", 0))
        nv_f = float(nr.get("value", 0))
        if kv_f > 0:
            pct = (nv_f / kv_f) * 100
            lines.append(f"- {key}: nextnfs = **{pct:.0f}%** of knfsd ({nv_f:.0f} vs {kv_f:.0f} {kr.get('unit', '')})")
    except (ValueError, TypeError):
        pass

report = "\n".join(lines)
print(report)

with open(report_file, "w") as f:
    f.write(report + "\n")

print(f"\nReport saved to {report_file}")
PYEOF

    # Also print the markdown report to stdout
    if [ -f "$RESULTS_DIR/comparison.md" ]; then
        ok "comparison report saved to $RESULTS_DIR/comparison.md"
    fi
}

# ══════════════════════════════════════════════════════════════════════════════
# Main
# ══════════════════════════════════════════════════════════════════════════════

main() {
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║           nextnfs CI — Unified Test Suite                   ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "Host:   $(hostname)"
    echo "Date:   $(date)"
    echo "Arch:   $(uname -m)"
    echo "Kernel: $(uname -r)"
    echo ""

    mkdir -p "$RESULTS_DIR"

    # ── Phase 1: Build ──

    phase 1 "Build"

    section "Rust toolchain"
    rustc --version
    cargo --version

    section "Debug build (all workspace crates)"
    if cargo build --workspace 2>&1; then
        ok "debug build"
    else
        fail "debug build"
        echo "Build failed — cannot proceed"
        exit 1
    fi

    section "Verify binaries"
    if [ -f "$NEXTNFS_BIN" ]; then
        ok "nextnfs binary: $(ls -lh "$NEXTNFS_BIN" | awk '{print $5}')"
    else
        fail "nextnfs binary missing"
    fi

    if [ -f "$NEXTNFSTEST_BIN" ]; then
        ok "nextnfstest binary: $(ls -lh "$NEXTNFSTEST_BIN" | awk '{print $5}')"
    else
        fail "nextnfstest binary missing"
    fi

    # ── Phase 2: Unit tests + Clippy ──

    phase 2 "Unit Tests & Lint"

    section "cargo test"
    if cargo test --workspace 2>&1; then
        ok "unit tests"
    else
        fail "unit tests"
    fi

    section "cargo clippy"
    if cargo clippy --workspace -- -D warnings 2>&1; then
        ok "clippy"
    else
        fail "clippy"
    fi

    # ── Phase 3: Baseline — knfsd ──

    phase 3 "Baseline: kernel NFS (knfsd)"

    install_prereqs

    if start_knfsd; then
        run_wire_tests 127.0.0.1 "$KNFSD_PORT" "knfsd" "$RESULTS_DIR/knfsd/wire"
        run_shell_tests "$KNFSD_MOUNT" "$KNFSD_PORT" "knfsd" "knfsd"
        run_perf_tests "$KNFSD_MOUNT" "$KNFSD_PORT" "knfsd" "knfsd"
        stop_knfsd
    else
        echo "  Skipping knfsd baseline — server failed to start"
        skip "knfsd baseline"
    fi

    # ── Phase 4: nextnfs ──

    phase 4 "Test: nextnfs"

    # start_nextnfs starts the server and tries to mount
    # Wire tests work without mount (they use nextnfstest directly)
    # Shell/perf tests require mount
    if start_nextnfs; then
        run_wire_tests 127.0.0.1 "$NEXTNFS_PORT" "nextnfs" "$RESULTS_DIR/nextnfs/wire"
        if mountpoint -q "$NEXTNFS_MOUNT" 2>/dev/null; then
            run_shell_tests "$NEXTNFS_MOUNT" "$NEXTNFS_PORT" "nextnfs" "nextnfs"
            run_perf_tests "$NEXTNFS_MOUNT" "$NEXTNFS_PORT" "nextnfs" "nextnfs"
        else
            echo "  Skipping shell/perf tests — mount not available (container?)"
        fi
        # Show final server log state
        echo "  server log (last 20 lines):"
        tail -20 /tmp/nextnfs-test.log 2>/dev/null || true
        stop_nextnfs
    else
        echo "  Skipping nextnfs tests — server failed to start"
        fail "nextnfs server start"
    fi

    # ── Phase 5: Release build ──

    phase 5 "Release Build"

    if cargo build --release --workspace 2>&1; then
        ok "release build"
        local size
        size=$(ls -lh target/release/nextnfs 2>/dev/null | awk '{print $5}')
        local test_size
        test_size=$(ls -lh target/release/nextnfstest 2>/dev/null | awk '{print $5}')
        echo "  nextnfs:     $size"
        echo "  nextnfstest: $test_size"
    else
        fail "release build"
    fi

    # ── Phase 6: Comparison report ──

    phase 6 "Comparison Report"

    generate_comparison_report

    # ── Final Summary ──

    echo ""
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║                      Final Summary                          ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "  Results directory: $RESULTS_DIR"
    echo ""

    if [ -f "$RESULTS_DIR/comparison.md" ]; then
        echo "  Comparison report: $RESULTS_DIR/comparison.md"
    fi

    echo ""
    ls -la "$RESULTS_DIR/" 2>/dev/null || true
    echo ""

    if [ "$TOTAL_FAILURES" -eq 0 ]; then
        echo -e "  ${GREEN}${BOLD}All phases passed${RESET}"
        exit 0
    else
        echo -e "  ${RED}${BOLD}$TOTAL_FAILURES failure(s)${RESET}"
        exit 1
    fi
}

main "$@"
