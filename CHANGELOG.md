# Changelog

## [Unreleased]

### Added
- Unit tests for GETATTR (3 tests: no filehandle, root type, multiple attrs)
- Unit tests for CREATE (4 tests: no filehandle, empty name, directory, unsupported type)
- Unit tests for LOOKUP (3 tests: no filehandle, nonexistent, after create)
- Unit tests for READDIR (3 tests: no filehandle, empty root, with entries)
- Unit tests for REMOVE (3 tests: no filehandle, nonexistent, directory)
- Unit tests for RENAME (3 tests: no saved fh, nonexistent source, directory rename)
- Unit tests for READ (2 tests: no filehandle, directory read fails)
- Unit tests for WRITE (1 test: no filehandle)

### Fixed
- Clippy collapsible_match warnings in SETCLIENTID_CONFIRM and RENEW tests
- Clippy bool_assert_comparison warning in nfstest XDR tests

## [v0.3.0] — 2026-03-25

### Added
- Unified CI test suite — wire-level + shell functional + performance benchmarks (ci-test.sh)
- nextnfstest wire-level protocol tester integrated as workspace member (nfstest/)
- NFSv4.0 basic functional tests — 35 cases (file ops, attrs, symlinks, hardlinks, read/write)
- NFSv4.0 edge case tests — 19 cases (error conditions, filehandle stability, concurrency, locking)
- NFSv4.0 stress tests — 9 cases (10K files, parallel workers, deep paths, mount cycling)
- NFSv4.1 session tests — 6 cases (mount, I/O, clean unmount, recovery, multi-session)
- Performance benchmarks — fio throughput/latency, metadata ops/sec, concurrency scaling
- knfsd baseline comparison — run all tests against kernel NFS for side-by-side report
- NFSv4.1 session operation types (proto)
- NFSv4.2 operation types (proto)
- RPM packaging for Fedora/RHEL (nextnfs.spec + build-rpm.sh)
- DEB packaging for Debian/Ubuntu (control files + build-deb.sh)
- systemd service file (packaging/nextnfs.service)
- test_utils module — in-memory VFS test harness for nextnfs-server unit tests
- SETCLIENTID, SETCLIENTID_CONFIRM, and RENEW integration tests restored

### Fixed
- Fattr4 attribute deserialization — implement all 12 common attribute types (was todo!() panics); fix XDR offset arithmetic (was using loop index instead of byte widths)
- RPC dispatch returns ProcUnavail/GarbageArgs per RFC 5531 instead of panicking on invalid procedure/message
- REMOVE operation returned Nfs4errStale on success instead of Nfs4Ok; error path was todo!()
- Filemanager actor channel sends no longer panic if actor dies — return NfsStat4::Nfs4errServerfault
- Parent filehandle lookup in create_file/remove_file no longer panics on missing parent
- SETATTR size truncation no longer panics on I/O errors
- OpaqueAuth deserialization for RFC 5531 compliance — custom de/serializer handles opaque body wrapper
- AuthUnix.stamp type corrected from u64 to u32 per RFC 5531 authsys_parms
- Manual RPC header parsing for wire compatibility with real NFS clients
- All clippy warnings resolved across workspace

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
