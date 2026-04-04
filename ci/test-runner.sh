#!/bin/bash
# test-runner.sh — NFS test suite runner for mkube CI
#
# Mounts an NFS export from the server runner and executes the full test suite:
#   - Wire-level protocol tests (nextnfstest)
#   - Shell functional tests (nfs4_basic, nfs4_edge, nfs4_stress, nfs41_session)
#   - Performance benchmarks (fio throughput, latency, metadata, concurrency)
#   - Optional knfsd baseline comparison
#
# Usage:
#   ./ci/test-runner.sh [--server HOST] [--port PORT] [--skip-baseline]
#
# Environment:
#   NFS_SERVER       — NFS server hostname/IP (default: 127.0.0.1)
#   NFS_PORT         — NFS port (default: 2049)
#   NEXTNFS_SRC      — source tree path (default: /data/nextnfs)
#   RESULTS_DIR      — results output directory (default: /data/results)
#   SKIP_BASELINE    — set to 1 to skip knfsd baseline (default: 0)
#   BUILD_MODE       — debug or release (default: release)

set -uo pipefail

# ── Configuration ─────────────────────────────────────────────────────────────

NFS_SERVER="${NFS_SERVER:-127.0.0.1}"
NFS_PORT="${NFS_PORT:-2049}"
# mkube clones to /build; fallback to /data/nextnfs for manual runs
if [ -f "/build/Cargo.toml" ]; then
    NEXTNFS_SRC="${NEXTNFS_SRC:-/build}"
else
    NEXTNFS_SRC="${NEXTNFS_SRC:-/data/nextnfs}"
fi
RESULTS_DIR="${RESULTS_DIR:-/data/results}"
SKIP_BASELINE="${SKIP_BASELINE:-0}"
BUILD_MODE="${BUILD_MODE:-release}"

NEXTNFS_MOUNT="/mnt/nfs-test"
KNFSD_MOUNT="/mnt/nfs-baseline"
KNFSD_PORT=2049
KNFSD_EXPORT_DIR="/tmp/nfs-knfsd-export"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --server) NFS_SERVER="$2"; shift 2 ;;
        --port) NFS_PORT="$2"; shift 2 ;;
        --skip-baseline) SKIP_BASELINE=1; shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Colour output ─────────────────────────────────────────────────────────────

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

TOTAL_FAILURES=0
TOTAL_PASSES=0

# ── Prerequisites ─────────────────────────────────────────────────────────────

install_prereqs() {
    info "Installing test prerequisites..."

    if command -v dnf >/dev/null 2>&1; then
        dnf install -y nfs-utils fio python3 2>/dev/null || true
    elif command -v apt-get >/dev/null 2>&1; then
        apt-get update && apt-get install -y nfs-common fio python3 2>/dev/null || true
    fi

    local missing=0
    for cmd in mount umount fio python3; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            echo "  WARNING: $cmd not found"
            ((missing++))
        fi
    done

    if [ "$missing" -eq 0 ]; then
        ok "all prerequisites installed"
    else
        echo "  Some tools missing — tests may be limited"
    fi
}

# ── Wait for server ───────────────────────────────────────────────────────────

wait_for_server() {
    local host="$1" port="$2" timeout="${3:-60}" label="${4:-server}"
    info "Waiting for $label at $host:$port (timeout ${timeout}s)..."

    local deadline=$(( $(date +%s) + timeout ))
    while ! bash -c "echo >/dev/tcp/$host/$port" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            fail "$label: timeout waiting for $host:$port"
            return 1
        fi
        sleep 1
    done
    ok "$label listening on $host:$port"
}

# ── Mount NFS ─────────────────────────────────────────────────────────────────

mount_nfs() {
    local host="$1" port="$2" mount_point="$3" label="${4:-nfs}"
    mkdir -p "$mount_point"

    info "Mounting $label: $host:/ -> $mount_point (port=$port)"

    local rc=0
    timeout 30 mount -t nfs4 \
        -o "vers=4.0,proto=tcp,port=${port},nolock,soft,timeo=50,retrans=2" \
        "${host}:/" "$mount_point" 2>&1 || rc=$?

    if mountpoint -q "$mount_point" 2>/dev/null; then
        ok "$label mounted at $mount_point"
        return 0
    else
        fail "failed to mount $label (exit code $rc)"
        return 1
    fi
}

unmount_nfs() {
    local mount_point="$1"
    if mountpoint -q "$mount_point" 2>/dev/null; then
        umount -l "$mount_point" 2>/dev/null || umount -f "$mount_point" 2>/dev/null || true
    fi
}

