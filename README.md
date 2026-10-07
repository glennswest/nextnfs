# nextnfs

High-performance, standalone **NFSv4.0/4.1 server over a real filesystem**, written in Rust. Runs as a static musl binary in a scratch/[stormd](https://github.com/glennswest/stormd) container, or as an RPM/DEB service on Fedora/RHEL/Debian. Multiple exports, a REST API + web UI for management, and NFSv4 protocol correctness verified against real Linux clients.

Current version: **0.13.9**.

## Features

- **NFSv4.0** — full compound operations: OPEN/CLOSE/OPEN_DOWNGRADE, READ/WRITE/COMMIT, READDIR, READLINK, CREATE, REMOVE, RENAME, LINK, VERIFY/NVERIFY, SECINFO
- **NFSv4.1 sessions** — COMPOUNDs with minor version 1 are accepted: EXCHANGE_ID, CREATE_SESSION, SEQUENCE, BIND_CONN_TO_SESSION, DESTROY_SESSION/CLIENTID, RECLAIM_COMPLETE. Minor version 2 (NFSv4.2) is rejected with `NFS4ERR_MINOR_VERS_MISMATCH`; clients should mount with `vers=4.1` or `vers=4.0`. (The dispatcher also routes ALLOCATE/COPY/SEEK, but only inside a v4.0/4.1 COMPOUND.)
- **Byte-range locking** — LOCK, LOCKT, LOCKU, RELEASE_LOCKOWNER with conflict detection
- **Multi-export** — serve multiple filesystem paths as separate exports under an NFSv4 pseudo-filesystem root; single-export mode is fully backwards-compatible
- **Real-filesystem semantics** — `stat()`-based attributes (mode/uid/gid/nlink/atime/mtime/ctime), inode-based persistent file handles (`dev:ino`, survive restart), hard links and symlinks; every WRITE is a `pwrite`/`O_APPEND` write followed by `fsync` before the reply
- **Per-export access control** — `clients` allow-list of IPs/CIDRs (checked whenever a COMPOUND moves onto an export — PUTFH, single-export PUTROOTFH, LOOKUP from the pseudo-root, RESTOREFH — others get `NFS4ERR_ACCESS`; pseudo-root READDIR lists only the exports the client may enter); `max_ops_per_sec` rate limit (excess COMPOUNDs get `NFS4ERR_DELAY`)
- **Authentication: AUTH_SYS only** — SECINFO and SECINFO_NO_NAME advertise `AUTH_SYS` alone, so clients mount with the default `sec=sys`. There is no Kerberos: RPCSEC_GSS credentials are parsed but no GSS context is set up and there is no keytab, so a call carrying one (e.g. `mount -o sec=krb5`) is denied with `AUTH_BADCRED` ([#99](https://github.com/glennswest/nextnfs/issues/99)). AUTH_NONE calls are still accepted but not advertised
- **RPC-over-TLS** (RFC 9289) — set `tls_cert` + `tls_key` and the NFS listener requires TLS on every connection
- **State recovery** — with `state_dir` set, client state is snapshotted every 30 s and restored on restart; the server never holds a grace period
- **REST API + Web UI** — manage exports, view per-export stats, health checks (axum on :8080); dark-themed dashboard; in the container, stormd shows it as a UI tab by proxying `127.0.0.1:8080` (`[process.ui]` in `stormd.toml`)
- **Operationally lean** — ~5 MB stripped static binary (x86_64-musl), TCP tuning (4 MB socket buffers, `TCP_NODELAY`, keepalive), scratch container

## NFSv4 correctness

nextnfs is tested against the real Linux kernel NFS client, and much of its maturity is in the protocol edge cases that only surface there:

- **Nanosecond `change` attribute** — packs `mtime_sec·1e9 + mtime_nsec` so bursts of concurrent ops within one wall-clock second invalidate the client's readdir cache correctly (a second-resolution `change` let clients `rmdir` against stale dentries and return `ENOTEMPTY` locally)
- **Inode-preserving RENAME** — uses `std::fs::rename()` (atomic, preserves inode) instead of a copy-and-delete fallback that changed the fileid and triggered `ESTALE`
- **Silly-rename handling** — recovers filehandles for both client-side (`.nfs*`) and server-side silly-renames via inode-based fallback; defers deletion of open-but-removed files to a periodic sweep so an open fd can still READ after CLOSE
- **Crash-resistant filehandle DB** — a self-healing `fhdb_replace` helper replaced panicking `MultiIndexMap` inserts that could kill the `FileManager` actor and cascade every subsequent op into `NFS4ERR_SERVERFAULT`
- **UTF-8 filenames** — a custom `utf8_opaque` XDR serializer removes the ASCII-only restriction so names like `filé_ñame_日本語` serialize instead of timing out the client
- **Correct sparse/append writes** — `O_APPEND` only at exact EOF; `pwrite` for writes past EOF (preserving holes); atomic concurrent appends

## Testing

Two companion workspace crates:

- **`nextnfstest`** — comprehensive NFS protocol test suite (v3, v4.0, v4.1, v4.2), wire-level, with an HTML report and web view
- **`nextnfs-stress`** — POSIX-path stress harness aimed at a live NFS mount: 1000 files across nine phases (create, readdir, stat, read+verify, rename rotation, post-rename stat, unlink-while-open silly-rename trigger, parallel workers, bulk delete), reporting per-phase ops/s and errno breakdowns

Plus shell integration suites in `tests/*.sh` (NFSv4 basic/edge/stress, NFSv4.1 sessions, integrity, performance), packaged as the `nextnfs-tests` RPM and run with `nextnfs-run-tests <server> [suite...]`.

553 unit tests pass with `cargo test --workspace` (474 `nextnfs-server`, 71 `nextnfs-proto`, 6 `nextnfstest`, 2 in the `nextnfs` binary's config parser). `ci/ci-stress.sh` drives an end-to-end pipeline (written for the now-retired mkube runners): build RPM → `rpm -Uvh` on each target → restart service → run `nextnfs-stress` against the live mount → collect per-server logs and journals.

## Quick start

### Container (recommended)

```bash
podman run -d \
  -v /export:/export:z \
  -p 2049:2049 -p 8080:8080 -p 9080:9080 -p 2222:22 \
  registry.gt.lo:5000/nextnfs:latest
mount -t nfs4 server:/ /mnt
```

| Port | Service |
|------|---------|
| 2049 | NFS |
| 8080 | nextnfs REST API + Web UI |
| 9080 | stormd dashboard + REST API |
| 22   | SSH shell (stormd; password in `stormd.toml`) |

In the container, stormd (`stormd.toml`) starts `nextnfs serve --export /export --listen 0.0.0.0:2049 --api-listen 0.0.0.0:8080` — flags only. The `/etc/nextnfs/nextnfs.toml` baked into the image is **not** read; to use a config file, change the process `args` in `stormd.toml` to `serve --config /etc/nextnfs/nextnfs.toml`. The container images are built by `build.sh` / `make container-*` from a locally built static binary (`Containerfile.x86_64` for x86_64, `Containerfile` for aarch64) on top of `registry.gt.lo:5000/stormdbase`.

### RPM / DEB

```bash
sudo rpm -i nextnfs-0.13.9-1.x86_64.rpm     # Fedora/RHEL
sudo dpkg -i nextnfs_0.13.9_amd64.deb        # Debian/Ubuntu
```

| | RPM | DEB |
|---|---|---|
| `/usr/bin/nextnfs`, `/etc/nextnfs/nextnfs.toml`, `nextnfs.service` | yes | yes |
| `/usr/bin/nextnfs-stress` | yes | yes |
| creates `/export`, `/var/lib/nextnfs` | yes | yes |
| after install | service **enabled**, not started (`systemctl start nextnfs`) | service enabled and started |

`nextnfs.service` runs `nextnfs serve --config /etc/nextnfs/nextnfs.toml` as root with systemd hardening; only `/export` and `/var/lib/nextnfs` are writable (`ReadWritePaths`, each `-`-prefixed so a missing one does not stop the unit), so exports elsewhere need a drop-in that adds their paths. The shipped config is `nextnfs.example.toml` (single export `/export`, `state_dir = /var/lib/nextnfs`), marked `%config(noreplace)`.

### stormcos golden

nextnfs is also a registered stormcentral component (`stormcentral component list` / `component export`): kind `service`, optional — it ships in the stormcos release but is started only by its operator, `nfsop` ([nextnfs-operator](https://github.com/glennswest/nextnfs-operator)), one pod per NFSServer with its own drive at `/export`. In the golden, stormd runs `nextnfs --config /etc/nextnfs/nextnfs.toml` (stormcentral's registry holds that config: one `[export]` at `/export`, `state_dir = /var/lib/nextnfs`; the operator mounts its own over it), health is `GET /health` on 8080, and the golden's stormd listens on 8180 — not the 9080 of this repo's `stormd.toml`, which only the container image above uses. The registry entry (port, health, argv, config) lives in stormcentral's database and is changed with `stormcentral component edit nextnfs`, not in this repo.

Goldens are built from pushed commits on request (`stormcentral component build nextnfs`); the first is `golden-nextnfs-81fcd9ea25dc` (0.13.9, `a3d625d`).

### Binary / config

```bash
nextnfs --export /path/to/share --listen 0.0.0.0:2049
nextnfs --config nextnfs.toml
```

```toml
[server]
listen = "0.0.0.0:2049"
api_listen = "0.0.0.0:8080"

[[exports]]
name = "data"
path = "/data"
read_only = false

[[exports]]
name = "backup"
path = "/backup"
read_only = true
```

## Configuration reference

TOML, loaded with `--config FILE` (see `nextnfs.example.toml`). Every key is optional except `name`/`path` in an `[[exports]]` entry.

**`[server]`**

| Key | Default | Meaning |
|---|---|---|
| `listen` | `0.0.0.0:2049` | NFS TCP listen address |
| `api_listen` | `0.0.0.0:8080` | REST API + web UI listen address (plain HTTP, no auth) |
| `state_dir` | unset | Directory for client-state snapshots (created if missing); written every 30 s, restored and cleared at startup |
| `tls_cert`, `tls_key` | unset | PEM cert and key; when **both** are set the NFS port speaks RPC-over-TLS only |
| `rdma_device`, `rdma_port` | unset | **Not implemented** — accepted so a config that sets them still loads, but no RDMA listener is started; nextnfs logs a warning at startup that they are ignored and serves TCP only ([#92](https://github.com/glennswest/nextnfs/issues/92)) |

**`[[exports]]`** (repeatable)

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Export name — the top-level directory under the pseudo-root, and the API key |
| `path` | required | Existing directory; canonicalized at startup, the server exits if it is missing or not a directory |
| `read_only` | `false` | Refuse changes with `NFS4ERR_ROFS` (client sees `EROFS`): WRITE, COMMIT, CREATE, REMOVE, RENAME, LINK, SETATTR, ALLOCATE, COPY, and OPEN that creates or asks for write access. ACCESS never grants MODIFY/EXTEND/DELETE |
| `clients` | `[]` (all) | Allowed client IPs or CIDRs (IPv4/IPv6). Others get `NFS4ERR_ACCESS` on any op that moves them onto the export (PUTFH, single-export PUTROOTFH, LOOKUP from the pseudo-root, RESTOREFH), and the export is left out of pseudo-root READDIR |
| `max_ops_per_sec` | `0` (unlimited) | Per-export operation rate limit |
| `max_bytes_per_sec` | `0` | **Not enforced** yet ([#90](https://github.com/glennswest/nextnfs/issues/90)) |
| `squash` | `""` (none) | `root_squash` or `all_squash`; any other value means none. **Today this only rewrites the owner/group shown by GETATTR** — requests still run as the server's uid ([#91](https://github.com/glennswest/nextnfs/issues/91)) |
| `anon_uid`, `anon_gid` | `65534` | Identity shown for squashed owners |

**`[export]`** (legacy single export): `path` (default `/export`) and `read_only`. It is merged in front of `[[exports]]` (named after the path's last component) unless an `[[exports]]` entry already has the same path.

Precedence: if the config has no exports, the `--export` path (default `/export`) is used. `--listen` / `--api-listen` override the file only when they differ from their defaults. With exactly one export, `PUTROOTFH` lands directly in that export (`mount server:/`); with several, the root is a pseudo-filesystem listing the export names (`mount server:/data`). Logging is controlled by `RUST_LOG` (default `info`).

Exports added through the API or CLI live in memory only; they are gone after a restart unless they are also in the config file.

## CLI

```
nextnfs [serve] [-e|--export PATH] [-l|--listen ADDR] [-a|--api-listen ADDR] [-c|--config FILE]
nextnfs export list | add --name N --path P [--read-only] | remove --name N
nextnfs stats | health   [--api URL]     # default http://127.0.0.1:8080
```

## REST API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | `{"status":"ok","exports":N}` |
| GET | `/api/v1/exports` | List exports: `name`, `path`, `read_only`, `export_id`, `stats` |
| POST | `/api/v1/exports` | Add `{"name","path","read_only"}` (in-memory; no `clients`/QoS/squash) |
| DELETE | `/api/v1/exports/{name}` | Remove export |
| GET | `/api/v1/stats` · `/api/v1/stats/{name}` | Server totals (`total_reads`, `total_writes`, `total_bytes_read`, `total_bytes_written`, `total_ops`, `exports`) / one export |
| GET · PUT | `/api/v1/qos/{name}` | Read / set `{"max_ops_per_sec","max_bytes_per_sec"}` (bytes limit not enforced, #90); 404 for unknown export |
| GET | `/` · `/ui/exports` · `/ui/stats` | Web UI |

The API has no authentication — bind `api_listen` to a trusted address.

## Architecture

Five-crate Rust workspace:

- **`nextnfs-proto`** (`proto/`) — XDR codec, RPC and NFSv4 protocol types
- **`nextnfs-server`** (`nfs/`) — the NFSv4.0/4.1 server library: `ExportManager` and `FileManager` actors, compound operation handling, locking, v4.1 sessions, pseudo-fs root, TLS, state recovery
- **`nextnfs`** (`.`) — CLI binary (clap subcommands), REST API (axum), Web UI
- **`nextnfstest`** (`nfstest/`) — NFS v3/v4.0/4.1/4.2 protocol test suite
- **`nextnfs-stress`** (`stress/`) — live-mount POSIX stress harness

`ExportManager` owns multiple exports, each with its own `FileManagerHandle`; the NFSv4 pseudo-fs root presents exports as top-level directories. Single-export mode routes `PUTROOTFH` straight to the export root.

> The overlay + dm-verity layering primitives that once lived here were **extracted into the standalone [`rspacefs`](https://github.com/glennswest/rspacefs) project** — layered-rootfs primitives shouldn't carry an NFS server in their data path. nextnfs does **not** depend on rspacefs; the two are independent. nextnfs's scope is now NFS-server-proper plus Fedora/RHEL/Debian packaging.

## Build

```bash
make build            # debug (dev)
make build-x86        # static x86_64-musl (Fedora CoreOS)
make build-arm64      # static aarch64 (MikroTik Rose)
make container-x86 | container-arm64 | push
make rpm-x86 | rpm-arm64 | deb-x86 | deb-arm64
```

`make build-*` builds and strips both `nextnfs` and `nextnfs-stress` (it needs the `x86_64-linux-musl-strip` / `aarch64-linux-musl-strip` cross tools); `make rpm-*` / `deb-*` package them. Without the cross strip tools, run `cargo build --release --target <triple>-unknown-linux-musl -p nextnfs -p nextnfs-stress` and then `packaging/build-rpm.sh <x86_64|aarch64>` or `packaging/build-deb.sh <amd64|arm64>` directly; both scripts look for the binaries under `$CARGO_TARGET_DIR` when it is set. `ci-rpm.sh` (x86_64: workspace tests, clippy, musl build of both binaries, rpmbuild) is the CI path.

Development builds and tests in the stormcentral environment go through `sc-build` (`cargo build && cargo test` on the build box, after `git push`).

Requires Rust 1.75+.

## License

MIT — derived from [bold-nfs](https://github.com/nicholasgasior/bold-nfs) by Michael Schilonka.
