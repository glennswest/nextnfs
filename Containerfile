# nextnfs — stormdbase container (pre-built static binary)
#
# Build x86:  make build-x86 && podman build --format docker --tls-verify=false -f Containerfile.x86_64 -t nextnfs .
# Build arm:  make build-arm64 && podman build --format docker --tls-verify=false -f Containerfile -t nextnfs .
# Run:        podman run -d -v /export:/export:z -p 2049:2049 -p 9080:9080 -p 2222:22 nextnfs
#
# This is the aarch64 (ARM64) variant for MikroTik Rose.

FROM registry.gt.lo:5000/stormdbase:latest

COPY target/aarch64-unknown-linux-musl/release/nextnfs /usr/bin/nextnfs
COPY nextnfs.example.toml /etc/nextnfs/nextnfs.toml
COPY stormd.toml /etc/stormd/config.toml

EXPOSE 9080 2049 22

ENTRYPOINT ["/stormd"]
