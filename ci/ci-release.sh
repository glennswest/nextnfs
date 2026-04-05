#!/bin/bash
# ci-release.sh — Build RPM and upload to GitHub release
#
# Builds the RPM via ci-rpm.sh, then uploads the RPM and binary
# as assets to the corresponding GitHub release.
#
# Required env vars:
#   GH_TOKEN — GitHub personal access token with repo/release scope
#
# Usage: ./ci/ci-release.sh
#
# Designed for mkube job runners (Fedora/RHEL x86_64).

set -euo pipefail

# ── Find source ──────────────────────────────────────────────────────────────

if [ -f "/build/Cargo.toml" ]; then
    SRC="/build"
elif [ -f "$(cd "$(dirname "$0")/.." && pwd)/Cargo.toml" ]; then
    SRC="$(cd "$(dirname "$0")/.." && pwd)"
else
    echo "FATAL: Cannot find nextnfs source"
    exit 1
fi

cd "$SRC"

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
REPO="glennswest/nextnfs"
TAG="v${VERSION}"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║       nextnfs Release Builder — ${TAG}                       ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# ── Validate GH_TOKEN ────────────────────────────────────────────────────────

if [ -z "${GH_TOKEN:-}" ]; then
    echo "FATAL: GH_TOKEN environment variable is required"
    exit 1
fi

echo "  Token:   present (${#GH_TOKEN} chars)"
echo "  Repo:    $REPO"
echo "  Tag:     $TAG"
echo ""

# ── Phase 1: Build RPM ──────────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 1: Build RPM"
echo "═══════════════════════════════════════════════════════════════"
echo ""

bash ci-rpm.sh

RPM_FILE=$(ls dist/nextnfs-${VERSION}-*.x86_64.rpm 2>/dev/null | head -1)
BINARY="target/x86_64-unknown-linux-musl/release/nextnfs"

if [ -z "$RPM_FILE" ]; then
    echo "FATAL: RPM not found in dist/"
    exit 1
fi

echo ""
echo "  RPM:    $RPM_FILE ($(ls -lh "$RPM_FILE" | awk '{print $5}'))"
echo "  Binary: $BINARY ($(ls -lh "$BINARY" | awk '{print $5}'))"
echo ""

# ── Phase 2: Get release ID ─────────────────────────────────────────────────

echo "═══════════════════════════════════════════════════════════════"
echo "  Phase 2: Upload to GitHub Release"
echo "═══════════════════════════════════════════════════════════════"
echo ""

# Get release ID from tag
RELEASE_JSON=$(curl -sf \
    -H "Authorization: token ${GH_TOKEN}" \
    -H "Accept: application/vnd.github.v3+json" \
    "https://api.github.com/repos/${REPO}/releases/tags/${TAG}" 2>&1) || {
    echo "FATAL: Could not find release for tag ${TAG}"
    echo "  Create it first: gh release create ${TAG}"
    exit 1
}

RELEASE_ID=$(echo "$RELEASE_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])" 2>/dev/null) || {
    # Fallback: grep for id
    RELEASE_ID=$(echo "$RELEASE_JSON" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
}

if [ -z "$RELEASE_ID" ]; then
    echo "FATAL: Could not parse release ID"
    exit 1
fi

echo "  Release ID: $RELEASE_ID"

# ── Upload RPM ───────────────────────────────────────────────────────────────

upload_asset() {
    local file="$1"
    local name="$2"
    local content_type="$3"

    echo "  Uploading: $name ..."

    # Delete existing asset with same name (if re-running)
    EXISTING=$(curl -sf \
        -H "Authorization: token ${GH_TOKEN}" \
        -H "Accept: application/vnd.github.v3+json" \
        "https://api.github.com/repos/${REPO}/releases/${RELEASE_ID}/assets" 2>/dev/null \
        | python3 -c "
import sys, json
for a in json.load(sys.stdin):
    if a['name'] == '$name':
        print(a['id'])
        break
" 2>/dev/null) || true

    if [ -n "$EXISTING" ]; then
        echo "    Replacing existing asset (id=$EXISTING)"
        curl -sf \
            -X DELETE \
            -H "Authorization: token ${GH_TOKEN}" \
            "https://api.github.com/repos/${REPO}/releases/assets/${EXISTING}" >/dev/null 2>&1 || true
    fi

    # Upload
    HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
        -X POST \
        -H "Authorization: token ${GH_TOKEN}" \
        -H "Content-Type: ${content_type}" \
        --data-binary "@${file}" \
        "https://uploads.github.com/repos/${REPO}/releases/${RELEASE_ID}/assets?name=${name}")

    if [ "$HTTP_CODE" = "201" ]; then
        echo "    OK ($HTTP_CODE)"
    else
        echo "    FAIL (HTTP $HTTP_CODE)"
        return 1
    fi
}

RPM_NAME=$(basename "$RPM_FILE")
upload_asset "$RPM_FILE" "$RPM_NAME" "application/x-rpm"
upload_asset "$BINARY" "nextnfs-${VERSION}-linux-x86_64" "application/octet-stream"

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                    Release Complete                         ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Release: https://github.com/${REPO}/releases/tag/${TAG}"
echo "  Assets:"
echo "    - $RPM_NAME"
echo "    - nextnfs-${VERSION}-linux-x86_64"
echo ""