# ── Wire-level tests ──────────────────────────────────────────────────────────

run_wire_tests() {
    local host="$1" port="$2" label="$3" output_dir="$4"

    echo ""
    echo -e "${BOLD}Wire-level protocol tests ($label)${RESET}"

    local bin="$NEXTNFS_SRC/target/${BUILD_MODE}/nextnfstest"
    if [ ! -f "$bin" ]; then
        skip "nextnfstest binary not found at $bin"
        return
    fi

    mkdir -p "$output_dir"

    local rc=0
    "$bin" run \
        --server "$host" \
        --port "$port" \
        --output "$output_dir" \
        2>&1 || rc=$?

    if [ "$rc" -eq 0 ]; then
        ok "wire tests passed"
        ((TOTAL_PASSES++))
    else
        fail "wire tests ($rc failures)"
        ((TOTAL_FAILURES++))
    fi
}

# ── Shell functional tests ────────────────────────────────────────────────────

run_shell_tests() {
    local mount_point="$1" port="$2" label="$3" results_prefix="$4"

    echo ""
    echo -e "${BOLD}Shell functional tests ($label)${RESET}"

    export NFS_HOST="$NFS_SERVER"
    export NFS_PORT="$port"
    export NFS_MOUNT="$mount_point"
    export NFS_EXPORT="/"
    export NFS_VERS="4.0"
    export RESULTS_DIR="$RESULTS_DIR/$results_prefix"
    export NEXTNFS_BIN="$NEXTNFS_SRC/target/${BUILD_MODE}/nextnfs"
    export NEXTNFS_PID_FILE=/tmp/nextnfs-test.pid

    mkdir -p "$RESULTS_DIR"

    local tests_dir="$NEXTNFS_SRC/tests"
    local suite rc
    for suite in nfs4_basic nfs4_edge nfs4_stress nfs41_session; do
        local script="$tests_dir/${suite}.sh"
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
            ((TOTAL_PASSES++))
        else
            fail "$suite (exit code $rc)"
            ((TOTAL_FAILURES++))
        fi
    done
}

# ── Performance benchmarks ────────────────────────────────────────────────────

run_perf_tests() {
    local mount_point="$1" port="$2" label="$3" results_prefix="$4"

    echo ""
    echo -e "${BOLD}Performance benchmarks ($label)${RESET}"

    if ! command -v fio >/dev/null 2>&1; then
        skip "fio not installed — skipping performance benchmarks"
        return
    fi

    export NFS_HOST="$NFS_SERVER"
    export NFS_PORT="$port"
    export NFS_MOUNT="$mount_point"
    export NFS_EXPORT="/"
    export NFS_VERS="4.0"
    export RESULTS_DIR="$RESULTS_DIR/$results_prefix"

    mkdir -p "$RESULTS_DIR"

    local script="$NEXTNFS_SRC/tests/nfs_performance.sh"
    if [ ! -f "$script" ]; then
        skip "nfs_performance.sh not found"
        return
    fi

    local rc=0
    bash "$script" 2>&1 || rc=$?

    if [ "$rc" -eq 0 ]; then
        ok "performance benchmarks complete"
        ((TOTAL_PASSES++))
    else
        fail "performance benchmarks (exit code $rc)"
        ((TOTAL_FAILURES++))
    fi
}

# ── knfsd Baseline ────────────────────────────────────────────────────────────

run_knfsd_baseline() {
    if [ "$SKIP_BASELINE" -eq 1 ]; then
        skip "knfsd baseline (--skip-baseline)"
        return
    fi

    echo ""
    echo -e "${BOLD}knfsd Baseline${RESET}"

    # Check if we can start knfsd
    if ! command -v exportfs >/dev/null 2>&1; then
        skip "knfsd — nfs-utils not installed"
        return
    fi

    mkdir -p "$KNFSD_EXPORT_DIR"
    chmod 777 "$KNFSD_EXPORT_DIR"

    # Configure exports
    echo "$KNFSD_EXPORT_DIR *(rw,sync,no_subtree_check,no_root_squash,fsid=0)" > /etc/exports

    # Start NFS services
    modprobe nfsd 2>/dev/null || true
    systemctl start rpcbind 2>/dev/null || rpcbind 2>/dev/null || true
    sleep 1

    if systemctl start nfs-server 2>&1; then
        ok "nfs-server.service started"
    else
        rpc.nfsd 8 2>&1 || true
        rpc.mountd 2>&1 || true
    fi
    exportfs -ra 2>&1
    sleep 2

    if ! wait_for_server 127.0.0.1 "$KNFSD_PORT" 15 "knfsd"; then
        skip "knfsd failed to start"
        return
    fi

    # Mount knfsd
    if mount_nfs 127.0.0.1 "$KNFSD_PORT" "$KNFSD_MOUNT" "knfsd"; then
        run_wire_tests 127.0.0.1 "$KNFSD_PORT" "knfsd" "$RESULTS_DIR/knfsd/wire"
        run_shell_tests "$KNFSD_MOUNT" "$KNFSD_PORT" "knfsd" "knfsd"
        run_perf_tests "$KNFSD_MOUNT" "$KNFSD_PORT" "knfsd" "knfsd"
        unmount_nfs "$KNFSD_MOUNT"
    fi

    # Stop knfsd
    exportfs -ua 2>/dev/null || true
    systemctl stop nfs-server 2>/dev/null || true
}

