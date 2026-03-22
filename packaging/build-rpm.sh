#!/bin/bash
set -euo pipefail

ARCH="${1:?Usage: $0 <x86_64|aarch64>}"
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
TOPDIR="$(pwd)/dist/rpmbuild"

echo "Building nextnfs-${VERSION}-1.${ARCH}.rpm"

# Map arch to target triple
case "${ARCH}" in
    x86_64)  TRIPLE="x86_64-unknown-linux-musl" ;;
    aarch64) TRIPLE="aarch64-unknown-linux-musl" ;;
    *) echo "Unknown arch: ${ARCH}"; exit 1 ;;
esac

BINARY="target/${TRIPLE}/release/nextnfs"
if [ ! -f "${BINARY}" ]; then
    echo "Binary not found: ${BINARY}"
    echo "Run 'make build-x86' or 'make build-arm64' first."
    exit 1
fi

# Create rpmbuild tree
rm -rf "${TOPDIR}"
mkdir -p "${TOPDIR}"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

# Copy sources
cp "${BINARY}"                      "${TOPDIR}/SOURCES/nextnfs"
cp nextnfs.example.toml             "${TOPDIR}/SOURCES/nextnfs.toml"
cp packaging/nextnfs.service        "${TOPDIR}/SOURCES/nextnfs.service"
cp packaging/nextnfs.spec           "${TOPDIR}/SPECS/nextnfs.spec"

# Build RPM
rpmbuild \
    --define "_topdir ${TOPDIR}" \
    --define "_version ${VERSION}" \
    --target "${ARCH}" \
    -bb "${TOPDIR}/SPECS/nextnfs.spec"

# Copy output
mkdir -p dist
cp "${TOPDIR}/RPMS/${ARCH}/"*.rpm dist/
echo "Output: dist/nextnfs-${VERSION}-1.${ARCH}.rpm"
