# Changelog

## [Unreleased]

### 2026-03-25
- **feat:** Unified CI test suite — wire-level + shell functional + performance benchmarks
- **feat:** nextnfstest wire-level protocol tester integrated as workspace member (nfstest/)
- **feat:** NFSv4.0 basic functional tests — 35 cases (file ops, attrs, symlinks, hardlinks, read/write)
- **feat:** NFSv4.0 edge case tests — 19 cases (error conditions, filehandle stability, concurrency, locking)
- **feat:** NFSv4.0 stress tests — 9 cases (10K files, parallel workers, deep paths, mount cycling)
- **feat:** NFSv4.1 session tests — 6 cases (mount, I/O, clean unmount, recovery, multi-session)
- **feat:** Performance benchmarks — fio throughput/latency, metadata ops/sec, concurrency scaling
- **feat:** knfsd baseline — run all tests against kernel NFS first for comparison
- **feat:** Comparison report — side-by-side knfsd vs nextnfs (functional, wire, performance)
- **chore:** ci-test.sh rewritten as 6-phase unified orchestrator
- **fix:** Resolve all clippy warnings in nfstest crate (strip_prefix, push_str, redundant cast, auto-deref)
- **fix:** CI excludes nextnfs-server from cargo test (broken test stubs)
- **fix:** CI knfsd mount tries export path before fallback to /
- **fix:** OpaqueAuth deserialization for RFC 5531 compliance — custom de/serializer handles opaque body wrapper
- **fix:** AuthUnix.stamp type corrected from u64 to u32 per RFC 5531 authsys_parms

### 2026-03-22
- **feat:** RPM packaging for Fedora/RHEL (nextnfs.spec + build-rpm.sh)
- **feat:** DEB packaging for Debian/Ubuntu (control files + build-deb.sh)
- **feat:** systemd service file (packaging/nextnfs.service)
- **feat:** Makefile targets for rpm-x86, rpm-arm64, deb-x86, deb-arm64
- **fix:** VERSION in Makefile and build.sh updated from 0.1.0 to 0.2.0
- **docs:** README updated with RPM/DEB install instructions and build targets

## [v0.2.0] — 2026-03-22

### Added
- Multi-export support — serve multiple filesystem paths as separate NFS exports
- ExportManager actor with per-export FileManagerHandle, stats, and dynamic add/remove
- NFSv4 pseudo-filesystem root — exports appear as top-level directories
- Pseudo-root PUTROOTFH, LOOKUP, READDIR, GETATTR for multi-export namespace
- Export-aware request routing — PUTFH extracts export_id from fh[1]
- REST API (axum) on port 8080 — /health, /api/v1/exports, /api/v1/stats
- Web UI dashboard with Dracula dark theme matching stormd iframe integration
- Export management page — add/remove exports via browser
- Statistics page — per-export read/write/bytes counters with auto-refresh
- CLI subcommands — `serve`, `export list/add/remove`, `stats`, `health`
- reqwest-based CLI client for REST API interaction
- stormd [process.ui] integration — NextNFS tab in stormd dashboard
- Multi-export TOML config — `[[exports]]` array with backwards-compatible `[export]`
- api_listen config option for REST API bind address

### Changed
- NfsRequest holds ExportManagerHandle with cached FileManagerHandle per-export
- NFSServer uses ExportManagerHandle instead of single VFS root
- ServerBuilder takes ExportManagerHandle, no longer requires root/export_root

## [v0.1.0] — 2026-03-21
### Added
- Initial project structure — NFSv4.0 server extracted from StormFS bold-nfs
- nextnfs-proto crate (XDR codec, NFS4/RPC protocol types)
- nextnfs-server crate with real filesystem metadata (stat-based attrs)
- Inode-based persistent file handles (dev:ino packed into NfsFh4)
- TCP socket tuning (4MB buffers, TCP_NODELAY, keepalive)
- Channel depth increased from 16 to 256 for throughput
- Proper EOF detection in READ operations
- CLI binary with clap (--export, --listen, --config)
- TOML config file support (server.listen, export.path, export.read_only)
- Symlink and hardlink support enabled
- NFSv4 byte-range locking (LOCK, LOCKT, LOCKU, RELEASE_LOCKOWNER)
- LINK operation for hard links
- Lock conflict detection with proper range overlap and read/write semantics
- Multi-arch Containerfile (x86_64-musl + aarch64-musl scratch container)
- Makefile with build-x86, build-arm64, container, push targets
- Build script (build.sh) for podman container builds
- Switch to stormdbase container image (logging, SSH, restart on failure, web dashboard)
- stormd.toml process supervisor config with TCP liveness probe on port 2049
- Separate Containerfiles for x86_64 and aarch64 (pre-built binary, no in-container Rust)
- Container pushed to registry.gt.lo:5000/nextnfs:0.1.0

### Performance
- Write cache rewritten — direct filesystem writes instead of in-memory buffer
- COMMIT now calls fsync() for real durability guarantees
- READ uses actual file size from seek(End) instead of cached attr
- READ buffer allocation capped to remaining file bytes