# ── Comparison report ─────────────────────────────────────────────────────────

generate_report() {
    echo ""
    echo -e "${BOLD}Generating comparison report...${RESET}"

    local report_file="$RESULTS_DIR/comparison.md"

    python3 - "$RESULTS_DIR" "$report_file" << 'PYEOF'
import json, sys, os
from pathlib import Path

results_dir = Path(sys.argv[1])
report_file = Path(sys.argv[2])

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

lines = ["# nextnfs CI Test Report", ""]
lines.append(f"Generated: {os.popen('date').read().strip()}")
lines.append(f"Host: {os.popen('hostname').read().strip()}")
lines.append("")

# Functional tests
suites = ["nfs4_basic", "nfs4_edge", "nfs4_stress", "nfs41_session"]
for prefix in ["nextnfs", "knfsd"]:
    total_pass = 0
    total_total = 0
    has_data = False
    for suite in suites:
        data = load_json(results_dir / prefix / f"{suite}.json")
        if data:
            has_data = True
            total_pass += data.get("passed", 0)
            total_total += data.get("total", 0)
    if has_data:
        lines.append(f"## {prefix} Functional Tests")
        lines.append(f"- Pass rate: **{total_pass}/{total_total}** ({total_pass/total_total*100:.1f}%)" if total_total > 0 else "")
        lines.append("")
        lines.append("| Suite | Passed | Failed | Skipped | Total |")
        lines.append("|-------|--------|--------|---------|-------|")
        for suite in suites:
            data = load_json(results_dir / prefix / f"{suite}.json")
            if data:
                lines.append(f"| {suite} | {data.get('passed',0)} | {data.get('failed',0)} | {data.get('skipped',0)} | {data.get('total',0)} |")
        lines.append("")

# Performance
for prefix in ["nextnfs", "knfsd"]:
    perf = load_perf(results_dir / prefix / "nfs_performance.json")
    if perf:
        lines.append(f"## {prefix} Performance")
        lines.append("")
        lines.append("| Benchmark | Value | Unit |")
        lines.append("|-----------|-------|------|")
        for key in sorted(perf.keys()):
            r = perf[key]
            lines.append(f"| {key} | {r.get('value', 'N/A')} | {r.get('unit', '')} |")
        lines.append("")

# Comparison
knfsd_perf = load_perf(results_dir / "knfsd" / "nfs_performance.json")
nextnfs_perf = load_perf(results_dir / "nextnfs" / "nfs_performance.json")
if knfsd_perf and nextnfs_perf:
    lines.append("## Performance Comparison (nextnfs vs knfsd)")
    lines.append("")
    lines.append("| Benchmark | knfsd | nextnfs | Ratio | Unit |")
    lines.append("|-----------|-------|---------|-------|------|")
    all_keys = sorted(set(list(knfsd_perf.keys()) + list(nextnfs_perf.keys())))
    for key in all_keys:
        kr = knfsd_perf.get(key, {})
        nr = nextnfs_perf.get(key, {})
        kv = kr.get("value", "N/A")
        nv = nr.get("value", "N/A")
        unit = kr.get("unit", nr.get("unit", ""))
        ratio = ""
        try:
            if float(kv) > 0:
                ratio = f"{float(nv)/float(kv)*100:.0f}%"
        except (ValueError, TypeError):
            pass
        lines.append(f"| {key} | {kv} | {nv} | {ratio} | {unit} |")
    lines.append("")

report = "\n".join(lines)
print(report)
with open(report_file, "w") as f:
    f.write(report + "\n")
print(f"\nReport saved to {report_file}")
PYEOF

    if [ -f "$report_file" ]; then
        ok "report saved to $report_file"
    fi
}

