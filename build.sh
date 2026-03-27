#!/bin/bash
set -euo pipefail

ARCH="${1:-x86}"
REGISTRY="${REGISTRY:-registry.gt.lo:5000}"
IMAGE="${REGISTRY}/nextnfs"
VERSION="0.10.1"

echo "Building nextnfs ${VERSION} for ${ARCH}"

case "${ARCH}" in
    x86|x86_64|amd64)
        cargo build --release --target x86_64-unknown-linux-musl
        x86_64-linux-musl-strip target/x86_64-unknown-linux-musl/release/nextnfs
        podman build --format docker --tls-verify=false \
            -f Containerfile.x86_64 \
            -t "${IMAGE}:${VERSION}" -t "${IMAGE}:latest" .
        ;;
    arm64|aarch64)
        cargo build --release --target aarch64-unknown-linux-musl
        aarch64-linux-musl-strip target/aarch64-unknown-linux-musl/release/nextnfs
        podman build --format docker --tls-verify=false \
            -f Containerfile \
            -t "${IMAGE}:${VERSION}" -t "${IMAGE}:latest" .
        ;;
    *)
        echo "Unknown arch: ${ARCH}. Use x86 or arm64."
        exit 1
        ;;
esac

echo ""
echo "Built: ${IMAGE}:${VERSION}"
echo "Push:  podman push --tls-verify=false ${IMAGE}:${VERSION}"
echo "Run:   podman run -d -v /export:/export:z -p 2049:2049 -p 9080:9080 -p 2222:22 ${IMAGE}:${VERSION}"
