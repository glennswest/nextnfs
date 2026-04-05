#!/bin/bash
# ci-build-test.sh — Build and test nextnfs on Linux
#
# Runs: cargo build, cargo test, cargo clippy
# Exits with 0 on success, 1 on failure.
# Designed for mkube job runner (buildImage container).

set -euo pipefail

# ── Colour output ────────────────────────────────────────────────────────────

if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; CYAN='\033[0;36m'
    BOLD='\033[1m'; RESET='\033[0m'
else
    GREEN='' RED='' CYAN='' BOLD='' RESET=''
fi

ok()   { echo -e "  ${GREEN}OK${RESET}: $1"; }
fail() { echo -e "  ${RED}FAIL${RESET}: $1"; }
info() { echo -e "  ${CYAN}INFO${RESET}: $1"; }

# ── Find source ──────────────────────────────────────────────────────────────

if [ -f "/build/Cargo.toml" ]; then
    SRC="/build"
elif [ -f "/data/nextnfs/Cargo.toml" ]; then
    SRC="/data/nextnfs"
else
    fail "Cannot find nextnfs source (checked /build and /data/nextnfs)"
    exit 1
fi

cd "$SRC"

echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║          nextnfs Build + Test                               ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""
echo "  Host:    $(hostname)"
echo "  Date:    $(date)"
echo "  Arch:    $(uname -m)"
echo "  Kernel:  $(uname -r)"
echo "  Source:  $SRC"
echo ""

# ── Install Rust if needed ───────────────────────────────────────────────────

if ! command -v cargo >/dev/null 2>&1; then
    info "Installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    source "$HOME/.cargo/env"
    ok "Rust $(rustc --version) installed"
else
    ok "Rust $(rustc --version)"
fi

# ── Build ────────────────────────────────────────────────────────────────────

ERRORS=0

echo -e "${BOLD}Building workspace...${RESET}"
if cargo build --workspace 2>&1; then
    ok "cargo build succeeded"
else
    fail "cargo build failed"
    ERRORS=$((ERRORS + 1))
fi

# ── Test ─────────────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}Running tests...${RESET}"
if cargo test --workspace 2>&1; then
    ok "all tests passed"
else
    fail "some tests failed"
    ERRORS=$((ERRORS + 1))
fi

# ── Clippy ───────────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}Running clippy...${RESET}"
if cargo clippy --workspace -- -D warnings 2>&1; then
    ok "clippy clean"
else
    fail "clippy warnings found"
    ERRORS=$((ERRORS + 1))
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
if [ $ERRORS -eq 0 ]; then
    echo -e "${GREEN}${BOLD}All checks passed${RESET}"
    exit 0
else
    echo -e "${RED}${BOLD}$ERRORS check(s) failed${RESET}"
    exit 1
fi
