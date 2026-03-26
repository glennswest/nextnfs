# Changelog

## [Unreleased]

## [v0.8.0] — 2026-03-25

### Added
- Functional workflow tests (16 tests: write→read roundtrip, write→overwrite→read, open→write→close lifecycle, create→lookup→getattr chain, nested dir readdir, create→remove→lookup, rename verify, multi-file readdir, lock→unlock→relock, partial read, setattr→getattr, compound CREATE→GETFH, compound CREATE→LOOKUP→GETATTR, compound SAVEFH→RENAME, open→read existing, create/remove/readdir)
- Directory removal verification test (CREATE→LOOKUP→REMOVE→LOOKUP fails)
- Proto codec edge-case tests (+13: decode empty/incomplete/oversized frames, EOF handling, encode reply, multi-fragment reassembly, from_bytes invalid/truncated/null-proc/unsupported-auth)
- Compound workflow tests (+6: create→write→read→close, lock→write→unlock, savefh→lookup→restorefh, readdir cookie continuation, open→write→overwrite→read, getattr-after-write size verification)
- Operation error-branch tests: op_link (+2: root source rejected, MemoryFS hard_link), op_open (+1: unsupported claim type), op_lookup (+2: subdirectory lookup, miss unsets filehandle)
- Total workspace tests: 340 (52 proto + 282 server + 6 nfstest)

### Fixed
- FileManager RemoveFile handler was calling `read_dir()` (listing) instead of `remove_dir()` for directories — VFS directory was never actually deleted
- ClientManager actor death now returns Nfs4errServerfault instead of panicking (upsert_client, confirm_client, renew_leases)
- Clock backward panics in request.rs, filehandle.rs, FileManager::new(), op_pseudo — use unwrap_or_default()
- READDIR cookieverf conversion panic on malformed verifier — use unwrap_or fallback, truncate oversized verifiers
- READDIR eof calculation removed unnecessary clone().unwrap()
- REMOVE path join panic on invalid target — now returns Nfs4errInval
- All dangerous unwrap() calls in production server code eliminated

## [v0.7.0] — 2026-03-25

### Added
- FileManager direct tests (28 tests: new/defaults, root_fh, create_file, filehandle lookups, lockingstate_id, attr_supported_attrs, real_path, lock/unlock/test_lock/release, touch/update/attach_locks, filehandle_attrs, volatile handle, drop_cache)
- WriteCache tests (4 tests: valid/invalid path, dirty flag, VFS fallback)
- NFSv4.1 session tests (10 new: client_id incrementing, session unique IDs, get/destroy nonexistent, channel/slot defaults, SessionManager::default, slot count, preserve others)
- NFSv4.2 enum tests (4 tests: op values, data_content, equality, clone)
- NfsOpResponse tests (2 tests: ok/error construction)
- Expanded all thin-coverage ops: op_locku (+1), op_close (+1), op_commit (+2), op_putfh (+1), op_openconfirm (+1), op_release_lockowner (+1), op_getattr (+3), op_setattr (+1), op_open (+3), op_create (+1), op_rename (+2), op_remove (+1), op_readdir (+2)
- Every .rs file in the server crate now has test coverage
- Total workspace tests: 299 (39 proto + 254 server + 6 nfstest), 0 clippy warnings

## [v0.6.0] — 2026-03-25

### Added
- Filehandle & RealMeta tests (23 tests: new/new_real for all 7 file types, time attrs, space_used, nlink, pseudo_root, attr_size, current_time, RealMeta from_path, initial state)
- ClientManager actor tests (11 tests: renew_leases valid/stale, set_current_fh, multiple clients unique ids, confirm wrong principal, get_client not found/unconfirmed, handle upsert+confirm/renew/set_fh)
- Proto type serialization tests (16 tests: Stateid4, Fsid4, Nfstime4, NfsFtype4, NfsStat4, NfsLockType4, StableHow4, Createtype4, NfsClientId4, ClientAddr4, LockOwner4, access flags)
- RPC proto tests (8 tests: AuthUnix defaults/roundtrip, RpcReplyMsg serialization, XID encoding, AuthStat default, CallBody fields)
- Expanded op_access (4 tests), op_read (4 tests), op_write (3 tests), op_lock (2 tests), op_lockt (1 test)
- Total workspace tests: 230 (39 proto + 185 server + 6 nfstest), 0 clippy warnings

