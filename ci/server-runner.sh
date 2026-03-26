#!/bin/bash
# server-runner.sh — NFS server runner for mkube CI
#
# Builds nextnfs from source and starts it as an NFS server.
# Designed to run on an mkube job runner node (Linux x86_64).
#
# Usage:
#   ./ci/server-runner.sh [--export-dir DIR] [--listen ADDR] [--api-listen ADDR]
#
# Environment:
#   NEXTNFS_SRC    — path to nextnfs source tree (default: /data/nextnfs)
#   NEXTNFS_EXPORT — export directory (default: /data/export)
#   NEXTNFS_LISTEN — NFS listen address (default: 0.0.0.0:2049)
#   NEXTNFS_API    — REST API listen address (default: 0.0.0.0:8080)
#   BUILD_MODE     — debug or release (default: release)

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────────────

NEXTNFS_SRC="${NEXTNFS_SRC:-/data/nextnfs}"
NEXTNFS_EXPORT="${NEXTNFS_EXPORT:-/data/export}"
NEXTNFS_LISTEN="${NEXTNFS_LISTEN:-0.0.0.0:2049}"
NEXTNFS_API="${NEXTNFS_API:-0.0.0.0:8080}"
BUILD_MODE="${BUILD_MODE:-release}"
PID_FILE="/tmp/nextnfs-server.pid"
LOG_FILE="/tmp/nextnfs-server.log"
READY_FILE="/tmp/nextnfs-server.ready"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --export-dir) NEXTNFS_EXPORT="$2"; shift 2 ;;
        --listen) NEXTNFS_LISTEN="$2"; shift 2 ;;
        --api-listen) NEXTNFS_API="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Colour output ─────────────────────────────────────────────────────────────

if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; CYAN='\033[0;36m'
    BOLD='\033[1m'; RESET='\033[0m'
else
    GREEN='' RED='' CYAN='' BOLD='' RESET=''
fi

ok()   { echo -e "  ${GREEN}OK${RESET}: $1"; }
fail() { echo -e "  ${RED}FAIL${RESET}: $1"; }
info() { echo -e "  ${CYAN}INFO${RESET}: $1"; }

# ── Prerequisites ─────────────────────────────────────────────────────────────

install_rust() {
    if command -v rustup >/dev/null 2>&1; then
        ok "rustup already installed"
        return
    fi
    info "Installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    source "$HOME/.cargo/env"
    ok "Rust $(rustc --version) installed"
}

install_deps() {
    info "Installing build dependencies..."
    if command -v dnf >/dev/null 2>&1; then
        dnf install -y gcc git 2>/dev/null || true
    elif command -v apt-get >/dev/null 2>&1; then
        apt-get update && apt-get install -y gcc git 2>/dev/null || true
    fi
}

# ── Build ─────────────────────────────────────────────────────────────────────

build_nextnfs() {
    echo -e "${BOLD}Building nextnfs (${BUILD_MODE})...${RESET}"
    cd "$NEXTNFS_SRC"

    local build_flags=""
    if [ "$BUILD_MODE" = "release" ]; then
        build_flags="--release"
    fi

    if cargo build --workspace $build_flags 2>&1; then
        local bin_path="target/${BUILD_MODE}/nextnfs"
        if [ -f "$bin_path" ]; then
            ok "nextnfs built: $(ls -lh "$bin_path" | awk '{print $5}')"
        else
            fail "nextnfs binary not found at $bin_path"
            exit 1
        fi
    else
        fail "cargo build failed"
        exit 1
    fi
}

# ── Unit Tests ────────────────────────────────────────────────────────────────

run_unit_tests() {
    echo -e "${BOLD}Running unit tests...${RESET}"
    cd "$NEXTNFS_SRC"

    if cargo test --workspace 2>&1; then
        ok "all unit tests passed"
    else
        fail "unit tests failed"
        # Continue anyway — server can still run
    fi
}

run_clippy() {
    echo -e "${BOLD}Running clippy...${RESET}"
    cd "$NEXTNFS_SRC"

    if cargo clippy --workspace -- -D warnings 2>&1; then
        ok "clippy clean"
    else
        fail "clippy warnings found"
    fi
}

# ── Start Server ──────────────────────────────────────────────────────────────

start_server() {
    echo -e "${BOLD}Starting nextnfs server...${RESET}"

    mkdir -p "$NEXTNFS_EXPORT"
    chmod 777 "$NEXTNFS_EXPORT"

    local bin_path="$NEXTNFS_SRC/target/${BUILD_MODE}/nextnfs"

    info "Binary:  $bin_path"
    info "Export:  $NEXTNFS_EXPORT"
    info "Listen:  $NEXTNFS_LISTEN"
    info "API:     $NEXTNFS_API"

    RUST_LOG=info "$bin_path" \
        --export "$NEXTNFS_EXPORT" \
        --listen "$NEXTNFS_LISTEN" \
        --api-listen "$NEXTNFS_API" \
        > "$LOG_FILE" 2>&1 &

    echo $! > "$PID_FILE"
    local pid
    pid=$(cat "$PID_FILE")
    info "PID: $pid"

    # Wait for NFS port
    local port="${NEXTNFS_LISTEN##*:}"
    local host="${NEXTNFS_LISTEN%%:*}"
    local deadline=$(( $(date +%s) + 30 ))

    while ! bash -c "echo >/dev/tcp/$host/$port" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            fail "nextnfs failed to start within 30s"
            echo "--- server log ---"
            cat "$LOG_FILE" 2>/dev/null || true
            echo "--- end log ---"
            exit 1
        fi
        sleep 0.2
    done

    ok "nextnfs listening on $NEXTNFS_LISTEN"

    # Signal readiness for test runner
    echo "ready" > "$READY_FILE"
    info "Ready file: $READY_FILE"
}

# ── Wait Mode ─────────────────────────────────────────────────────────────────

wait_for_tests() {
    echo -e "${BOLD}Server running. Waiting for test completion signal...${RESET}"
    info "Send SIGTERM or create /tmp/nextnfs-server.done to stop"

    # Trap SIGTERM to clean shutdown
    trap 'echo "Received shutdown signal"; stop_server; exit 0' TERM INT

    # Wait until done file appears or server dies
    while true; do
        if [ -f /tmp/nextnfs-server.done ]; then
            info "Test completion signal received"
            break
        fi
        if [ -f "$PID_FILE" ]; then
            local pid
            pid=$(cat "$PID_FILE")
            if ! kill -0 "$pid" 2>/dev/null; then
                fail "nextnfs process died unexpectedly"
                echo "--- server log (last 50 lines) ---"
                tail -50 "$LOG_FILE" 2>/dev/null || true
                echo "--- end log ---"
                exit 1
            fi
        fi
        sleep 5
    done
}

stop_server() {
    if [ -f "$PID_FILE" ]; then
        local pid
        pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            sleep 2
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f "$PID_FILE"
    fi
    rm -f "$READY_FILE"
    ok "server stopped"
}

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║          nextnfs Server Runner                              ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo "  Host:   $(hostname)"
    echo "  Date:   $(date)"
    echo "  Arch:   $(uname -m)"
    echo "  Kernel: $(uname -r)"
    echo ""

    install_deps
    install_rust
    build_nextnfs
    run_unit_tests
    run_clippy
    start_server
    wait_for_tests
    stop_server

    echo ""
    echo -e "${GREEN}${BOLD}Server runner complete${RESET}"
}

main "$@"
