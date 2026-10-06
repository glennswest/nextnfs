#!/bin/bash
set -euo pipefail

ARCH="${1:?Usage: $0 <amd64|arm64>}"
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
STAGING="$(pwd)/dist/deb-staging"

echo "Building nextnfs_${VERSION}_${ARCH}.deb"

# Map arch to target triple
case "${ARCH}" in
    amd64)  TRIPLE="x86_64-unknown-linux-musl" ;;
    arm64)  TRIPLE="aarch64-unknown-linux-musl" ;;
    *) echo "Unknown arch: ${ARCH}. Use amd64 or arm64."; exit 1 ;;
esac

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
BINARY="${TARGET_DIR}/${TRIPLE}/release/nextnfs"
STRESS_BINARY="${TARGET_DIR}/${TRIPLE}/release/nextnfs-stress"
for bin in "${BINARY}" "${STRESS_BINARY}"; do
    if [ ! -f "${bin}" ]; then
        echo "Binary not found: ${bin}"
        echo "Run 'make build-x86' or 'make build-arm64' first."
        exit 1
    fi
done

# Create staging tree
rm -rf "${STAGING}"
mkdir -p "${STAGING}/usr/bin"
mkdir -p "${STAGING}/etc/nextnfs"
mkdir -p "${STAGING}/usr/lib/systemd/system"
mkdir -p "${STAGING}/DEBIAN"
# nextnfs.service has ReadWritePaths=/export /var/lib/nextnfs and the shipped
# config exports /export with state_dir /var/lib/nextnfs, so the package owns
# both directories (like the RPM's %dir entries) (#96)
install -d -m 0755 "${STAGING}/export"
install -d -m 0755 "${STAGING}/var/lib/nextnfs"

# Copy files
cp "${BINARY}"                      "${STAGING}/usr/bin/nextnfs"
chmod 755                           "${STAGING}/usr/bin/nextnfs"
cp "${STRESS_BINARY}"               "${STAGING}/usr/bin/nextnfs-stress"
chmod 755                           "${STAGING}/usr/bin/nextnfs-stress"
cp nextnfs.example.toml             "${STAGING}/etc/nextnfs/nextnfs.toml"
cp packaging/nextnfs.service        "${STAGING}/usr/lib/systemd/system/nextnfs.service"

# Copy and fill in control files
sed -e "s/VERSION/${VERSION}/" -e "s/ARCH/${ARCH}/" \
    packaging/deb/control > "${STAGING}/DEBIAN/control"
cp packaging/deb/conffiles          "${STAGING}/DEBIAN/conffiles"

for script in postinst prerm postrm; do
    cp "packaging/deb/${script}"    "${STAGING}/DEBIAN/${script}"
    chmod 755                       "${STAGING}/DEBIAN/${script}"
done

# Build .deb
mkdir -p dist
dpkg-deb --build "${STAGING}" "dist/nextnfs_${VERSION}_${ARCH}.deb"
echo "Output: dist/nextnfs_${VERSION}_${ARCH}.deb"