## [v0.5.0] — 2026-03-25

### Added
- Proto XDR roundtrip tests (17 tests: bitmap encoding, attr value roundtrips, FattrRaw parsing, NfsStat4 serialization, OpaqueAuth roundtrips)
- Compound dispatch tests (11 tests: NULL, PUTROOTFH+GETATTR, error short-circuiting, minor version mismatch, SAVEFH/RESTOREFH, GETFH, create+readdir lifecycle, unsupported ops, empty args)
- FileManager actor tests (10 tests: root fh, nonexistent path/id, attrs, stable id, create/remove/touch lifecycle)
- Lock conflict detection unit tests (14 tests: write-vs-write, read-vs-read, read-vs-write, same-owner, non-overlapping, adjacent, zero-length-to-EOF, WritewLt, lock/unlock/test_lock/release via actor)
- ExportManager tests (11 tests: empty list, add/remove, duplicate, nonexistent path, get by id/name, sequential IDs, initial stats)
- RPC dispatch tests (5 tests: NULL, COMPOUND, ProcUnavail, GarbageArgs, XID preservation)
- NfsRequest edge case tests (13 tests: initial state, save/restore, unset fh, client addr, export id, pseudo root, bad fh id, boot/request time, close, set_filehandle_with_export)
- Total workspace tests: 160 (17 proto + 137 server + 6 nfstest)

## [v0.4.0] — 2026-03-25

### Added
- Unit tests for CLOSE (2 tests: no filehandle, successful close with stateid)
- Unit tests for SETATTR (2 tests: no filehandle, empty attributes)
- Unit tests for COMMIT (2 tests: no filehandle, verifier generation)
- Unit tests for OPEN (4 tests: no filehandle, empty filename, create file, read nonexistent)
- Unit tests for OPEN_CONFIRM (2 tests: no filehandle, no locks)
- Unit tests for READLINK (1 test: returns NOTSUPP)
- Unit tests for LINK (2 tests: no saved fh, no current fh)
- Unit tests for LOCK (2 tests: no filehandle, lock on root directory)
- Unit tests for LOCKT (2 tests: no filehandle, no conflict)
- Unit tests for LOCKU (1 test: nonexistent lock)
- Unit tests for RELEASE_LOCKOWNER (1 test: no locks)
- Unit tests for pseudo-fs (6 tests: fh structure, is_pseudo_root, export_id, stamp, getattr type/multiple)
- Unit tests for PUTFH (2 tests: pseudo root, invalid handle)
- All 18 op_*.rs files now have test modules — 100% operation coverage
- Make Commit4args and OpenConfirm4args fields public for testability

### Fixed
- Getattr4resok serializer panics on None obj_attributes — now uses if-let pattern
- Attrlist4<FileAttr> deserializer panics on malformed input — now propagates errors

## [v0.3.1] — 2026-03-25

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
- CLOSE panics if no current filehandle — now returns Nfs4errNofilehandle
- OPEN_CONFIRM panics if no filehandle or empty locks — now returns proper NFS errors
- OPEN read path panics if no filehandle — now returns Nfs4errNofilehandle
- OPEN write path panics on invalid path join — now returns Nfs4errInval
- OPEN is_dir() panic on VFS error — now uses unwrap_or(false)
- COMMIT panics on write cache actor failure — now returns Nfs4errServerfault
- WRITE panics on write cache, append, write, and flush failures — now returns proper NFS errors
- READDIR panics on read_dir() VFS failure — now returns Nfs4errIo
- CREATE is_file() panic on VFS error — now uses unwrap_or(false)
- CREATE panics on invalid path join — now returns Nfs4errInval
- RENAME panics on invalid destination path join — now returns Nfs4errInval
- FileManager actor: path join and exists() panics on invalid VFS operations
- FileManager: cache lookup uses if-let instead of contains_key+unwrap
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
