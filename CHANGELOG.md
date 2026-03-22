# Changelog

## [Unreleased]

### 2026-03-22
- **feat:** Multi-export support — serve multiple filesystem paths as separate NFS exports
- **feat:** ExportManager actor with per-export FileManagerHandle, stats, and dynamic add/remove
- **feat:** NFSv4 pseudo-filesystem root — exports appear as top-level directories
- **feat:** Pseudo-root PUTROOTFH, LOOKUP, READDIR, GETATTR for multi-export namespace
- **feat:** Export-aware request routing — PUTFH extracts export_id from fh[1]
- **feat:** REST API (axum) on port 8080 — /health, /api/v1/exports, /api/v1/stats
- **feat:** Web UI dashboard with Dracula dark theme matching stormd iframe integration
- **feat:** Export management page — add/remove exports via browser
- **feat:** Statistics page — per-export read/write/bytes counters with auto-refresh
- **feat:** CLI subcommands — `serve`, `export list/add/remove`, `stats`, `health`
- **feat:** reqwest-based CLI client for REST API interaction
- **feat:** stormd [process.ui] integration — NextNFS tab in stormd dashboard
- **feat:** Multi-export TOML config — `[[exports]]` array with backwards-compatible `[export]`
- **feat:** api_listen config option for REST API bind address
- **refactor:** NfsRequest holds ExportManagerHandle with cached FileManagerHandle per-export
- **refactor:** NFSServer uses ExportManagerHandle instead of single VFS root
- **refactor:** ServerBuilder takes ExportManagerHandle, no longer requires root/export_root

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
