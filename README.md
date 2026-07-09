# nextnfs

High-performance, standalone **NFSv4.0 server over a real filesystem**, written in Rust. Runs as a static musl binary in a scratch/[stormd](https://github.com/glennswest/stormd) container, or as an RPM/DEB service on Fedora/RHEL/Debian. Multiple exports, a REST API + web UI for management, and NFSv4 protocol correctness verified against real Linux clients.

Current version: **0.13.9**.

## Features

- **NFSv4.0** — full compound operations: OPEN/CLOSE/OPEN_DOWNGRADE, READ/WRITE/COMMIT, READDIR, READLINK, CREATE, REMOVE, RENAME, LINK, VERIFY/NVERIFY, SECINFO
- **Byte-range locking** — LOCK, LOCKT, LOCKU, RELEASE_LOCKOWNER with conflict detection
- **Multi-export** — serve multiple filesystem paths as separate exports under an NFSv4 pseudo-filesystem root; single-export mode is fully backwards-compatible
- **Real-filesystem semantics** — `stat()`-based attributes (mode/uid/gid/nlink/atime/mtime/ctime), inode-based persistent file handles (`dev:ino`, survive restart), hard links and symlinks, direct-I/O writes with `fsync` on COMMIT
- **REST API + Web UI** — manage exports, view per-export stats, health checks (axum on :8080); Dracula-themed dashboard that integrates into stormd as an iframe tab
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

464 tests pass. `ci/ci-stress.sh` drives an end-to-end mkube pipeline: build RPM → `rpm -Uvh` on each target → restart service → run `nextnfs-stress` against the live mount → collect per-server logs and journals.

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
| 22   | SSH shell |

### RPM / DEB

```bash
sudo rpm -i nextnfs-0.13.9-1.x86_64.rpm     # Fedora/RHEL
sudo dpkg -i nextnfs_0.13.9_amd64.deb        # Debian/Ubuntu
# installs /usr/bin/nextnfs (+ /usr/bin/nextnfs-stress), config /etc/nextnfs/nextnfs.toml,
# enables and starts nextnfs.service
```

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

## CLI

```
nextnfs [serve] [--export PATH] [--listen ADDR] [--api-listen ADDR] [--config FILE]
nextnfs export list | add --name N --path P [--read-only] | remove --name N
nextnfs stats | health   [--api URL]     # default http://127.0.0.1:8080
```

## REST API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check |
| GET | `/api/v1/exports` | List exports with stats |
| POST | `/api/v1/exports` | Add `{"name","path","read_only"}` |
| DELETE | `/api/v1/exports/{name}` | Remove export |
| GET | `/api/v1/stats` · `/api/v1/stats/{name}` | Server / per-export stats |
| GET | `/` · `/ui/exports` · `/ui/stats` | Web UI |

## Architecture

Five-crate Rust workspace:

- **`nextnfs-proto`** (`proto/`) — XDR codec, RPC and NFSv4 protocol types
- **`nextnfs-server`** (`nfs/`) — the NFSv4.0 server library: `ExportManager` and `FileManager` actors, compound operation handling, locking, pseudo-fs root
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

Requires Rust 1.75+.

## License

MIT — derived from [bold-nfs](https://github.com/nicholasgasior/bold-nfs) by Michael Schilonka.
