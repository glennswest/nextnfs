#!/bin/bash
set -euo pipefail

echo "=== nextnfs CI Build & Test ==="
echo "Host: $(hostname)"
echo "Date: $(date)"
echo "Arch: $(uname -m)"
echo "Kernel: $(uname -r)"
echo ""

# Show Rust toolchain
echo "=== Rust Toolchain ==="
rustc --version
cargo --version
echo ""

# Build (debug — faster for CI)
echo "=== Building (debug) ==="
cargo build 2>&1
echo "Build OK"
echo ""

# Run tests
echo "=== Running Tests ==="
cargo test 2>&1
echo "Tests OK"
echo ""

# Run clippy
echo "=== Clippy ==="
cargo clippy -- -D warnings 2>&1
echo "Clippy OK"
echo ""

# Build release to verify it compiles
echo "=== Building (release) ==="
cargo build --release 2>&1
echo "Release build OK"
echo ""

# Show binary size
ls -lh target/release/nextnfs 2>/dev/null || true

echo ""
echo "=== All checks passed ==="
