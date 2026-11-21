# CLAUDE.md — nextnfs

Standalone NFSv4 server over a real filesystem, in Rust. Cross-project rules
live in `../CLAUDE.md`; this file is the project context and work plan.

## Version

**0.18.0** (tag `v0.18.0`). Version locations — all must match:

- `Cargo.toml` → `[workspace.package] version` (every crate inherits it)
- `build.sh` → `VERSION=`
- `README.md` → "Current version" line and RPM/DEB file names

RPM specs and `packaging/deb/control` take the version from `Cargo.toml` at
package time (`packaging/build-*.sh`, `ci-rpm.sh`); do not hard-code it there.

## Layout

| Crate | Dir | What |
|---|---|---|
| `nextnfs-proto` | `proto/` | XDR codec, RPC + NFSv4 types |
| `nextnfs-krb5` | `krb5/` | pure-Rust Kerberos V5 acceptor: AES enctypes, MIT keytab, AP-REQ/AP-REP, RFC 4121 MIC/Wrap |
| `nextnfs-server` | `nfs/` | server library: `ExportManager`, `FileManager` actors, COMPOUND dispatch, locking, sessions, TLS, state recovery |
| `nextnfs` | `.` | binary: CLI (`src/main.rs`, `src/cli.rs`), TOML config (`src/config.rs`), REST API (`src/api.rs`), web UI (`src/web.rs`) |
| `nextnfstest` | `nfstest/` | wire-level NFS protocol test suite |
| `nextnfs-stress` | `stress/` | POSIX stress harness against a live mount |

Other: `tests/*.sh` (shell integration suites; `tests/krb5-interop.sh` + `tests/krb5-interop/` drive an unprivileged MIT KDC and libgssapi_krb5 against the server; shipped as the `nextnfs-tests`
RPM with `packaging/nextnfs-run-tests`), `packaging/` (RPM/DEB/systemd),
`Containerfile*` + `stormd.toml` (stormd container), `ci/` (pipeline scripts),
`doc/` (deploy guide), `enhancements/` (design proposals).

## How it ships

- **RPM / DEB** (`make rpm-x86|deb-x86`, `packaging/build-*.sh`): `/usr/bin/nextnfs`,
  `/usr/bin/nextnfs-stress`, `/etc/nextnfs/nextnfs.toml` (from
  `nextnfs.example.toml`), `nextnfs.service` running `serve --config /etc/nextnfs/nextnfs.toml`.
  RPM `%post` enables the service but does not start it; DEB `postinst`
  enables and starts it. Both ship `nextnfs-stress` and own `/export` and
  `/var/lib/nextnfs` (#96). `make build-*` builds and strips both binaries
  (needs `*-linux-musl-strip`, absent on dev — there, `cargo build` the two
  binaries and run `packaging/build-{rpm,deb}.sh` directly). `ci-rpm.sh` is
  the CI RPM path.
- **Container** (`build.sh`, `make container-*`): `stormdbase` image, stormd as
  PID 1, which runs `nextnfs serve --export /export` (CLI flags — the copied
  `/etc/nextnfs/nextnfs.toml` is **not** read in the container).
- **stormcos golden**: nextnfs is a registered stormcentral component
  (`service`, optional, started only by `nfsop` — one pod per NFSServer).
  stormd runs `nextnfs --config /etc/nextnfs/nextnfs.toml`, health
  `/health` on 8080, the golden's stormd on 8180 (the repo's `stormd.toml`
  9080 is the container image only). Registry entry: `stormcentral component
  edit nextnfs`, not a commit. After an issue's work is pushed and sc-build
  is green: `stormcentral component build nextnfs --url
  http://stormcentral.g8.lo`. First golden: golden-nextnfs-81fcd9ea25dc
  (0.13.9, a3d625d). There is no `test/` test container yet.

## Known gaps (tracked as issues)

