# Changelog

## [v0.13.7] — 2026-04-06

### Fixed
- O_APPEND used for sparse writes — the O_APPEND condition (`offset >= file_len`) incorrectly applied O_APPEND mode for writes past EOF (e.g., `dd seek=256`), which ignores the requested offset and always writes at the actual end. Sparse files were truncated to 4096 bytes instead of creating holes. Changed to `offset == file_len` so O_APPEND is only used for writes exactly at the current EOF; writes past EOF use pwrite to preserve the requested offset
- Concurrent appends lose data — two writers appending to the same file could both read the same file length from GETATTR, then both use pwrite at the same offset, causing one write to overwrite the other. Now detects writes at exact EOF and uses O_APPEND mode so the kernel atomically positions each write at the true end of file
- Silly-rename CLOSE deletes file too early — when a file is removed while open (server-side silly-rename), CLOSE immediately deleted the renamed file and evicted it from fhdb. The kernel client may still need to READ from the file via an open fd after sending CLOSE. Deferred deletion is now handled by a periodic sweep timer (every 15s) instead of CLOSE
- GETATTR returns stale mtime/size — `filehandle_attrs()` in the handle returned cached metadata from the Filehandle struct instead of reading current values from the real filesystem. After a WRITE, `stat` would show the old mtime. Now refreshes from disk via `RealMeta::from_path()` before returning attributes
- XDR UTF-8 string serialization — `serde-xdr` v0.6 only allows ASCII in XDR strings, but NFS4 defines `utf8string` as `typedef opaque utf8string<>` (RFC 7531). Non-ASCII filenames like `filé_ñame_日本語` caused READDIR serialization failure, no response sent, kernel timeout → EIO. Added `utf8_opaque` serde module that serializes via `serialize_bytes()` (identical wire format, no ASCII restriction). Applied to all 22 String fields in proto
- RENAME path update eviction — `handle_rename_path()` used `get_filehandle_by_path()` which calls `path_exists()` on the old path. Since the rename already completed, the old path no longer exists, causing the fhdb entry to be evicted instead of updated. Client-side silly-rename (RENAME + READ) then got NFS4ERR_STALE. Fixed to use `fhdb.get_by_path()` directly
- REMOVE silently discards filesystem errors — `remove_dir()` and `remove_file()` results were ignored, fhdb entries cleaned up even on failure. `rmdir` on non-empty directories returned Ok. Now returns NFS4ERR_NOTEMPTY for non-empty dirs, NFS4ERR_IO for other failures, and preserves fhdb on failure
- READDIR cookie index skewed by hidden entries — `.nfs4attrs` directory skip used `enumerate()` index for cookies, causing off-by-one on paginated READDIR. Verifier mismatch led to NFS4ERR_NOTSAME (kernel EIO). Now uses separate entry counter
- READDIR EOF flag miscalculated — `entry.cookie + added_entries >= fnames.len()` was off by the cookie base offset (3), causing premature eof=true on partial results and infinite retry loops on directories with certain entry counts. Now correctly compares last returned cookie against last possible cookie
- `get_filehandle_by_path()` returned stale entries without existence validation — unlike `get_filehandle_by_id()` which checks `path_exists()`, path-based lookups blindly returned cached entries. Added existence check and eviction of stale entries, fixing massive stale file handle errors at scale
- Silly-rename path validation — `get_filehandle_by_id()` evicted fhdb entries for silly-renamed files because `path_exists()` failed on the renamed VfsPath. Now skips validation for entries in `pending_deletes`, allowing READ via open fd after REMOVE
- Silly-rename support for delete-while-open — REMOVE on a file with active open locks now renames to `.nfs.<inode>` instead of deleting. CLOSE completes the deferred deletion when last lock is released
- READDIR now hides `.nfs.*` silly-rename files from directory listings
- Test helpers: `run_test()` now handles return code 77 as SKIP instead of FAIL
- access-denied tests restore file permissions before SKIP (prevents mode 000 files from affecting later tests)
- flock shared test: use `flock -c` file-path form instead of fd 200 redirection (avoids fd invalidation on NFS)

