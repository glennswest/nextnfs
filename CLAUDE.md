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
- nextnfs is **not** a stormcentral component (no golden; not in
  `stormcentral component list`). There is no `test/` test container yet.

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
- [ ] #93 verify: `sc-build 'cargo test --workspace'` for 3f2ef53 was queued
      (not yet run) at the 2026-09-27 session restart — rerun it. Note plain
      `sc-build` (`cargo test`) only tests the root crate, so the lib tests
      never ran there; the first workspace run failed to compile on a private
      const (fixed in 3f2ef53, build-failure issue #94 — close once green).
      Then close #93 and consider a patch release.
- [ ] #90/#91/#92.
