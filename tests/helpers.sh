#!/bin/bash
# helpers.sh — Shared test utilities for nextnfs CI test suite
#
# Source this file from test scripts:
#   SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
#   source "$SCRIPT_DIR/helpers.sh"

# ── Global state ──────────────────────────────────────────────────────────────

TESTS_TOTAL=0
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0
FAILED_NAMES=()
SKIPPED_NAMES=()

# JSON results array (newline-separated JSON objects)
RESULTS_JSON=""

# Timestamps
SUITE_START_TIME=""

# Mount and server config — set by caller or ci-test.sh via env vars
: "${NFS_HOST:=127.0.0.1}"
: "${NFS_PORT:=2049}"
: "${NFS_MOUNT:=/mnt/nfs-test}"
: "${NFS_EXPORT:=/}"
: "${NFS_VERS:=4.0}"
: "${NEXTNFS_BIN:=./target/debug/nextnfs}"
: "${NEXTNFS_PID_FILE:=/tmp/nextnfs-test.pid}"
: "${EXPORT_DIR:=/tmp/nfs-test-export}"
: "${RESULTS_DIR:=/tmp/nfs-test-results}"

# ── Colour output ─────────────────────────────────────────────────────────────

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

# ── Assertion functions ───────────────────────────────────────────────────────

