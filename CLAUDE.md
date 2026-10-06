# CLAUDE.md — nextnfs

Standalone NFSv4 server over a real filesystem, in Rust. Cross-project rules
live in `../CLAUDE.md`; this file is the project context and work plan.

## Version

**0.13.9** (tag `v0.13.9`). Version locations — all must match:

- `Cargo.toml` → `[workspace.package] version` (every crate inherits it)
- `build.sh` → `VERSION=`
- `README.md` → "Current version" line and RPM/DEB file names

RPM specs and `packaging/deb/control` take the version from `Cargo.toml` at
package time (`packaging/build-*.sh`, `ci-rpm.sh`); do not hard-code it there.

## Layout

| Crate | Dir | What |
|---|---|---|
| `nextnfs-proto` | `proto/` | XDR codec, RPC + NFSv4 types |
| `nextnfs-server` | `nfs/` | server library: `ExportManager`, `FileManager` actors, COMPOUND dispatch, locking, sessions, TLS, state recovery |
| `nextnfs` | `.` | binary: CLI (`src/main.rs`, `src/cli.rs`), TOML config (`src/config.rs`), REST API (`src/api.rs`), web UI (`src/web.rs`) |
| `nextnfstest` | `nfstest/` | wire-level NFS protocol test suite |
| `nextnfs-stress` | `stress/` | POSIX stress harness against a live mount |

Other: `tests/*.sh` (shell integration suites, shipped as the `nextnfs-tests`
RPM with `packaging/nextnfs-run-tests`), `packaging/` (RPM/DEB/systemd),
`Containerfile*` + `stormd.toml` (stormd container), `ci/` (pipeline scripts),
`doc/` (deploy guide), `enhancements/` (design proposals).

## How it ships

- **RPM / DEB** (`make rpm-x86|deb-x86`, `packaging/build-*.sh`): `/usr/bin/nextnfs`,
  `/usr/bin/nextnfs-stress`, `/etc/nextnfs/nextnfs.toml` (from
  `nextnfs.example.toml`), `nextnfs.service` running `serve --config /etc/nextnfs/nextnfs.toml`.
  RPM `%post` enables the service but does not start it; DEB `postinst`
  enables and starts it. The DEB lacks `nextnfs-stress` and does not create
  `/export` / `/var/lib/nextnfs` (#96). `make rpm-*` is broken (#95) —
  `ci-rpm.sh` is the working RPM path.
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

Config keys that parse but do not do what their names say:
`max_bytes_per_sec` (not enforced), `squash`/`anon_uid`/`anon_gid` (only
rewrites GETATTR owner display), `rdma_device`/`rdma_port` (logged only). Packaging: #95 (`make rpm-*`),
#96 (DEB dirs). See `gh issue list`.

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
- [ ] #101 (ACCESS as uid 0 for non-AUTH_SYS), #95/#96 (packaging),
      #90/#91/#92.
