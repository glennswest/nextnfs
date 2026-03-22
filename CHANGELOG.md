# Changelog

## [Unreleased]

### 2026-03-21
- **feat:** Initial project structure — NFSv4.0 server extracted from StormFS bold-nfs
- **feat:** nextnfs-proto crate (XDR codec, NFS4/RPC protocol types)
- **feat:** nextnfs-server crate with real filesystem metadata (stat-based attrs)
- **feat:** Inode-based persistent file handles (dev:ino packed into NfsFh4)
- **feat:** TCP socket tuning (4MB buffers, TCP_NODELAY, keepalive)
- **feat:** Channel depth increased from 16 to 256 for throughput
- **feat:** Proper EOF detection in READ operations
- **feat:** CLI binary with clap (--export, --listen, --config)
- **feat:** TOML config file support (server.listen, export.path, export.read_only)
- **feat:** Symlink and hardlink support enabled
- **feat:** NFSv4 byte-range locking (LOCK, LOCKT, LOCKU, RELEASE_LOCKOWNER)
- **feat:** LINK operation for hard links
- **feat:** Lock conflict detection with proper range overlap and read/write semantics
- **feat:** Multi-arch Containerfile (x86_64-musl + aarch64-musl scratch container)
- **feat:** Makefile with build-x86, build-arm64, container, push targets
- **feat:** Build script (build.sh) for podman container builds
- **perf:** Write cache rewritten — direct filesystem writes instead of in-memory buffer
- **perf:** COMMIT now calls fsync() for real durability guarantees
- **perf:** READ uses actual file size from seek(End) instead of cached attr
- **perf:** READ buffer allocation capped to remaining file bytes
- **feat:** Switch to stormdbase container image (logging, SSH, restart on failure, web dashboard)
- **feat:** stormd.toml process supervisor config with TCP liveness probe on port 2049
- **feat:** Separate Containerfiles for x86_64 and aarch64 (pre-built binary, no in-container Rust)
- **feat:** Container pushed to registry.gt.lo:5000/nextnfs:0.1.0
