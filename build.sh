#!/bin/bash
set -euo pipefail

VERSION="${1:-latest}"
REGISTRY="${REGISTRY:-ghcr.io/glennswest}"
IMAGE="${REGISTRY}/nextnfs:${VERSION}"

echo "Building nextnfs ${VERSION}"

# Build for the current platform
podman build -t "${IMAGE}" .

echo "Built: ${IMAGE}"
echo ""
echo "To push: podman push ${IMAGE}"
echo "To run:  podman run -v /path/to/export:/export:z -p 2049:2049 ${IMAGE}"
