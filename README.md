# nextnfs

High-performance standalone NFSv4.0 server over a real filesystem. Runs as a static musl binary in a scratch/stormd container.

## Features

- **NFSv4.0** — full compound operations, OPEN/CLOSE, READ/WRITE/COMMIT, READDIR, READLINK, CREATE, REMOVE, RENAME
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
  -p 2049:2049 -p 9080:9080 -p 2222:22 \
  registry.gt.lo:5000/nextnfs:latest

# Mount from a client
mount -t nfs4 server:/ /mnt
```

Container includes [stormd](https://github.com/glennswest/stormd) for process supervision, auto-restart, logging, SSH, and web dashboard.

| Port | Service |
|------|---------|
| 2049 | NFS |
| 9080 | stormd web dashboard + REST API |
| 22   | SSH shell (password: `nextnfs`) |

### Binary

```bash
nextnfs --export /path/to/share --listen 0.0.0.0:2049
```

### With config file

```bash
nextnfs --config nextnfs.toml
```

```toml
[server]
listen = "0.0.0.0:2049"

[export]
path = "/export"
read_only = false
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
```

## Architecture

Three-crate Rust workspace:

- **nextnfs-proto** — XDR codec, NFS4/RPC protocol types
- **nextnfs-server** — NFSv4.0 server library (FileManager actor, compound operations, locking)
- **nextnfs** — CLI binary with clap + TOML config

Binary size: ~3.4 MB stripped (x86_64-musl), ~3.1 MB (aarch64-musl).

## License

MIT — derived from [bold-nfs](https://github.com/nicholasgasior/bold-nfs) by Michael Schilonka.
