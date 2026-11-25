#!/bin/sh
# Build nextnfs's test image context for the commit checked out (#133).
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, with cargo. It builds two static binaries and stages them
# in test/.stage/, which test/Containerfile (context: the repo root)
# packages FROM scratch:
# - nextnfs-server-tests: the nextnfs-server library's test binary at this
#   commit (`cargo test --no-run`);
# - test: the runner, test/runner/main.rs (std only, built with rustc).
set -eu
target=${1:-x86_64-unknown-linux-musl}
root=$(cd "$(dirname "$0")/.." && pwd)
tdir=${CARGO_TARGET_DIR:-$root/target}
stage="$root/test/.stage"
rm -rf "$stage"
mkdir -p "$stage"

# The test binary's path is in cargo's JSON messages: the artifact of the
# nextnfs_server lib target built as a test.
exe=$(cargo test --locked --no-run --release --target "$target" -p nextnfs-server \
        --manifest-path "$root/Cargo.toml" --message-format=json \
    | grep '"name":"nextnfs_server"' | grep '"kind":\["lib"\]' | grep '"test":true' \
    | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -n 1)
if [ -z "$exe" ] || [ ! -x "$exe" ]; then
    echo "test/build.sh: no nextnfs-server test binary found" >&2
    exit 1
fi
cp "$exe" "$stage/nextnfs-server-tests"

rustc --edition 2021 -O --target "$target" -C target-feature=+crt-static \
    -o "$stage/test" "$root/test/runner/main.rs"

for f in "$stage/test" "$stage/nextnfs-server-tests"; do
    if file "$f" 2>/dev/null | grep -q 'dynamically linked'; then
        echo "test/build.sh: $f is not static" >&2
        exit 1
    fi
done
echo "staged: $(ls "$stage" | tr '\n' ' ')"
