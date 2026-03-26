# nextnfs

High-performance standalone NFSv4.0 server over a real filesystem. Runs as a static musl binary in a scratch/stormd container. Supports multiple exports, REST API management, and a built-in web UI.

## Features

- **NFSv4.0** — full compound operations, OPEN/CLOSE/OPEN_DOWNGRADE, READ/WRITE/COMMIT, READDIR, READLINK, CREATE, REMOVE, RENAME, VERIFY/NVERIFY, SECINFO
- **Multi-export** — serve multiple filesystem paths as separate NFS exports
- **Pseudo-filesystem root** — NFSv4 namespace with exports as top-level directories
- **REST API** — manage exports, view stats, health checks (axum on port 8080)
- **Web UI** — Dracula-themed dashboard, integrates into stormd as an iframe tab
- **Byte-range locking** — LOCK, LOCKT, LOCKU, RELEASE_LOCKOWNER with conflict detection
- **Real filesystem metadata** — stat()-based attrs (mode, uid, gid, nlink, atime/mtime/ctime)
- **Persistent file handles** — inode-based (dev:ino), survives server restarts
- **Hard links and symlinks** — LINK and READLINK operations
- **Direct I/O writes** — no in-memory buffering, fsync on COMMIT
- **TCP tuning** — 4 MB socket buffers, TCP_NODELAY, keepalive

## Quick Start

### Container (recommended)

```bash
# Run on Fedora CoreOS (x86_64)
podman run -d \
  -v /export:/export:z \
  -p 2049:2049 -p 8080:8080 -p 9080:9080 -p 2222:22 \
  registry.gt.lo:5000/nextnfs:latest

# Mount from a client
mount -t nfs4 server:/ /mnt
```

Container includes [stormd](https://github.com/glennswest/stormd) for process supervision, auto-restart, logging, SSH, and web dashboard. The NextNFS web UI appears as an integrated tab.

| Port | Service |
|------|---------|
| 2049 | NFS |
| 8080 | NextNFS REST API + Web UI |
| 9080 | stormd web dashboard + REST API |
| 22   | SSH shell (password: `nextnfs`) |

### RPM (Fedora/RHEL)

```bash
sudo rpm -i nextnfs-0.10.0-1.x86_64.rpm
# Installs to /usr/bin/nextnfs, config at /etc/nextnfs/nextnfs.toml
# Enables and starts nextnfs.service automatically

sudo systemctl status nextnfs
```

### DEB (Debian/Ubuntu)

```bash
sudo dpkg -i nextnfs_0.10.0_amd64.deb
# Installs to /usr/bin/nextnfs, config at /etc/nextnfs/nextnfs.toml
# Enables and starts nextnfs.service automatically

sudo systemctl status nextnfs
```

### Binary

```bash
# Single export (backwards-compatible)
nextnfs --export /path/to/share --listen 0.0.0.0:2049

# Or explicitly use the serve subcommand
nextnfs serve --export /path/to/share --api-listen 0.0.0.0:8080
```

### With config file

```bash
nextnfs --config nextnfs.toml
```

```toml
[server]
listen = "0.0.0.0:2049"
api_listen = "0.0.0.0:8080"

# Single export (legacy, still works)
[export]
path = "/export"
read_only = false

# Multi-export
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
nextnfs export list [--api URL]
nextnfs export add --name NAME --path PATH [--read-only] [--api URL]
nextnfs export remove --name NAME [--api URL]
nextnfs stats [--api URL]
nextnfs health [--api URL]
```

Default API URL for CLI subcommands is `http://127.0.0.1:8080`.

## REST API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check |
| GET | `/api/v1/exports` | List all exports with stats |
| POST | `/api/v1/exports` | Add export `{"name":"x","path":"/x","read_only":false}` |
| DELETE | `/api/v1/exports/{name}` | Remove export |
| GET | `/api/v1/stats` | Server-wide stats |
| GET | `/api/v1/stats/{name}` | Per-export stats |
| GET | `/` | Web UI dashboard |
| GET | `/ui/exports` | Export management page |
| GET | `/ui/stats` | Statistics page |

```bash
# List exports
curl -s http://localhost:8080/api/v1/exports | python3 -m json.tool

# Add an export
curl -s -X POST http://localhost:8080/api/v1/exports \
  -H 'Content-Type: application/json' \
  -d '{"name":"share","path":"/data/share","read_only":false}'

# Remove an export
curl -s -X DELETE http://localhost:8080/api/v1/exports/share

# Server stats
curl -s http://localhost:8080/api/v1/stats | python3 -m json.tool
```

## Build

Requires Rust 1.75+ and [musl-cross](https://github.com/nickhutchinson/homebrew-musl-cross) for cross-compilation.

```bash
# Debug build (macOS, for development)
make build

# Static x86_64 binary (for Fedora CoreOS)
make build-x86

# Static aarch64 binary (for MikroTik Rose)
make build-arm64

# Build and tag x86_64 container
make container-x86

# Build and tag aarch64 container
make container-arm64

# Push to registry
make push

# Build RPM (Fedora/RHEL)
make rpm-x86       # x86_64
make rpm-arm64     # aarch64

# Build DEB (Debian/Ubuntu)
make deb-x86       # amd64
make deb-arm64     # arm64
```

## Architecture

Three-crate Rust workspace:

- **nextnfs-proto** — XDR codec, NFS4/RPC protocol types
- **nextnfs-server** — NFSv4.0 server library (ExportManager, FileManager actors, compound operations, locking)
- **nextnfs** — CLI binary with clap subcommands, REST API (axum), Web UI

The ExportManager actor manages multiple exports, each with its own FileManagerHandle. The NFSv4 pseudo-filesystem root presents exports as top-level directories. Single-export mode is fully backwards-compatible — PUTROOTFH goes directly to the export root.

Binary size: ~5 MB stripped (x86_64-musl) with REST API.

## License

MIT — derived from [bold-nfs](https://github.com/nicholasgasior/bold-nfs) by Michael Schilonka.