assert_eq() {
    local actual="$1" expected="$2" desc="${3:-assert_eq}"
    if [ "$actual" = "$expected" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc"
        echo "    expected: '$expected'"
        echo "    actual:   '$actual'"
        return 1
    fi
}

assert_ne() {
    local actual="$1" unexpected="$2" desc="${3:-assert_ne}"
    if [ "$actual" != "$unexpected" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — got unexpected value: '$actual'"
        return 1
    fi
}

assert_gt() {
    local actual="$1" threshold="$2" desc="${3:-assert_gt}"
    if [ "$actual" -gt "$threshold" ] 2>/dev/null; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — expected > $threshold, got $actual"
        return 1
    fi
}

assert_file_exists() {
    local path="$1" desc="${2:-file exists}"
    if [ -e "$path" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — file not found: $path"
        return 1
    fi
}

assert_file_not_exists() {
    local path="$1" desc="${2:-file should not exist}"
    if [ ! -e "$path" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — file unexpectedly exists: $path"
        return 1
    fi
}

assert_file_content() {
    local path="$1" expected="$2" desc="${3:-file content}"
    if [ ! -f "$path" ]; then
        echo "  ASSERT FAILED: $desc — file not found: $path"
        return 1
    fi
    local actual
    actual="$(cat "$path")"
    if [ "$actual" = "$expected" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc"
        echo "    expected content: '$expected'"
        echo "    actual content:   '$(echo "$actual" | head -c 200)'"
        return 1
    fi
}

assert_exit_code() {
    local expected_code="$1" desc="${2:-exit code}"
    shift 2
    local actual_code=0
    "$@" >/dev/null 2>&1 || actual_code=$?
    if [ "$actual_code" -eq "$expected_code" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — expected exit $expected_code, got $actual_code"
        echo "    command: $*"
        return 1
    fi
}

assert_checksum() {
    local file="$1" expected="$2" desc="${3:-checksum}"
    if [ ! -f "$file" ]; then
        echo "  ASSERT FAILED: $desc — file not found: $file"
        return 1
    fi
    local actual
    actual="$(md5sum "$file" | awk '{print $1}')"
    if [ "$actual" = "$expected" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — md5 mismatch"
        echo "    expected: $expected"
        echo "    actual:   $actual"
        return 1
    fi
}

assert_contains() {
    local haystack="$1" needle="$2" desc="${3:-contains}"
    if echo "$haystack" | grep -qF "$needle"; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — output does not contain '$needle'"
        return 1
    fi
}

assert_is_dir() {
    local path="$1" desc="${2:-is directory}"
    if [ -d "$path" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — not a directory: $path"
        return 1
    fi
}

assert_is_symlink() {
    local path="$1" desc="${2:-is symlink}"
    if [ -L "$path" ]; then
        return 0
    else
        echo "  ASSERT FAILED: $desc — not a symlink: $path"
        return 1
    fi
}

# ── Test lifecycle ────────────────────────────────────────────────────────────

suite_start() {
    local name="${1:-tests}"
    SUITE_START_TIME=$(date +%s)
    TESTS_TOTAL=0
    TESTS_PASSED=0
    TESTS_FAILED=0
    TESTS_SKIPPED=0
    FAILED_NAMES=()
    SKIPPED_NAMES=()
    RESULTS_JSON=""
    echo -e "${BOLD}━━━ $name ━━━${RESET}"
    echo ""
}

test_start() {
    local name="$1"
    CURRENT_TEST="$name"
    CURRENT_TEST_START=$(date +%s%N 2>/dev/null || date +%s)
    ((TESTS_TOTAL++))
    printf "  %-60s " "$name"
}

test_pass() {
    local name="${1:-$CURRENT_TEST}"
    local elapsed
    elapsed=$(_elapsed_ms)
    ((TESTS_PASSED++))
    echo -e "${GREEN}PASS${RESET} (${elapsed}ms)"
    _record_result "$name" "pass" "" "$elapsed"
}

test_fail() {
    local name="${1:-$CURRENT_TEST}" reason="${2:-}"
    local elapsed
    elapsed=$(_elapsed_ms)
    ((TESTS_FAILED++))
    FAILED_NAMES+=("$name")
    echo -e "${RED}FAIL${RESET} (${elapsed}ms)"
    if [ -n "$reason" ]; then
        echo -e "    ${RED}→ $reason${RESET}"
    fi
    _record_result "$name" "fail" "$reason" "$elapsed"
}

test_skip() {
    local name="${1:-$CURRENT_TEST}" reason="${2:-}"
    local elapsed
    elapsed=$(_elapsed_ms)
    ((TESTS_SKIPPED++))
    SKIPPED_NAMES+=("$name")
    echo -e "${YELLOW}SKIP${RESET}"
    if [ -n "$reason" ]; then
        echo -e "    ${YELLOW}→ $reason${RESET}"
    fi
    _record_result "$name" "skip" "$reason" "0"
}

# Run a test function, capturing pass/fail automatically
run_test() {
    local name="$1"
    local func="$2"
    test_start "$name"
    local output=""
    local rc=0
    output=$("$func" 2>&1) || rc=$?
    if [ $rc -eq 0 ]; then
        test_pass "$name"
    else
        test_fail "$name" "$output"
    fi
}

_elapsed_ms() {
    local now
    now=$(date +%s%N 2>/dev/null || date +%s)
    if [ ${#now} -gt 10 ]; then
        # nanosecond precision available
        echo $(( (now - CURRENT_TEST_START) / 1000000 ))
    else
        echo $(( (now - CURRENT_TEST_START) * 1000 ))
    fi
}

_record_result() {
    local name="$1" status="$2" reason="$3" elapsed="$4"
    local json
    reason="${reason//\"/\\\"}"
    reason="${reason//$'\n'/\\n}"
    json="{\"name\":\"$name\",\"status\":\"$status\",\"reason\":\"$reason\",\"elapsed_ms\":$elapsed}"
    if [ -n "$RESULTS_JSON" ]; then
        RESULTS_JSON="$RESULTS_JSON
$json"
    else
        RESULTS_JSON="$json"
    fi
}

# ── Summary and reporting ────────────────────────────────────────────────────

print_summary() {
    local suite_name="${1:-Test Suite}"
    local suite_end
    suite_end=$(date +%s)
    local suite_elapsed=$(( suite_end - SUITE_START_TIME ))

    echo ""
    echo -e "${BOLD}━━━ $suite_name Summary ━━━${RESET}"
    echo -e "  Total:   $TESTS_TOTAL"
    echo -e "  ${GREEN}Passed:  $TESTS_PASSED${RESET}"
    if [ "$TESTS_FAILED" -gt 0 ]; then
        echo -e "  ${RED}Failed:  $TESTS_FAILED${RESET}"
        for name in "${FAILED_NAMES[@]}"; do
            echo -e "    ${RED}✗ $name${RESET}"
        done
    else
        echo -e "  Failed:  0"
    fi
    if [ "$TESTS_SKIPPED" -gt 0 ]; then
        echo -e "  ${YELLOW}Skipped: $TESTS_SKIPPED${RESET}"
    fi
    echo -e "  Duration: ${suite_elapsed}s"
    echo ""

    return $TESTS_FAILED
}

save_results_json() {
    local file="$1" suite_name="${2:-tests}"
    mkdir -p "$(dirname "$file")"
    {
        echo "{"
        echo "  \"suite\": \"$suite_name\","
        echo "  \"total\": $TESTS_TOTAL,"
        echo "  \"passed\": $TESTS_PASSED,"
        echo "  \"failed\": $TESTS_FAILED,"
        echo "  \"skipped\": $TESTS_SKIPPED,"
        echo "  \"tests\": ["
        local first=true
        while IFS= read -r line; do
            if [ -n "$line" ]; then
                if $first; then
                    first=false
                else
                    echo ","
                fi
                printf "    %s" "$line"
            fi
        done <<< "$RESULTS_JSON"
        echo ""
        echo "  ]"
        echo "}"
    } > "$file"
}

# ── NFS helpers ──────────────────────────────────────────────────────────────

nfs_mount() {
    local vers="${1:-$NFS_VERS}"
    local export_path="${2:-$NFS_EXPORT}"
    local mount_point="${3:-$NFS_MOUNT}"

    mkdir -p "$mount_point"

    # Build mount options
    local opts="vers=${vers},proto=tcp,port=${NFS_PORT},mountport=${NFS_PORT}"
    # For v4+, we don't need separate mountd
    opts="${opts},soft,timeo=50,retrans=2"

    mount -t nfs4 -o "$opts" "${NFS_HOST}:${export_path}" "$mount_point"
}

nfs_unmount() {
    local mount_point="${1:-$NFS_MOUNT}"
    if mountpoint -q "$mount_point" 2>/dev/null; then
        # lazy unmount in case of stale handles
        umount -l "$mount_point" 2>/dev/null || umount -f "$mount_point" 2>/dev/null || true
    fi
}

wait_for_port() {
    local host="${1:-127.0.0.1}" port="${2:-2049}" timeout="${3:-10}"
    local deadline=$(( $(date +%s) + timeout ))
    while ! bash -c "echo >/dev/tcp/$host/$port" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "ERROR: timeout waiting for $host:$port after ${timeout}s"
            return 1
        fi
        sleep 0.2
    done
}

start_nextnfs() {
    local export_dir="${1:-$EXPORT_DIR}"
    local listen="${2:-127.0.0.1:$NFS_PORT}"
    local api_listen="${3:-127.0.0.1:8080}"

    mkdir -p "$export_dir"

    echo "Starting nextnfs: $NEXTNFS_BIN --export $export_dir --listen $listen --api-listen $api_listen"
    RUST_LOG=info "$NEXTNFS_BIN" \
        --export "$export_dir" \
        --listen "$listen" \
        --api-listen "$api_listen" \
        > /tmp/nextnfs-test.log 2>&1 &

    local pid=$!
    echo "$pid" > "$NEXTNFS_PID_FILE"
    echo "nextnfs started with PID $pid"

    # Wait for NFS port to accept connections
    if ! wait_for_port "$NFS_HOST" "$NFS_PORT" 10; then
        echo "ERROR: nextnfs failed to start"
        cat /tmp/nextnfs-test.log
        return 1
    fi
    echo "nextnfs is ready on $listen"
}

stop_nextnfs() {
    if [ -f "$NEXTNFS_PID_FILE" ]; then
        local pid
        pid=$(cat "$NEXTNFS_PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            # Wait for exit
            local i=0
            while kill -0 "$pid" 2>/dev/null && [ $i -lt 20 ]; do
                sleep 0.25
                ((i++))
            done
            # Force kill if still running
            if kill -0 "$pid" 2>/dev/null; then
                kill -9 "$pid" 2>/dev/null || true
            fi
        fi
        rm -f "$NEXTNFS_PID_FILE"
    fi
}

# ── knfsd helpers ────────────────────────────────────────────────────────────

start_knfsd() {
    local export_dir="${1:-$EXPORT_DIR}"
    mkdir -p "$export_dir"

    # Configure exports
    echo "$export_dir *(rw,sync,no_subtree_check,no_root_squash,fsid=0)" > /etc/exports

    # Start NFS services
    systemctl start nfs-server 2>/dev/null || {
        # Fallback: manual start for containers
        rpc.nfsd 8
        rpc.mountd
        exportfs -ra
    }
    exportfs -ra

    echo "knfsd started, exporting $export_dir"
}

stop_knfsd() {
    exportfs -ua 2>/dev/null || true
    systemctl stop nfs-server 2>/dev/null || {
        killall rpc.nfsd 2>/dev/null || true
        killall rpc.mountd 2>/dev/null || true
    }
}

# ── Cleanup helpers ──────────────────────────────────────────────────────────

# Clean the NFS mount point content (not unmount)
clean_mount() {
    local mount_point="${1:-$NFS_MOUNT}"
    if mountpoint -q "$mount_point" 2>/dev/null; then
        rm -rf "${mount_point:?}/"* 2>/dev/null || true
    fi
}

# Full cleanup: unmount, stop server, remove temp dirs
full_cleanup() {
    nfs_unmount "$NFS_MOUNT" 2>/dev/null || true
    nfs_unmount "/mnt/nfs-baseline" 2>/dev/null || true
    nfs_unmount "/mnt/nfs-test2" 2>/dev/null || true
    stop_nextnfs 2>/dev/null || true
    stop_knfsd 2>/dev/null || true
    rm -rf "$EXPORT_DIR" 2>/dev/null || true
}

# ── Utility functions ────────────────────────────────────────────────────────

# Generate random data file
gen_random_file() {
    local path="$1" size_bytes="$2"
    dd if=/dev/urandom of="$path" bs=1M count=$(( size_bytes / 1048576 )) 2>/dev/null ||
    dd if=/dev/urandom of="$path" bs="$size_bytes" count=1 2>/dev/null
}

# Get file size in bytes
file_size() {
    stat -c %s "$1" 2>/dev/null || stat -f %z "$1" 2>/dev/null
}

# Get file mode (octal)
file_mode() {
    stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1" 2>/dev/null
}

# Get inode number
file_inode() {
    stat -c %i "$1" 2>/dev/null || stat -f %i "$1" 2>/dev/null
}

# Get hard link count
file_nlink() {
    stat -c %h "$1" 2>/dev/null || stat -f %l "$1" 2>/dev/null
}

# Get file type (file, directory, symlink)
file_type() {
    if [ -L "$1" ]; then
        echo "symlink"
    elif [ -d "$1" ]; then
        echo "directory"
    elif [ -f "$1" ]; then
        echo "file"
    else
        echo "unknown"
    fi
}

# Check if a command exists
has_cmd() {
    command -v "$1" >/dev/null 2>&1
}

# Format bytes into human-readable
human_bytes() {
    local bytes="$1"
    if [ "$bytes" -ge 1073741824 ]; then
        echo "$(( bytes / 1073741824 )) GB"
    elif [ "$bytes" -ge 1048576 ]; then
        echo "$(( bytes / 1048576 )) MB"
    elif [ "$bytes" -ge 1024 ]; then
        echo "$(( bytes / 1024 )) KB"
    else
        echo "$bytes B"
    fi
}

# Measure wall time of a command in milliseconds
measure_ms() {
    local start end
    start=$(date +%s%N 2>/dev/null || date +%s)
    "$@" >/dev/null 2>&1
    end=$(date +%s%N 2>/dev/null || date +%s)
    if [ ${#start} -gt 10 ]; then
        echo $(( (end - start) / 1000000 ))
    else
        echo $(( (end - start) * 1000 ))
    fi
}
