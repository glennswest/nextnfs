#!/bin/bash
# ci-rpm.sh — Build nextnfs RPM package on Linux
#
# Builds from source on the runner, produces an RPM in dist/.
# Designed for mkube job runners (Fedora/RHEL x86_64).
#
# Usage: ./ci-rpm.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
ARCH=$(uname -m)
TOPDIR="$SCRIPT_DIR/dist/rpmbuild"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          nextnfs RPM Builder — v${VERSION}                       ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Host:   $(hostname)"
echo "  Date:   $(date)"
echo "  Arch:   $ARCH"
echo "  Kernel: $(uname -r)"
echo ""

# ── Install build dependencies ───────────────────────────────────────────────

echo "==> Installing build dependencies..."
if command -v dnf >/dev/null 2>&1; then
    dnf install -y gcc git rpm-build musl-gcc 2>/dev/null || true
elif command -v apt-get >/dev/null 2>&1; then
    apt-get update && apt-get install -y gcc git rpm musl-tools 2>/dev/null || true
fi

# ── Install Rust if needed ───────────────────────────────────────────────────

if ! command -v rustup >/dev/null 2>&1; then
    echo "==> Installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    source "$HOME/.cargo/env"
fi

echo "  rustc: $(rustc --version)"
echo "  cargo: $(cargo --version)"

# ── Add musl target ─────────────────────────────────────────────────────────

echo "==> Adding musl target..."
rustup target add x86_64-unknown-linux-musl

# ── Run tests first ─────────────────────────────────────────────────────────

echo "==> Running unit tests..."
if cargo test --workspace 2>&1; then
    echo "  OK: all tests passed"
else
    echo "  WARN: some tests failed (continuing with build)"
fi

echo "==> Running clippy..."
if cargo clippy --workspace -- -D warnings 2>&1; then
    echo "  OK: clippy clean"
else
    echo "  WARN: clippy warnings (continuing with build)"
fi

# ── Build static musl binary ────────────────────────────────────────────────

echo "==> Building static musl binary (release)..."
cargo build --release --target x86_64-unknown-linux-musl 2>&1

BINARY="target/x86_64-unknown-linux-musl/release/nextnfs"
if [ ! -f "$BINARY" ]; then
    echo "FATAL: Binary not found at $BINARY"
    exit 1
fi

# Strip if strip is available
if command -v strip >/dev/null 2>&1; then
    strip "$BINARY" 2>/dev/null || true
fi

echo "  Binary: $(ls -lh "$BINARY" | awk '{print $5}')"
echo "  Type:   $(file "$BINARY")"

# ── Build RPM ────────────────────────────────────────────────────────────────

echo "==> Building RPM..."

rm -rf "$TOPDIR"
mkdir -p "${TOPDIR}"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

cp "$BINARY"                       "${TOPDIR}/SOURCES/nextnfs"
cp nextnfs.example.toml            "${TOPDIR}/SOURCES/nextnfs.toml"
cp packaging/nextnfs.service       "${TOPDIR}/SOURCES/nextnfs.service"
cp packaging/nextnfs.spec          "${TOPDIR}/SPECS/nextnfs.spec"

rpmbuild \
    --define "_topdir ${TOPDIR}" \
    --define "_version ${VERSION}" \
    --target "${ARCH}" \
    -bb "${TOPDIR}/SPECS/nextnfs.spec"

# ── Collect output ───────────────────────────────────────────────────────────

mkdir -p dist
cp "${TOPDIR}/RPMS/${ARCH}/"*.rpm dist/

RPM_FILE=$(ls dist/nextnfs-*.rpm 2>/dev/null | head -1)
if [ -z "$RPM_FILE" ]; then
    echo "FATAL: RPM not found in dist/"
    exit 1
fi

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                      Build Complete                         ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  RPM:     $RPM_FILE"
echo "  Size:    $(ls -lh "$RPM_FILE" | awk '{print $5}')"
echo "  Binary:  $(ls -lh "$BINARY" | awk '{print $5}')"
echo ""
echo "  Install: rpm -Uvh $RPM_FILE"
echo "  Start:   systemctl start nextnfs"
echo "  Status:  systemctl status nextnfs"
echo ""
rpm -qip "$RPM_FILE" 2>/dev/null || true