### Added
- `utf8_opaque` and `utf8_opaque_vec` serde modules for NFS4 utf8string fields
- 4 new proto tests: non-ASCII roundtrip for Entry4, Readlink4res, Compound4res, and ASCII compatibility
- Periodic pending_deletes sweep timer (15s) in FileManager actor for deferred silly-rename cleanup

## [v0.13.6] — 2026-04-06

### Fixed
- Stale PUTFH cache causes RENAME across directories to target wrong path — inode reuse after file deletion caused per-connection cache to return filehandles with outdated paths. Added path existence validation in cache lookup (#44)
- Symlink test uses absolute client-side paths as link targets — changed to relative paths which are correct for NFS (server doesn't have client mount paths)

### Added
- XDR roundtrip tests for Createtype4::Nf4lnk and CREATE symlink compound
- Pre-dispatch debug logging in compound loop for operation-level tracing

## [v0.13.5] — 2026-04-06

### Fixed
- Grace period blocks all mutating ops on fresh start — when state recovery file is stale/missing (no clients to reclaim), the server still entered a 90-second grace period, causing EIO for all CREATE/WRITE/REMOVE/RENAME operations. Now skips grace when there's nothing to recover (#43)
- Grace period denial audit logging — rejected operations during grace were invisible in logs (compound returned early). Now logs the denied operation name and status
- Unused import warnings in op_destroy_session.rs and op_allocate.rs test modules
- ci-deploy-test.sh grep patterns for PASS/FAIL counting (same fix as nextnfs-run-tests)

## [v0.13.4] — 2026-04-06

### Fixed
- OPEN_CONFIRM fails with NFS4ERR_BAD_STATEID due to stale filehandle cache — PUTFH returned cached filehandle without locks after stat-after-CLOSE or inode reuse. OPEN now caches filehandle with locks so OPEN_CONFIRM in the next compound gets fresh data (#42)
- Test runner result parsing — nextnfs-run-tests grep patterns didn't match test output format (PASS/FAIL not OK:/FAIL:) and `grep -c || echo 0` produced "0\n0" causing arithmetic error

## [v0.13.3] — 2026-04-05

### Fixed
- Stale filehandle from inode reuse — `get_filehandle()` now verifies path consistency when an inode-based ID matches an existing fhdb entry, evicting stale entries from deleted files whose inodes were reused (#37, #38)
- CLOSE didn't refresh fhdb after write cache flush — GETATTR after CLOSE returned stale file size/mtime because the fhdb entry wasn't updated after commit (#40)
- Symlink operations hang — VfsPath::exists() follows symlinks via stat(), so symlinks with client-side targets appeared non-existent on the server. Added `path_exists()` helper using lstat() fallback (#41)
- SETATTR chown fails with NFSv4 idmapping strings — `set_attr()` only accepted numeric uid/gid strings, but Linux NFS clients may send "user@domain" format. Added `resolve_nfs4_uid()`/`resolve_nfs4_gid()` with NSS lookup via getpwnam_r/getgrnam_r (#39)

## [v0.13.2] — 2026-04-05

### Fixed
- OPEN for reading never created lock state — `open_for_reading()` returned hardcoded zero stateid without registering in lockdb, causing OPEN_CONFIRM to fail with NFS4ERR_BAD_STATEID on every read-after-write (#33)
- Cascading EIO failures on read operations (#34, #35) resolved by #33 fix

## [v0.13.1] — 2026-04-05

### Fixed
- CLOSE stateid leak — op_close now releases open stateid from lockdb, preventing resource exhaustion after ~2000 files (#30)
- REMOVE lock cleanup — RemoveFile handler cleans up all lockdb entries, write cache, and delegations for the removed filehandle (#28)
- Concurrent write corruption — write cache and FileSync4 writes use pwrite (write_all_at) for atomic positional I/O instead of seek+write (#29)

### Added
- Special characters and dot file tests — CREATE/LOOKUP/READDIR/OPEN tests for .hidden, spaces, dashes, underscores, dots (#27)
- close_file() method on FileManagerHandle for stateid cleanup
- 13 new unit tests (536 total)

## [v0.13.0] — 2026-04-05

### Added
- ci-build-test.sh — standalone combined build+test CI script for mkube runners

### Fixed
- CI scripts (server-runner.sh, test-runner.sh) auto-detect /build source directory for mkube runners

## [v0.12.1] — 2026-04-03

### Fixed
- COMMIT synchronization — commit() now awaits fsync completion via oneshot before returning NFS4OK (#26)
- CLOSE flushes write cache — dirty data flushed (fsync) before dropping filehandle on close (#26)
- RENAME path invalidation — filehandle database updated with new path/VfsPath after rename (#22)
- LINK real filesystem paths — hard_link() uses export_root to construct real paths instead of VFS strings (#24)
- SYMLINK real filesystem paths — symlink() uses export_root to construct real link path (#23)
- READLINK real filesystem paths — read_link() uses export_root to resolve symlink target (#23)
- SETATTR owner/group/mode/time — chown(uid), chown(gid), chmod(mode), utimensat(mtime) via libc syscalls on real paths (#25)
- ACCESS permission checking — POSIX mode-based permission check against caller uid/gid instead of echoing back client bits; root gets all access (#31)

### Changed
- FileManagerHandle stores export_root and exposes real_path() helper for op_* code

### 2026-04-03
- **feat:** Hardened systemd service — NoNewPrivileges, ProtectSystem=strict, PrivateTmp, PrivateDevices, MemoryDenyWriteExecute, ProtectKernelTunables/Modules/ControlGroups, ReadWritePaths for /export and /var/lib/nextnfs
- **feat:** RPM spec update — /var/lib/nextnfs state directory, /export directory, v4.0/v4.1/v4.2 description, changelog
- **feat:** ci-rpm.sh — automated RPM builder for mkube Fedora runners, musl-gcc CC workaround, end-to-end build + package
- **feat:** ci-deploy-servers.sh — build + deploy to server2/server3 via SSH with firewalld integration
- **feat:** Example config — state_dir, TLS, QoS, access control examples
- **ops:** Deployed v0.12.0 to server2.g10.lo (Fedora 43) and server3.g10.lo (Fedora 43), systemd service running, health checks passing

## [v0.12.0] — 2026-04-02

### Added
- **feat:** Missing GETATTR attributes — FilesAvail/FilesFree/FilesTotal (inode counts for `df -i`), TimeDelta (1ns server time granularity), TimeCreate (birth time), MountedOnFileid, CaseInsensitive/CasePreserving; XDR serialize/deserialize, 6 new tests
- **feat:** Per-export access controls — IP ACL with CIDR subnet matching (IPv4/IPv6), SquashMode (none/root_squash/all_squash) with configurable anon_uid/anon_gid, enforcement in PUTFH/PUTROOTFH (NFS4ERR_ACCESS), Owner/OwnerGroup squash in GETATTR, TOML config support, 15 new tests
- **feat:** CLAIM_PREVIOUS grace period state reclaim — 90s grace period on startup (skipped if state recovery succeeds), NFS4ERR_GRACE for mutating ops during grace, OPEN CLAIM_PREVIOUS reclaims open state on existing files, CreateOpenState actor message for lock-free reclaim, 2 new tests
- **feat:** NFSv4 ACLs — synthesize POSIX mode-based ACLs (owner/group/everyone ALLOW ACEs), ACL XDR serialize/deserialize, ACE type/flag/mask constants, GETATTR Acl support on real files and pseudo-root, mode_to_acl() conversion, 6 new tests
- **feat:** FsLocations attribute — GETATTR returns fs_locations4 (RFC 7530 §7.7), XDR serialize/deserialize for pathname4 arrays, local export returns fs_root=["/"] with empty locations, supported on real files and pseudo-root, 3 new tests
- **feat:** Named attributes (OPENATTR) — opens per-file named attribute directory via `.nfs4attrs/<fileid>/`, createdir flag creates on demand, hidden from READDIR, NamedAttr GETATTR now reports true, 6 new tests
- **feat:** File delegations — OPEN grants read delegations (OPEN_DELEGATE_READ) with stateid tracking, DELEGRETURN returns delegations, DELEGPURGE purges reclaim state, delegation conflict detection per-file, 4 new tests
- **feat:** RPC-over-TLS transport encryption (RFC 9289) — tokio-rustls TLS acceptor wrapping TCP connections, PEM cert/key loading, ServerBuilder `.tls()` method, TOML config `tls_cert`/`tls_key` fields, ConnectionContext struct for clean parameter passing, generic `handle_connection<T>` over any AsyncRead+AsyncWrite transport, 3 new tests
- **feat:** NFSv4.2 server-side operations (RFC 7862) — COPY (op 60) server-side file copy with saved/current filehandle source/destination, partial offset/count support, chunked 256KB I/O; SEEK (op 69) data/hole boundary detection, contiguous data model for VFS; ALLOCATE (op 59) space preallocation with zero-fill extension; all operations enforce quota via QuotaManager; 10 new tests
- **feat:** NFSv4.1 session support (RFC 5661) — EXCHANGE_ID (client-server identity negotiation), CREATE_SESSION (session establishment with slot table), SEQUENCE (per-compound slot/sequence validation), DESTROY_SESSION, DESTROY_CLIENTID, RECLAIM_COMPLETE, BIND_CONN_TO_SESSION, FREE_STATEID, TEST_STATEID; minor_version=1 accepted in COMPOUND; SessionManager with Arc<RwLock<HashMap>> for thread-safe session state; 15 new tests
- **feat:** Session trunking (RFC 5661 §2.10.5) — BIND_CONN_TO_SESSION tracks bound connections per session via HashSet, connection_count() query, idempotent re-binding, session destruction cleans up bindings, 4 new trunking tests
- **feat:** pNFS layout operations (RFC 5661 §12) — LAYOUTGET returns NFS4ERR_LAYOUTUNAVAILABLE (single-server, no parallel data paths), LAYOUTCOMMIT/LAYOUTRETURN return NFS4ERR_NOMATCHING_LAYOUT, GETDEVICEINFO/GETDEVICELIST return NFS4ERR_NOTSUPP; LayoutType4, LayoutIomode4, LayoutReturnType4, DeviceId4 protocol types; pNFS error codes (BADIOMODE, BADLAYOUT, LAYOUTTRYLATER, LAYOUTUNAVAILABLE, NOMATCHINGLAYOUT, RECALLCONFLICT, UNKNOWNLAYOUTTYPE); fixed NfsStat4 error code numbers (SeqMisordered=10063, ConnNotBoundToSession=10055, SeqFalseRetry=10076); 6 new tests
- **feat:** RPCSEC_GSS / Kerberos framework (RFC 2203) — RPCSEC_GSS auth flavor (6) parsing in RPC layer, RpcSecGssCred credential structure (gss_proc/seq_num/service/handle), OpaqueAuth::AuthGss variant with XDR serialize/deserialize roundtrip; SECINFO_NO_NAME operation (op 52, RFC 5661 §18.45) for pseudo-root security negotiation; SECINFO and SECINFO_NO_NAME advertise krb5/krb5i/krb5p flavors; NfsRequest auth credential accessors (auth_uid/auth_gid/is_gss_auth); auth credentials propagated from RPC call to compound handler; fixed NfsStat4 error codes (ConnNotBoundToSession=10055, SeqMisordered=10063, SeqFalseRetry=10076); 8 new tests
- **feat:** RPC-over-RDMA transport layer (RFC 8166/8267) — RPCRdma protocol framing (RDMA_MSG, RDMA_NOMSG, RDMA_MSGP, RDMA_DONE, RDMA_ERROR), RdmaHeader 16-byte wire format with XID/version/credits/proc, RdmaSegment memory region descriptors for zero-copy DMA, ReadChunk/WriteChunk for RDMA Read/Write verbs, RdmaConfig (device/port/GID/inline threshold/queue depths), RdmaError error type, TOML config fields (rdma_device, rdma_port), 12 new tests
- **chore:** 591 workspace tests (62 proto + 523 server + 6 nfstest), 0 clippy warnings

### 2026-03-25
- **feat:** OverlayFS VFS backend (overlay.rs) — merges writable upper with read-only lower layers, whiteout markers (OCI spec), copy-up on write, merged directory listings, 31 tests
- **feat:** Export manager overlay integration — AddOverlayExport message, PhysicalFS-backed OverlayFS creation, path validation, 9 new tests
- **feat:** dm-verity Merkle hash tree library (verity.rs) — SHA-256 Merkle tree builder, block verification, compact binary serialization, VFS directory tree scanning, LayerManifest with per-file content/metadata hashes, VerifiedBlockCache atomic bitset, 32 tests
- **feat:** VerifiedFS wrapper implementing vfs::FileSystem trait — verified lower layer usable as drop-in in OverlayFS, read-time integrity verification, 8 new tests
- **feat:** Per-export NFS quota support — QuotaConfig (hard/soft limits), QuotaManager with AtomicU64 byte tracking, GETATTR reports QuotaAvailHard/QuotaAvailSoft/QuotaUsed/SpaceAvail/SpaceFree/SpaceTotal, WRITE/CREATE/OPEN enforce NFS4ERR_DQUOT on hard limit exceeded, quota cached in NfsRequest via set_export()
- **chore:** 493 workspace tests (56 proto + 431 server + 6 nfstest), 0 clippy warnings

## [v0.11.0] — 2026-03-25

### Added
- Linux kernel NFS mount support — nextnfs can now be mounted by the Linux kernel NFS client (`mount -t nfs4`)
- RPC program/version validation — unknown programs (e.g. nfslocalio 400122) get PROG_UNAVAIL, wrong NFS version gets PROG_MISMATCH
- MismatchInfo constructor for RPC version negotiation responses
- GETATTR attributes: MAXREAD, MAXWRITE, MAXFILESIZE, MAXLINK, MAXNAME, HOMOGENEOUS, NOTRUNC, CANSETTIME, CHOWNRESTRICTED
- XDR padding roundtrip tests for Owner/OwnerGroup string serialization
- Tests for ProgUnavail (unknown program) and ProgMismatch (wrong version) RPC dispatch
- 407 workspace tests (56 proto + 345 server + 6 nfstest), 0 clippy warnings

### Fixed
- XDR padding for Owner/OwnerGroup strings in GETATTR serialization — missing 4-byte alignment corrupted all subsequent attributes in the opaque blob, causing kernel mount to reject responses with EIO
- `from_bytes()` no longer tries to parse COMPOUND args for non-NFS RPC programs, preventing false GarbageArgs errors on multiplexed connections
- nfslocalio (Linux 6.12+) mount hang — kernel sends RPC program 400122 on the NFS TCP connection; server now responds immediately with PROG_UNAVAIL instead of timing out

## [v0.10.1] — 2026-03-26

### Fixed
- multi_index_map `modify_by_*` panic on Linux — replaced all 3 usages (confirm_client, renew_leases, sweep_leases) with safe remove+insert pattern to avoid internal reindex panics
- Client manager actor resilience — added `catch_unwind` around message handling so a panic in one request doesn't kill the actor and cascade-fail all subsequent client operations
- SETCLIENTID error propagation — handler was swallowing the actual NFS error and always returning NFS4ERR_SERVERFAULT; now returns the correct error code from ClientManager
- All 14/14 NFSv4.0 wire tests now pass on Linux CI (previously 3 SETCLIENTID-related tests failed: W40-010, W40-011, W40-014)
- Added proto roundtrip tests for SETCLIENTID wire encoding compatibility
- 403 workspace tests (54 proto + 343 server + 6 nfstest), 0 clippy warnings

## [v0.10.0] — 2026-03-26

### Added
- SECINFO operation (RFC 7530 S16.31) — returns AUTH_SYS and AUTH_NONE security flavors for client security negotiation
- OPEN_DOWNGRADE operation (RFC 7530 S16.19) — reduces open share access/deny modes without closing the file
- Per-client audit logging — structured tracing with client IP, operation, status, export ID, and file path for every NFS operation
- Per-export I/O statistics — READ/WRITE operations now increment ExportStats counters (reads, writes, bytes_read, bytes_written, ops) visible via REST API `/api/v1/stats`
- Cached `Arc<ExportStats>` in NfsRequest for zero-cost counter updates (no actor messages)
- SeCinfo4 proto type extended with AuthNone and AuthSys variants for proper XDR encoding
- Proto OpenDowngrade4args, OpenDowngrade4resok, SecInfo4args fields now public
- Courteous server behavior — expired client leases enter courtesy state instead of immediate purge; background lease sweep every 30s marks expired→courtesy→purge with 2x lease window
- Per-export QoS rate limiting — token bucket algorithm (ops/sec and bytes/sec), configurable via TOML `max_ops_per_sec`/`max_bytes_per_sec` and REST API `GET/PUT /api/v1/qos/{name}`, returns NFS4ERR_DELAY when exceeded
- Near-zero grace period recovery — periodic client state snapshots to JSON (every 30s), atomic writes, restore on startup to skip grace period; configurable via TOML `server.state_dir`
- RestoreClients actor message for ClientManager — bulk client restoration from state snapshots
- 401 workspace tests (52 proto + 343 server + 6 nfstest), 0 clippy warnings

## [v0.9.0] — 2026-03-26

### Added
- VERIFY and NVERIFY operations (RFC 7530 S16.32, S16.15) for client cache validation
- READLINK operation implemented with std::fs::read_link() (was returning NOTSUPP)
- Industry benchmark suite (tests/nfs_bench_suite.sh): fio, IOzone, Dbench, Bonnie++, SPECstorage-style workloads (AI/Image, Software Build, Genomics)
- Data integrity test (tests/nfs_integrity.sh): Linux kernel source untar, SHA-256 all files, 10 parallel copies with full verification
- Ramdisk baseline benchmark for peak throughput reference
- CI test-runner integration with bench suite + integrity validation
- 371 workspace tests (52 proto + 313 server + 6 nfstest), 0 clippy warnings

### Fixed
- Proto Verify4args, Verify4res, Nverify4args, Nverify4res fields now public
- VERIFY/NVERIFY wired into compound dispatch (previously returned NOTSUPP)

## [v0.8.1] — 2026-03-25

### Added
- 47 new tests across proto codec, operations, and workflow lifecycles
- Proto codec edge-case tests (+13): decode/encode, multi-fragment reassembly, oversized frame rejection, EOF handling, from_bytes edge cases
- Compound workflow tests (+6): create→write→read→close, lock→write→unlock, savefh→lookup→restorefh, readdir cookie continuation, overwrite→read, getattr size verification
- Operation error-branch tests (+15): op_read (EOF, zero count), op_write (empty data, DataSync4, offset), op_lock (concurrent reads, LockOwner), op_locku (bad stateid), op_readdir (stale cookieverf), op_rename (no fh, cross-dir), op_renew (unknown client), op_set_clientid (different verifier), op_set_clientid_confirm (wrong verifier, zero clientid)
- LOOKUPP tests (+3): no filehandle, from root, from subdirectory
- Compound edge-case tests (+5): empty argarray, multiple PUTROOTFH, op_link (+2), op_open (+1), op_lookup (+2)
- Workflow lifecycle tests (+3): unstable write→commit, create→lookup→remove→verify, open→close→reopen

### Fixed
- Removed 11 unused import warnings in test modules
- Total workspace tests: 363 (52 proto + 305 server + 6 nfstest), 0 warnings, 0 clippy

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