# ── Signal server done ────────────────────────────────────────────────────────

signal_server_done() {
    # For two-machine mode: signal the server runner to stop
    if [ "$NFS_SERVER" != "127.0.0.1" ] && [ "$NFS_SERVER" != "localhost" ]; then
        info "Remote server at $NFS_SERVER — send shutdown signal manually"
    fi
    # For local mode: create done file
    touch /tmp/nextnfs-server.done 2>/dev/null || true
}

# ── Cleanup ───────────────────────────────────────────────────────────────────

cleanup() {
    unmount_nfs "$NEXTNFS_MOUNT" 2>/dev/null || true
    unmount_nfs "$KNFSD_MOUNT" 2>/dev/null || true
    exportfs -ua 2>/dev/null || true
    systemctl stop nfs-server 2>/dev/null || true
    rm -rf "$KNFSD_EXPORT_DIR" 2>/dev/null || true
}
trap cleanup EXIT

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║          nextnfs Test Runner                                ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "  Host:     $(hostname)"
    echo "  Date:     $(date)"
    echo "  Server:   $NFS_SERVER:$NFS_PORT"
    echo "  Results:  $RESULTS_DIR"
    echo ""

    mkdir -p "$RESULTS_DIR"

    install_prereqs

    # Wait for nextnfs server
    if ! wait_for_server "$NFS_SERVER" "$NFS_PORT" 120 "nextnfs"; then
        fail "Server not reachable at $NFS_SERVER:$NFS_PORT"
        exit 1
    fi

    # Mount nextnfs
    if ! mount_nfs "$NFS_SERVER" "$NFS_PORT" "$NEXTNFS_MOUNT" "nextnfs"; then
        fail "Cannot mount nextnfs"
        exit 1
    fi

    # Run tests against nextnfs
    run_wire_tests "$NFS_SERVER" "$NFS_PORT" "nextnfs" "$RESULTS_DIR/nextnfs/wire"
    run_shell_tests "$NEXTNFS_MOUNT" "$NFS_PORT" "nextnfs" "nextnfs"
    run_perf_tests "$NEXTNFS_MOUNT" "$NFS_PORT" "nextnfs" "nextnfs"

    # Industry benchmark suite (fio, IOzone, Dbench, Bonnie++, SPECstorage-style)
    echo ""
    echo -e "${BOLD}Industry Benchmark Suite${RESET}"
    local bench_script="$NEXTNFS_SRC/tests/nfs_bench_suite.sh"
    if [ -f "$bench_script" ]; then
        NFS_MOUNT="$NEXTNFS_MOUNT" RESULTS_DIR="$RESULTS_DIR/nextnfs" \
            bash "$bench_script" --quick 2>&1 || fail "bench suite"
    fi

    # Data integrity validation (Linux kernel source copy + verify)
    echo ""
    echo -e "${BOLD}Data Integrity Validation${RESET}"
    local integrity_script="$NEXTNFS_SRC/tests/nfs_integrity.sh"
    if [ -f "$integrity_script" ]; then
        NFS_MOUNT="$NEXTNFS_MOUNT" RESULTS_DIR="$RESULTS_DIR/nextnfs" \
            bash "$integrity_script" --copies 10 2>&1 || fail "integrity test"
    fi

    unmount_nfs "$NEXTNFS_MOUNT"

    # Optionally run knfsd baseline (only makes sense on same machine)
    if [ "$NFS_SERVER" = "127.0.0.1" ] || [ "$NFS_SERVER" = "localhost" ]; then
        run_knfsd_baseline
    fi

    # Generate comparison report
    generate_report

    # Signal server to stop
    signal_server_done

    # ── Summary ───────────────────────────────────────────────────────────────

    echo ""
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║                      Test Summary                           ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "  Results:  $RESULTS_DIR"
    echo "  Passed:   $TOTAL_PASSES"
    echo "  Failed:   $TOTAL_FAILURES"
    echo ""

    ls -la "$RESULTS_DIR/" 2>/dev/null || true
    echo ""

    if [ "$TOTAL_FAILURES" -eq 0 ]; then
        echo -e "  ${GREEN}${BOLD}All tests passed${RESET}"
        exit 0
    else
        echo -e "  ${RED}${BOLD}$TOTAL_FAILURES failure(s)${RESET}"
        exit 1
    fi
}

main "$@"
