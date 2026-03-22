# Changelog

## [Unreleased]

### 2026-03-21
- **feat:** Initial project structure — NFSv4.0 server extracted from StormFS bold-nfs
- **feat:** nextnfs-proto crate (XDR codec, NFS4/RPC protocol types)
- **feat:** nextnfs-server crate with real filesystem metadata (stat-based attrs)
- **feat:** Inode-based persistent file handles (dev:ino packed into NfsFh4)
- **feat:** TCP socket tuning (4MB buffers, TCP_NODELAY, keepalive)
- **feat:** Channel depth increased from 16 to 256 for throughput
- **feat:** Proper EOF detection in READ operations
- **feat:** CLI binary with clap (--export, --listen, --config)
- **feat:** TOML config file support (server.listen, export.path, export.read_only)
- **feat:** Symlink and hardlink support enabled