None of the config keys is a stub: `rdma_device`/`rdma_port` were removed
(#117); NFS-over-RDMA is #110. See `gh issue list`.

Kerberos (#106): `nextnfs-krb5` (`krb5/`) is the acceptor;
`nfs/src/server/gss.rs` is RPCSEC_GSS (handled in `nfs/src/lib.rs`
before NFSService when `[server] krb5_keytab` is set); per-export `sec`
is enforced in the COMPOUND loop (WRONGSEC). GETATTR owners are still
numeric (#114).

Permissions (#91): `nfs/src/server/perm.rs` checks every op for the
squash-mapped caller in the COMPOUND dispatcher (after the read-only
check) and chowns new objects; `squash` defaults to `root_squash`.

## Work plan

- [x] 2026-09-27 — docs refresh from code (README config/API reference, deploy
      guide build commands, this file); issues filed for the gaps above.
- [x] 2026-09-27 — second pass: README `read_only` row corrected (#93),
      `doc/` deploy guide build step fixed (build.sh, not bare `podman build`)
      and mkube references marked retired, example config annotated.
- [x] 2026-09-27 — #93 `read_only` enforced in the COMPOUND dispatcher
      (NFS4ERR_ROFS via `NfsResOp4::OpError`; ACCESS drops write bits).
- [x] 2026-09-27 — third pass: packaging checked against the docs; README
      RPM-vs-DEB table, RPM `%description` / DEB control no longer claim
      v4.2/pNFS/RDMA/quota/OverlayFS; issues #95, #96 filed.
- [x] 2026-09-27 — #93 verified: `sc-build 'cargo test --workspace'` on
      38ebd4c green (544 tests); #93 and #94 closed. Use the `--workspace`
      form — plain `sc-build` runs no library tests. Patch release pending.
- [x] 2026-09-27 — fourth pass (no code change since the third): verified
      TLS, rate limit, web UI and test-RPM claims; `clients` allow-list gap in
      multi-export mode filed as #97 and documented.
- [x] 2026-10-06 — #100: README/CLAUDE.md say nextnfs is a stormcentral
      component with a golden (checked against `stormcentral component
      export`); #98 (request the golden) closed as already done.
- [x] 2026-10-06 — #99: SECINFO/SECINFO_NO_NAME advertise only AUTH_SYS
      (no krb5*, no AUTH_NONE); RPCSEC_GSS calls rejected with
      MSG_DENIED/AUTH_BADCRED instead of being served as uid 0.
      `sc-build 'cargo test --workspace'` green (546 tests).
- [x] 2026-10-06 — #97: `clients` checked on every export switch.
      LOOKUP from the pseudo-root refuses a denied export (NFS4ERR_ACCESS);
      the COMPOUND dispatcher refuses any op that leaves the client on an
      export it is not allowed (defense in depth); RESTOREFH restores the
      saved handle's export (it kept the previous one, which also let a
      write into a read-only export through); pseudo-root READDIR hides
      denied exports. Handles now carry their export id in byte 1 (it was
      0, so PUTFH skipped the list everywhere). `sc-build 'cargo test
      --workspace'` on ce4338d green (551 tests).
- [x] 2026-10-06 — #96: DEB owns `/export`, `/var/lib/nextnfs`, ships
      `nextnfs-stress`; built with `--root-owner-group` (an unprivileged
      build made every file owned by the build user); `build-deb.sh` honours
      `CARGO_TARGET_DIR`; unit `ReadWritePaths=-/export -/var/lib/nextnfs`.
      Verified on dev (b426601): musl build, `build-deb.sh amd64`,
      `dpkg-deb -c` (root:root, both dirs, both binaries),
      `systemd-analyze verify` OK. No root on dev, so `dpkg -i` + unit
      start on a clean host was not run.
- [x] 2026-10-06 — #95: `build-rpm.sh` stages `nextnfs-stress` (spec
      Source3) and honours `CARGO_TARGET_DIR`; spec changelog weekday fixed.
      Verified on dev (8aeffa6): musl build, `build-rpm.sh x86_64`,
      `rpm -qlpv` lists both binaries, `/export`, `/var/lib/nextnfs`
      (root:root); extracted binaries run.
- [x] 2026-10-06 — #92: `rdma_device`/`rdma_port` warned at startup that
      there is no NFS-over-RDMA transport and the keys are ignored (instead of
      logging "RDMA transport configured"); docs say the same. A real
      verbs transport would be a new feature, not this fix.
      `sc-build 'cargo test --workspace'` on ff30e99 green (553 tests;
      first attempt failed to compile, #104).
- [x] 2026-10-10 — #91 decided (owner 2026-10-09: option A + default
      `root_squash`, breaking minor bump → 0.14.0). In progress:
      new `nfs/src/server/perm.rs` — `Caller` (AUTH_SYS uid/gid/gids mapped
      through the export's squash; AUTH_NONE/other flavours = anon),
      per-op mode-bit checks in the COMPOUND dispatcher (dir w+x for
      create/remove/rename/link, sticky bit, x for LOOKUP, r for READDIR,
      r/w for OPEN/READ/WRITE with owner override, owner-only chmod/utimes,
      root-only chown, owner chgrp to own groups), new objects from
      OPEN/CREATE chowned to the caller (setgid dir keeps its gid);
      ACCESS uses the same caller (also fixes #101); GETATTR owner rewrite
      removed; `SquashMode` default `root_squash`, `AccessConfig` Default
      gives 65534. nfsop side: glennswest/nextnfs-operator#13.
      Code pushed (d3d4eec, dd89aa4 `--squash` CLI flag + test harness
      `--squash none`, 229b1a8); docs/CHANGELOG done (1e1c167). Next:
      `sc-build 'cargo test --workspace && cargo clippy --workspace
      --all-targets'` on 229b1a8 green (577 tests; clippy warnings only in
      pre-existing code: filehandle.rs, nfstest, stress). Released v0.14.0;
      #91 and #101 closed; golden-nextnfs-1e46268c0918 (9a97461), release
      request glennswest/stormcos#158. #90 is verified by the same
      build (its code is in 229b1a8).
- [x] 2026-10-06 — #90 (shipped in v0.14.0, code in 229b1a8, whose build was green): `max_bytes_per_sec` enforced in the COMPOUND
      loop — READ (requested count) and WRITE (data length) charge the
      export's byte bucket, NFS4ERR_DELAY when over; an op bigger than the
      bucket is let through once it is full (bucket goes into debt).
      Code + docs pushed (f08bfb7, 3b7b9b3). `sc-build 'cargo test
      --workspace'` got no slot in an hour twice (exit 75, dev busy); still
      to verify, then close #90 and request the golden.
- [x] 2026-10-13 — #111: serde-xdr (GPL-3.0) replaced by `proto/src/xdr.rs`
      (own serde XDR codec, same wire format, same API); dependency and its
      lock entries removed. `sc-build 'cargo test --workspace && cargo
      clippy --workspace --all-targets'` on 01f038f green (584 tests, no
      lockfile change, no clippy warnings in proto/). Closed;
      golden-nextnfs-efc61bb35b25 (--locked musl build), release request
      glennswest/stormcos#158.
- [x] #101 fixed with #91 (ACCESS uses the squash-mapped caller; AUTH_NONE = anon).
- [x] 2026-10-10 — #106 (RPCSEC_GSS/Kerberos; owner: build it, not
      urgent). Decided 2026-10-13: option A (pure-Rust RustCrypto +
      rasn-kerberos in nextnfs, porting irondirectory's accept/wrap logic).
      #111 done; in progress 2026-10-13. Plan, one commit (or more) each:
      1. new crate `krb5/` (`nextnfs-krb5`): RFC 3961/3962/8009 AES
         enctypes (RustCrypto, RFC test vectors), MIT keytab reader, GSS
         token framing, AP-REQ acceptor (+ AP-REP for mutual auth),
         RFC 4121 MIC + Wrap tokens. Test initiator in its tests.
      2. proto: rpc_gss_cred_t has a version word first (parser read
         gss_proc from it); keep raw call header + raw args for GSS calls;
         GSS reply verifier; raw (wrapped) success body.
      3. server `nfs/src/server/gss.rs`: context table shared across
         connections, INIT/DESTROY/DATA, seq window, krb5/krb5i/krb5p
         unwrap/wrap; principal → uid/gid (config map, then /etc/passwd
         by name, else anon); caller flavour recorded on the request.
      4. config: `[server] krb5_keytab`, `[[server.krb5_map]]`, per-export
         `sec = ["sys","krb5","krb5i","krb5p"]` (default ["sys"]), enforced
         like `clients` on export switch (NFS4ERR_WRONGSEC); SECINFO lists
         the export's flavours.
      5. docs, CHANGELOG, end-to-end tests (initiator → INIT → COMPOUND
         per service), sc-build, minor bump, golden.
      Status 2026-10-14: steps 1–4 pushed (c0fac99 krb5 crate, a6c896e
      proto, 7a27a54 server, 588e25b tests, 46e029f MIT interop test,
      fbf0b97 docs/CHANGELOG). The build VMs DO have MIT krb5
      (krb5kdc, kadmin.local, kinit, krb5-devel, python3-gssapi, gcc):
      `tests/krb5-interop.sh` runs an unprivileged KDC and drives nextnfs
      with MIT libgssapi_krb5 (dlopen, `tests/krb5-interop/`). No root,
      so no kernel `mount -o sec=krb5*`. Done 2026-10-14: v0.15.0
      (a168192), `cargo test --workspace --locked` 621 green, clippy 0
      warnings, MIT interop PASS (4 enctypes × krb5/krb5i/krb5p);
      golden-nextnfs-acf1141301b3, release request stormcos#158; #106
      closed; config keys posted on nextnfs-operator#18. Follow-up: #114
      (owner strings as name@domain for krb5 clients).
      Background: the golden is one static musl binary (rustls, no OpenSSL);
      rocketsmbd links C GSSAPI (not static-linkable), irondirectory has a
      pure-Rust acceptor + RFC 4121 wrap (`crates/ldap/src/gssapi`) on
      OpenSSL FIPS (`ossl`, dynamic). Options were: (A, chosen) RustCrypto +
      rasn-kerberos in nextnfs, porting irondirectory's logic; (B) reuse
      iron-crypto/FIPS; (C) shared crate, pluggable crypto. Either way it
      goes after #111 (serde-xdr replacement): krb5i/krb5p wrap RPC bodies
      in that codec. Operator keytab side: irondirectory-operator#11.
- [x] 2026-11-07 — docs refresh from code since 2026-10-11 (#106 Kerberos, #111 xdr codec): README crate list (six crates, `nextnfs-krb5`), this file's layout. Config keys/defaults, `sec`, test counts already matched the code. Open gaps stay as issues #105, #108, #113, #114, #116.
- [x] 2026-11-14 — docs refresh pass: no code commits since 2026-11-07; docs verified unchanged-correct, no new issues.
- [x] 2026-11-18 — #117 (P-1, no stubs): `rdma_device`/`rdma_port` removed
      (RDMA is #110); unused `rdma.rs`/`nfs42.rs` modules deleted; OPEN
      CLAIM_DELEGATE_CUR → BAD_STATEID, CLAIM_DELEGATE_PREV → NOTSUPP (no
      delegations are granted); nextnfstest stub tests removed (W42-*,
      W41-020 → server gap #118), unknown test IDs say so. Breaking →
      0.16.0 (c0d47e2, tag v0.16.0). Proof grep clean outside tests/; sc-build
      test+clippy on c0d47e2 green (604 tests, 0 warnings); image build
      4c97f0a463f6 requested; #117 closed.
- [x] 2026-11-18 — #118 (P2): NFSv4.1 SEQUENCE exactly-once semantics.
      Plan: per-slot state in `nfs41.rs` (last seqid, in-use flag, cached
      `Compound4res`); SEQUENCE: seqid == last+1 → claim slot and execute;
      == last → replay cached reply (or NFS4ERR_RETRY_UNCACHED_REP on the
      next op when it was not cached); slot busy → NFS4ERR_DELAY; else
      NFS4ERR_SEQ_MISORDERED. COMPOUND stores the reply when `sa_cachethis`
      (or SEQUENCE-only) and it fits `ca_maxresponsesize_cached` (capped at
      64 KiB); a dropped request frees its slot. nextnfstest W41-020 back.
      Code pushed 9fedec0; CHANGELOG/README done. The live nextnfstest
      -V 4.1 run then failed every test with GARBAGE_ARGS: #121 (enums
      encoded by variant position; every v4.1 op from 41 up was the wrong
      op; LOCK READ_LT read as WRITE_LT), fixed in c9749c8. `sc-build`
      on c9749c8: 612 tests, clippy 0 warnings, live `nextnfstest -V 4.1
      -l wire` 6/6 incl. W41-020. Released v0.16.1 (afc3f87, --locked
      build green); #118, #121 closed; image build d1a36bc38821 requested.
- [x] 2026-11-20 — #122 (P0, 4 critical review findings). Plan:
      b31fc72eff57 + 7db43602328a (symlinks): `perm::check_confined` in the
      COMPOUND dispatcher — single-component names (BADNAME), handles reached
      through a symlinked directory → STALE, dir ops on a symlink → SYMLINK,
      OPEN of a symlink name → SYMLINK, READ/WRITE/COMMIT/ALLOCATE/SEEK/COPY
      on a symlink → INVAL, SETATTR size/mode/ACL on one → INVAL; `set_attr`
      uses lchown + AT_SYMLINK_NOFOLLOW; WRITE opens with O_NOFOLLOW.
      a163d43c8336: CREATE dir on an existing name → EXIST.
      ea1e5db68528: API writes (POST/DELETE exports, PUT qos) need a bearer
      token (`[server] api_token_file`, `--api-token-file`); none set → 403;
      optional `api_export_roots` confine API-added exports.
      Done: 0168750 (symlinks, CREATE), 6e6a145 (API token), a0398c1
      (test fix); `sc-build 'cargo test --workspace --locked && cargo clippy
      --workspace --all-targets --locked'` on a0398c1 green (635 tests,
      0 warnings). Breaking (API) → v0.17.0. Residual: the symlink checks run
      before the op, so a directory swapped for a symlink in between is a
      narrow race; descriptor-relative opens (openat2 RESOLVE_BENEATH)
      would close it. The majors of the same review follow as P1.
- [x] 2026-11-20 — #123 (P1): close the symlink-swap race left by #122.
      Plan: `nfs/src/server/beneath.rs` — `ExportRoot` holds an O_PATH fd on
      the export root; every path is opened with `openat2(RESOLVE_BENEATH |
      RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)` from it (Linux >= 5.6;
      an export is refused when openat2 is missing), and ops run on the
      returned fd or as `*at` calls on a confined parent fd (unlinkat,
      renameat, linkat, symlinkat, readlinkat, mkdirat, fchownat,
      utimensat NOFOLLOW, fchmodat2/proc fd, ftruncate, pwrite). `BeneathFS`
      (a `vfs::FileSystem` on it) replaces AltrootFS/PhysicalFS; WRITE,
      LINK, RENAME, CREATE symlink/dir, READLINK, SETATTR, chown of new
      objects, stat (RealMeta) and the write cache use `ExportRoot` directly.
      Test: a thread swaps a directory for a symlink to an outside dir in a
      loop while WRITEs run; the outside file never changes.
      Status 2026-11-20 (paused for P0 #125): code, tests, docs, CHANGELOG
      pushed (5741704 … 9b2a9a5). 2c124c7 failed to compile (borrow of a
      temporary `file_manager()`, non-Copy StableHow4), fixed in 9b2a9a5.
      Done: the race test caught a vfs O_APPEND fallback on NotFound
      (#126, fixed fded1fc). `sc-build 'cargo test --workspace --locked &&
      cargo clippy --workspace --all-targets --locked -- -D warnings'` on
      fded1fc green (642 tests, 0 warnings). Shipped in v0.18.0 (aefebc4)
      with #125; image build c035a3fc7a81 requested; #123 closed.
- [x] 2026-11-20 — #125 (P0, review finding ea1e5db68528 on 8a66330, before
      #122's token): the rest of the finding's fix. API listens on
      127.0.0.1:8080 by default (`config::DEFAULT_API_LISTEN`; the container,
      golden registry config and nfsop all set 0.0.0.0:8080 explicitly);
      POST /api/v1/exports needs a non-empty `api_export_roots` (403
      otherwise, even with the token). Breaking → v0.18.0 after sc-build.
      Build on 9988108: #125 tests green; #123's race test failed (#126):
      WRITE fell back to the vfs (O_APPEND) when the swap made the file
      NotFound — appended inside, never outside. Fixed: vfs fallbacks
      (WRITE, RENAME, SETATTR size) only for `FileManagerHandle::new`
      (MemoryFS tests, `vfs_only`); real-export NotFound WRITE → STALE.
      `sc-build 'cargo test --workspace --locked && cargo clippy --workspace
      --all-targets --locked -- -D warnings'` on fded1fc green (642 tests,
      0 warnings; #126 closed by it). Released v0.18.0 (carries #123's code
      too); #125 closed; image requested. #123 itself: its code is verified
      by the same build — close it when it is handed back.
- [x] 2026-11-20 — #127 (P1, review finding 0d36258c216e): OPEN/CREATE
      createattrs. Plan: new `nfs/src/server/createattrs.rs` — parse and
      validate createattrs (size, mode, owner, owner_group applied; ACL,
      time_*_set, archive, hidden, mimetype, system, time_backup,
      time_create → ATTRNOTSUPP; read-only attrs → INVAL; unknown
      owner → BADOWNER; giving the object away needs root, a group the
      caller is not in → PERM). The object is created with the requested
      permission bits from the start (O_CREAT|O_EXCL / mkdirat with that
      mode, so umask only narrows it), then chowned (caller, or the
      requested owner/group; setgid dir's gid), then chmod to the exact
      mode, then size; attrset says what was applied. The ops do this
      themselves; the dispatcher's `chown_new_object` goes. UNCHECKED4 on
      an existing file no longer truncates it (O_TRUNC did) unless size 0
      is asked; GUARDED4 implemented (EXIST); existing dir → ISDIR.
      Status 2026-11-20: code, tests, docs, CHANGELOG pushed (cdfbb19;
      ed1f56e fixes a duplicate test helper that failed cdfbb19's build,
      #141). Next: `sc-build 'cargo test --workspace --locked && cargo
      clippy --workspace --all-targets --locked -- -D warnings'` on ed1f56e
      (the first attempt sat ~3h in the build queue and was stopped locally
      for low memory before it got a VM), then close #127 and #141, request
      the image. EXCLUSIVE4 verifier gap filed as #140.
- [x] 2026-11-20 — #128 (P1, review finding 124a859a1973): NFSv4.1 session
      IDs came from `sessions.len()` (reused after a DESTROY_SESSION,
      racy between CREATE_SESSIONs) and could overwrite a live session.
      Plan: session id = per-boot random 8 bytes ‖ atomic counter, inserted
      under the same lock as the client check (never replaces an entry);
      client ids = per-boot random high 32 bits ‖ counter (an id from before
      a restart is unknown → STALE_CLIENTID); EXCHANGE_ID registers a client
      record; CREATE_SESSION checks `csa_sequence` against it (last+1 → new,
      == last → the cached reply again, else SEQ_MISORDERED, unknown client
      → STALE_CLIENTID). DESTROY_CLIENTID drops the record.
      Done 2026-11-21: 37be878; `sc-build 'cargo test --workspace --locked
      && cargo clippy --workspace --all-targets --locked -- -D warnings'` on
      it green (660 tests, 0 warnings; also verifies #127). #128 and #127
      closed (#141 closed by the build); image requested.
- [x] 2026-11-21 — #142 (P3, 3 minor review findings). 3d2b666851f1:
      `TestManager::run_with_progress` takes the web API's run id (report,
      RunFinished, active run all one id). 1e45f4ce40e3: SECINFO and
      SECINFO_NO_NAME unset the current filehandle on success;
      `NfsRequest::is_pseudo_root` needs a filehandle, so GETATTR after
      them at the pseudo-root is NOFILEHANDLE too. 4d86693f316f:
      SECINFO_NO_NAME PARENT — pseudo-root → NOENT, export root with >1
      export → pseudo-root flavours (`exports_secinfo_flavors`), else the
      export's. Code + CHANGELOG pushed (fd9f10e). Not yet built: two
      `sc-build 'cargo test --workspace --locked && cargo clippy --workspace
      --all-targets --locked -- -D warnings'` attempts on fd9f10e got no VM
      slot (P3, ~60 builds ahead; second one exit 75, then hung, stormcentral#1640);
      two more on 4502937 got an empty job id + 404: stormcentral's DB is in
      the I/O error of stormcentral#1638 (`propose --after` refused with the
      same 500). Done: verified by #129's build on 6d48640 (green, 669
      tests); its first build found GETATTR-without-filehandle STALE (#143,
      fixed cdbe806). #142 closed; image requested.
- [x] 2026-11-21 — #129 (P1, review finding 1cc239368b2c on 8a66330, before
      #122): multi-component names past a directory's search check. The
      dispatcher's `perm::check_confined` (#122, 0168750) already refuses
      `/`, `.`, `..`, NUL and empty names for LOOKUP/CREATE/REMOVE/RENAME/
      LINK/OPEN/SECINFO before the permission checks; the ops themselves
      still joined `dir/name`. Now LOOKUP (`perm::component_path`), OPEN,
      CREATE, REMOVE, RENAME, LINK check the name too; tests for the
      finding's case (OTHER, 0700 `private`, LOOKUP `private/file` →
      BADNAME; one component at a time → ACCESS), OPEN, the mutations, and
      each op called without the dispatcher. Pushed b609041. Its sc-build
      (job 85a4769a0c): all 5 #129 tests ok, but #142's
      `test_secinfo_then_getattr_is_nofilehandle` failed (fd9f10e was never
      built): GETATTR without a filehandle said STALE, not NOFILEHANDLE
      (#143, fixed cdbe806). Build on cdbe806: 669 tests green, clippy
      `ptr_arg` on op_open's `file: &String` (#144; the new
      `component_path(path, file)` call made it a plain slice), now `&str`.
      Done: `sc-build 'cargo test --workspace --locked && cargo clippy
      --workspace --all-targets --locked -- -D warnings'` on 6d48640 green
      (669 tests, 0 warnings, job afafaef97a); #129, #142 closed (#143,
      #144 closed by the build); golden-nextnfs-93bf37f484bd, release
      request stormcos#1506.
- [x] 2026-11-21 — #130 (P1, 2 major review findings on op_readdir.rs at
      8a66330). 33b440b1f96e: cookieverf sampled from names, compared
      unpadded (7-byte name → 7 bytes vs 8 on the wire) → NOT_SAME on every
      continuation; now `listing_verifier` (FNV-1a 64 over the names in
      order, always [u8; 8]), pseudo-root too (export names; was zeros,
      unchecked). 9078222b42e0: dircount 0 ignored maxcount, fixed 200-byte
      estimate; now `Budget` measures each Entry4 with the XDR codec,
      maxcount always enforced, dircount a hint (never refuses the first
      entry), TOOSMALL when nothing fits; pseudo-root budgeted (it returned
      every export). Tests: 7-byte-name continuation, name lengths 1–20 one
      entry per page, verifier changes with the directory, dircount 0 +
      200 files within 1024, 11 attrs × 5 budgets every page ≤ maxcount,
      TOOSMALL, pseudo-root paging/NOT_SAME/TOOSMALL. Done: b7f828f;
      `sc-build 'cargo test --workspace --locked && cargo clippy --workspace
      --all-targets --locked -- -D warnings'` green (676 tests, 0 warnings,
      job 350ff983b5); #130 closed; golden-nextnfs-63427c090188, release
      request stormcos#1506.
- [x] 2026-11-21 — #131 (P1, review finding 45e86120ffb7 on 8a66330):
      COMPOUND left out a failed op whose handler returned no result (and
      the grace refusal), so the reply's status had no matching result.
      Now `nfs40::failed_result(op, status, result)` makes every failed
      op's result: OpError (op + status) for void-on-error unions, LOCK/
      LOCKT DENIED and SETCLIENTID CLID_INUSE keep their body, SETATTR
      always has attrsset; all dispatcher refusals go through it; success
      without a result → SERVERFAULT. Found on the way: LOCK/LOCKT/
      SETCLIENTID unions tagged by position (DENIED sent as 1 / 0) — new
      `xdr_status_union!` in proto; LOCKT OK sent a bogus LOCK4denied
      (now `Lockt4res::Resok4(())`). Build on 8889757: 2 test expectations
      wrong (empty bitmap4 is one zero word; #130's pseudo-root test assumed
      the export list order), fixed b9d7c9d. Done: `sc-build 'cargo test
      --workspace --locked && cargo clippy --workspace --all-targets
      --locked -- -D warnings'` on b9d7c9d green (683 tests, 0 warnings,
      job a822d8c195); #131 closed; golden-nextnfs-7acf58209131, release
      request stormcos#1506.
- [x] 2026-11-21 — #132 (P1, review finding 4bb3de8fbdd6 on 8a66330):
      a stable WRITE whose offset equalled a stat of the file length used
      O_APPEND, so a writer extending the file in between moved the bytes.
      Now every stable WRITE opens `O_WRONLY` beneath the root and
      `pwrite`s at its offset (no stat; missing file → vfs only for
      vfs-only trees, else STALE as before). Test: per round a thread and a
      FILE_SYNC WRITE both write one block at EOF, released by a barrier;
      the file grows by exactly one block, 300 rounds. README write claims
      corrected. COPY has the same O_APPEND+seek pattern: #145.
      Done: 746c042; `sc-build 'cargo test --workspace --locked && cargo
      clippy --workspace --all-targets --locked -- -D warnings'` green
      (684 tests, 0 warnings, job 63e70ec348); #132 closed; golden-nextnfs-52265f817560,
      release request stormcos#1506.
- [ ] 2026-11-21 — #133 (P1, 4 major review findings on filemanager/mod.rs
      at 8a66330). 60ad262caa93: share reservations enforced in one
      file-manager step (`open_reservation`: SHARE_DENIED across
      open-owners, same owner merges into its stateid, seqid+1);
      OPEN_DOWNGRADE narrows it (`downgrade_open`), OPEN_CONFIRM picks the
      given stateid. 696ec94aaf26 (+ #140): EXCLUSIVE4 verifier in
      atime/mtime, retry → OK, else EXIST (UNCHECKED truncation was #127).
      9c0c932c5136: inode handles checked by dev:ino (`id_names`) in
      get_filehandle_by_id/by_path and the connection cache → STALE on a
      replaced path; no descriptors are held for opens (STALE, not the
      original object). fdfec7e60065 (handles after restart): needs the
      owner's choice of design — not done; README claim corrected.
      Build on 5bbd1ab: `sc-build 'cargo test --workspace --locked && cargo
      clippy --workspace --all-targets --locked -- -D warnings'` green (690
      tests, 0 warnings, job c2665f720a); #140 closed. #133 stays open for
      fdfec7e60065 — waiting on the owner's choice (options posted on the
      issue 2026-11-21; asked again, no answer yet).
- [x] 2026-11-21 — #134 (P1, review finding 739140171b7d on 8a66330):
      SEQUENCE validation + replay were done by #118 (`begin_slot`,
      SlotCheck); what was left is placement. `sequence_placement_error`
      in the COMPOUND loop: later SEQUENCE → SEQUENCE_POS (ops before it
      ran, slot caches the reply); v4.1 without SEQUENCE first →
      OP_NOT_IN_SESSION unless a session-less op (41–44, 57) alone, else
      NOT_ONLY_OP (new NfsStat4 10081); SEQUENCE in v4.0 → OP_ILLEGAL.
      `test_compound_minor_version_1_accepted` asserted the old behaviour:
      now `_needs_sequence`. nfstest v4.1 already conforms.
      Done: f2c98de; `sc-build 'cargo test --workspace --locked && cargo
      clippy --workspace --all-targets --locked -- -D warnings'` green (692
      tests, 0 warnings, job f5b4e61a87); #134 closed; golden-nextnfs-33899f78a596 (also
      carries #133's three fixes), release request stormcos#1506.
- [x] 2026-11-21 — #135 (P1, review finding 77bd1161399f on 8a66330):
      the per-export quota counted WRITE traffic, never freed, started at
      0 — and nothing could configure it (no config key/CLI/API; default 0
      = unlimited), so it was a stub. Removed per the no-stubs rule
      (QuotaConfig/QuotaManager, set/get_quota, op checks, quota_* attrs
      out of supported_attrs); real quotas filed as gap #146 (proposed
      after #135). Also: space_*/files_* now from fstatvfs of the export
      root (were a fixed 1 TiB minus traffic, files = bytes/4096);
      stable WRITE errors via `io_status` (NOSPC/DQUOT/ISDIR, was IO).
      Breaking (library API, supported_attrs) → next minor.
      Done: 0841810; `sc-build 'cargo test --workspace --locked && cargo
      clippy --workspace --all-targets --locked -- -D warnings'` green (690
      tests = 692 − 3 quota tests + 1, 0 warnings, job 1b79c8b3af); #135
      closed; golden-nextnfs-accebe0eec60, release request stormcos#1506.
- [ ] 2026-11-21 — #136 (P1, review finding 8e411131c3ba on 8a66330):
      unstable WRITE answered once queued (failed pwrite only logged);
      COMMIT logged a failed fsync, cleared dirty, dropped the cache and
      said OK with the same verifier. Now write/commit results come back
      through the cache actor (`WriteCacheError`, write_status/
      commit_status); a failed fsync keeps cache + dirty and bumps the
      per-export write epoch (verifier = boot_time + epoch<<32), so a
      later COMMIT cannot vouch for WRITEs answered before; CLOSE's flush
      keeps its failure for the next COMMIT; COMMIT always fsyncs; on a
      real export the cache opens without O_CREAT (removed file → STALE).
      Tests: pipe fd as the failing file (pwrite ESPIPE, fsync EINVAL),
      unstable WRITE at an off_t-overflow offset → IO, removed file →
      STALE. Next: sc-build, close, image.
