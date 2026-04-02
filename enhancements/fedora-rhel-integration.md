# Enhancement: Fedora / RHEL Desktop and Server Integration

**Status:** Proposed
**Date:** 2026-03-31
**Priority:** Medium
**Depends on:** [overlay-vfs.md](overlay-vfs.md) (Phase 1-2)

## Summary

Package NextNFS as a standard system service for Fedora and RHEL, providing a production NFS server (with optional overlay VFS) that integrates natively with systemd, firewalld, SELinux, DNF, and Cockpit. Replace or supplement the legacy `nfs-utils` (kernel NFS) with a single static binary that requires zero configuration for common use cases.

## Motivation

### kernel nfsd is painful to operate

The existing Linux NFS stack (`nfs-utils` + kernel `nfsd`) has decades of accumulated complexity:

- **Multiple daemons** — `rpc.nfsd`, `rpc.mountd`, `rpc.statd`, `rpc.idmapd`, `rpcbind`, `blkmapd`
- **Multiple config files** — `/etc/exports`, `/etc/nfs.conf`, `/etc/idmapd.conf`, `/etc/sysconfig/nfs`
- **portmapper dependency** — `rpcbind` required for NFSv3, source of security issues
- **Export syntax** — arcane, easy to get wrong, no validation until mount time
- **No REST API** — management via `exportfs -a` and file editing
- **No web UI** — need third-party tools or raw CLI
- **SELinux pain** — NFS exports need correct contexts (`nfs_export_all_rw`, `setsebool`, `restorecon`)
- **Firewall pain** — NFSv3 uses random ports, needs `mountd`, `statd` exceptions
- **No metrics** — `nfsstat` gives basic counters, no Prometheus, no per-export stats
- **No QoS** — one noisy client saturates the server for everyone

### NextNFS is simpler

| kernel nfsd | NextNFS |
|-------------|---------|
| 6+ daemons | 1 binary |
| 4+ config files | 1 TOML file |
| rpcbind required (NFSv3) | NFSv4 only, no portmapper |
| `exportfs` CLI | REST API + CLI + Web UI |
| No metrics | Prometheus + per-export stats |
| No QoS | Per-export rate limiting |
| ~50 MB installed | ~9 MB static binary |
| C (kernel + userspace) | Rust (userspace only) |

## Integration Components

### 1. RPM Package

```
nextnfs-0.12.0-1.fc41.x86_64.rpm
nextnfs-0.12.0-1.el9.x86_64.rpm
nextnfs-0.12.0-1.el10.x86_64.rpm
```

**Contents:**

```
/usr/bin/nextnfs                           # static binary
/usr/lib/systemd/system/nextnfs.service    # systemd unit
/usr/lib/systemd/system/nextnfs.socket     # socket activation (optional)
/etc/nextnfs/config.toml                   # default config (marked %config(noreplace))
/usr/lib/firewalld/services/nextnfs.xml    # firewalld service definition
/usr/share/selinux/packages/nextnfs.pp     # SELinux policy module
/usr/share/nextnfs/cockpit/                # Cockpit plugin (web management)
/usr/share/man/man1/nextnfs.1.gz           # man page
/usr/share/man/man5/nextnfs.toml.5.gz      # config file man page
/usr/share/doc/nextnfs/                    # documentation
```

**Install experience:**

```bash
# Fedora
sudo dnf install nextnfs

# RHEL (EPEL or direct)
sudo dnf install nextnfs

# Enable and start
sudo systemctl enable --now nextnfs

# Add an export
sudo nextnfs export add --name shared --path /srv/shared
# or
sudo nextnfs export add --name shared --path /srv/shared --read-only

# Done. Clients can mount immediately.
```

### 2. systemd service

```ini
# /usr/lib/systemd/system/nextnfs.service
[Unit]
Description=NextNFS File Server
Documentation=man:nextnfs(1) https://nextnfs.io/docs
After=network-online.target local-fs.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=/usr/bin/nextnfs serve --config /etc/nextnfs/config.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/lib/nextnfs /srv
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDs=true
MemoryDenyWriteExecute=true

# Resource limits
LimitNOFILE=65536
LimitNPROC=4096

[Install]
WantedBy=multi-user.target
```

**Socket activation** (start on first NFS connection):

```ini
# /usr/lib/systemd/system/nextnfs.socket
[Unit]
Description=NextNFS Socket

[Socket]
ListenStream=2049

[Install]
WantedBy=sockets.target
```

### 3. firewalld service

```xml
<!-- /usr/lib/firewalld/services/nextnfs.xml -->
<?xml version="1.0" encoding="utf-8"?>
<service>
  <short>NextNFS</short>
  <description>
    NextNFS file server. NFSv4 on port 2049 and REST API on port 8080.
  </description>
  <port protocol="tcp" port="2049"/>
  <port protocol="tcp" port="8080"/>
</service>
```

```bash
# Enable through firewall
sudo firewall-cmd --permanent --add-service=nextnfs
sudo firewall-cmd --reload
```

No random ports, no portmapper, no `mountd` — just two TCP ports. NFSv4 eliminated the multi-port problem that made NFS firewalling painful.

### 4. SELinux policy module

```
# nextnfs.te — SELinux type enforcement

policy_module(nextnfs, 1.0.0)

# Define nextnfs types
type nextnfs_t;
type nextnfs_exec_t;
type nextnfs_var_lib_t;
type nextnfs_port_t;
type nextnfs_export_t;

# Transition: systemd starts nextnfs binary → runs as nextnfs_t
init_daemon_domain(nextnfs_t, nextnfs_exec_t)

# Allow nextnfs to bind NFS port and API port
allow nextnfs_t nextnfs_port_t:tcp_socket { name_bind };
corenet_tcp_bind_generic_port(nextnfs_t)

# Allow nextnfs to read/write export directories
allow nextnfs_t nextnfs_export_t:dir { read write create getattr setattr search add_name remove_name open };
allow nextnfs_t nextnfs_export_t:file { read write create getattr setattr unlink open rename };
allow nextnfs_t nextnfs_export_t:lnk_file { read create unlink };

# Allow nextnfs to manage its state directory
allow nextnfs_t nextnfs_var_lib_t:dir manage_dir_perms;
allow nextnfs_t nextnfs_var_lib_t:file manage_file_perms;

# Network access for registry pulls (layer extraction)
corenet_tcp_connect_http_port(nextnfs_t)
```

**User-facing:**

```bash
# Label export directories (done automatically by nextnfs export add)
sudo semanage fcontext -a -t nextnfs_export_t "/srv/shared(/.*)?"
sudo restorecon -Rv /srv/shared

# Or just use the CLI which handles it
sudo nextnfs export add --name shared --path /srv/shared
# → automatically runs semanage + restorecon
```

### 5. Cockpit plugin

Web-based management via Cockpit (standard on Fedora Server and RHEL):

```
┌─────────────────────────────────────────────┐
│  Cockpit                                    │
│  ┌───────────────────────────────────────┐  │
│  │ NextNFS                               │  │
│  │                                       │  │
│  │ Status: ● Running (12 clients)        │  │
│  │ Uptime: 4 days, 7 hours              │  │
│  │                                       │  │
│  │ Exports                               │  │
│  │ ┌─────────┬──────────┬────┬────────┐  │  │
│  │ │ Name    │ Path     │ RO │ Clients│  │  │
│  │ ├─────────┼──────────┼────┼────────┤  │  │
│  │ │ shared  │ /srv/sh… │ No │ 5      │  │  │
│  │ │ backups │ /backup  │ Yes│ 3      │  │  │
│  │ │ vm-base │ (overlay)│ —  │ 4      │  │  │
│  │ └─────────┴──────────┴────┴────────┘  │  │
│  │                                       │  │
│  │ [+ Add Export]  [Settings]            │  │
│  │                                       │  │
│  │ Throughput          Operations        │  │
│  │ ▁▃▅▇█▇▅▃▁ 45 MB/s  ▁▂▃▅▇ 1.2k/s   │  │
│  └───────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
```

The Cockpit plugin talks to NextNFS's existing REST API — no new server-side code needed. The plugin is a static JS/HTML bundle that ships with the RPM.

**Features:**
- View/add/remove exports (regular and overlay)
- Per-export stats and client list
- QoS configuration sliders
- Service start/stop/restart
- Log viewer with level filtering
- Layer cache management (for overlay exports)

### 6. Default configuration

```toml
# /etc/nextnfs/config.toml
# NextNFS default configuration for Fedora/RHEL

[server]
listen = "0.0.0.0:2049"
api_listen = "127.0.0.1:8080"    # REST API on localhost only by default
state_dir = "/var/lib/nextnfs"

# Exports are managed via CLI or REST API.
# They are persisted in state_dir and survive restarts.
# You can also define static exports here:
#
# [[exports]]
# name = "shared"
# path = "/srv/shared"
# read_only = false
# max_ops_per_sec = 0          # 0 = unlimited
# max_bytes_per_sec = 0        # 0 = unlimited
```

### 7. CLI experience

```bash
# Service management
sudo systemctl start nextnfs
sudo systemctl status nextnfs
sudo nextnfs health

# Export management
sudo nextnfs export add --name shared --path /srv/shared
sudo nextnfs export add --name photos --path /home/user/photos --read-only
sudo nextnfs export add --name vm-base --type overlay \
  --lower /var/lib/nextnfs/layers/sha256-aaa \
  --lower /var/lib/nextnfs/layers/sha256-bbb \
  --upper /var/lib/nextnfs/upper/vm-42
sudo nextnfs export list
sudo nextnfs export remove shared

# Stats
sudo nextnfs stats
sudo nextnfs stats shared

# QoS
sudo nextnfs qos set shared --max-ops 5000 --max-bytes 100MB
sudo nextnfs qos get shared

# Layer management (for overlay exports)
sudo nextnfs layer list
sudo nextnfs layer extract --registry registry.local:5000 --image alpine:3.19
sudo nextnfs layer gc --dry-run

# Client info
sudo nextnfs clients
```

### 8. Compatibility with /etc/exports (migration helper)

For users migrating from kernel NFS, provide an import tool:

```bash
# Import existing kernel NFS exports
sudo nextnfs migrate-from-exports /etc/exports

# Reads /etc/exports format:
#   /srv/shared  192.168.1.0/24(rw,sync,no_subtree_check)
#   /backup      *(ro,sync)
#
# Converts to NextNFS config:
#   [[exports]]
#   name = "srv-shared"
#   path = "/srv/shared"
#   read_only = false
#
#   [[exports]]
#   name = "backup"
#   path = "/backup"
#   read_only = true

# Stop kernel NFS, start NextNFS
sudo systemctl disable --now nfs-server
sudo systemctl enable --now nextnfs
```

### 9. COPR / Fedora package review

**Build targets:**

| Target | Package format | Repository |
|--------|---------------|-----------|
| Fedora 40, 41, 42 | RPM | COPR initially, Fedora repos goal |
| RHEL 9 | RPM | EPEL or direct |
| RHEL 10 | RPM | EPEL or direct |
| CentOS Stream 9, 10 | RPM | EPEL |

**COPR (fast path):**

```bash
# Create COPR repo
copr-cli create nextnfs --chroot fedora-41-x86_64 --chroot fedora-41-aarch64 \
  --chroot epel-9-x86_64 --chroot epel-9-aarch64

# Users install via:
sudo dnf copr enable nextnfs/nextnfs
sudo dnf install nextnfs
```

**Fedora package review (long-term goal):**
- Submit package review request to Fedora Bugzilla
- Meet Fedora packaging guidelines (license, naming, macros)
- Get sponsor, pass review, push to Fedora repos
- Becomes available via standard `dnf install nextnfs`

### 10. Podman storage driver (standalone, non-Kubernetes)

On a Fedora/RHEL workstation or server running Podman without Kubernetes:

```bash
# Install
sudo dnf install nextnfs nextnfs-podman-driver

# Configure Podman to use NextNFS for container storage
cat >> ~/.config/containers/storage.conf <<EOF
[storage]
driver = "nextnfs"

[storage.options.nextnfs]
nextnfs_api = "http://127.0.0.1:8080"
layers_dir = "/var/lib/nextnfs/layers"
upper_dir = "/var/lib/nextnfs/upper"
EOF

# Use Podman normally — now backed by NextNFS overlay
podman pull alpine:3.19
podman run -it alpine sh

# Builds no longer fail with EXDEV
podman build -t myapp .
```

Benefits for Fedora/RHEL desktop developers:
- No more `rpm --rebuilddb` failures in container builds
- No more fuse-overlayfs slowness in rootless Podman
- Layer sharing across podman and Kubernetes (same NextNFS instance)
- Web UI to inspect container layers and storage usage

### 11. autofs integration

For environments using autofs for on-demand NFS mounts:

```bash
# /etc/auto.nextnfs
shared    -fstype=nfs4,vers=4.0    nextnfs-server.local:/shared
backups   -fstype=nfs4,vers=4.0    nextnfs-server.local:/backups
```

```bash
# /etc/auto.master.d/nextnfs.autofs
/nfs    /etc/auto.nextnfs
```

Users access `/nfs/shared` and it auto-mounts from NextNFS. Standard autofs, nothing custom.

### 12. Samba gateway (future)

NextNFS exports accessible to Windows clients via Samba's VFS module system:

```
Windows client ──SMB──→ Samba ──VFS──→ NextNFS export path
```

This is a stretch goal — documented here for completeness. Samba can serve any local directory, so if NextNFS exports are mounted locally (or served as local overlay dirs), Samba can re-export them to Windows clients without any custom integration.

## Comparison: kernel nfsd vs NextNFS on Fedora

| Feature | kernel nfsd | NextNFS |
|---------|------------|---------|
| Install | `dnf install nfs-utils` (pulls 15 deps) | `dnf install nextnfs` (0 deps) |
| Configure | Edit `/etc/exports`, run `exportfs -a` | `nextnfs export add --name x --path /x` |
| Firewall | Open 2049, 111, random mountd/statd ports | Open 2049, 8080 |
| SELinux | Manual `setsebool`, `semanage`, `restorecon` | Automatic on `export add` |
| Web management | None (Cockpit has basic NFS but limited) | Built-in Web UI + Cockpit plugin |
| REST API | None | Full CRUD + stats + QoS |
| Monitoring | `nfsstat` (basic counters) | Prometheus metrics, per-export stats |
| QoS | None | Per-export ops/sec and bytes/sec limits |
| Overlay support | No | Yes (container/VM rootfs) |
| Protocol | NFSv3 + NFSv4 + NFSv4.1 | NFSv4.0 (NFSv4.1 planned) |
| Binary size | ~50 MB (6 daemons + libs) | ~9 MB (1 binary) |
| Dependencies | rpcbind, libtirpc, krb5-libs | None (static binary) |
| Recovery | Grace period (90 seconds default) | Near-zero grace period |

## Testing on Fedora/RHEL

### Package tests (RPM)

```bash
# Install
sudo dnf install ./nextnfs-0.12.0-1.fc41.x86_64.rpm

# Verify files installed
rpm -ql nextnfs

# Service starts
sudo systemctl start nextnfs
systemctl is-active nextnfs  # → active

# Firewall rule works
sudo firewall-cmd --add-service=nextnfs
sudo firewall-cmd --list-services | grep nextnfs

# SELinux contexts correct
ls -Z /usr/bin/nextnfs       # → system_u:object_r:nextnfs_exec_t:s0
ls -Z /var/lib/nextnfs       # → system_u:object_r:nextnfs_var_lib_t:s0

# Create export and mount from another machine
sudo nextnfs export add --name test --path /srv/test
mount -t nfs4 server:/test /mnt/test
echo "hello" > /mnt/test/hello.txt
cat /mnt/test/hello.txt  # → hello

# Cockpit plugin visible
# Navigate to https://server:9090 → NextNFS tab visible
```

### Upgrade tests

```bash
# Upgrade from previous version
sudo dnf upgrade ./nextnfs-0.13.0-1.fc41.x86_64.rpm

# Config preserved (%config(noreplace))
# Exports survive restart
# Clients reconnect automatically (grace period recovery)
```

### SELinux enforcing tests

```bash
# Verify works with SELinux enforcing (default on Fedora/RHEL)
getenforce  # → Enforcing

sudo nextnfs export add --name test --path /srv/test
# Mount and read/write from client
# Check no AVC denials:
sudo ausearch -m AVC -ts recent | grep nextnfs  # → (empty)
```

## Implementation phases

### Phase 1: RPM packaging
- RPM spec file for Fedora and RHEL
- systemd service and socket units
- Default config file
- Man pages
- COPR build and repository
- **Deliverable:** `nextnfs` RPM in COPR

### Phase 2: firewalld + SELinux
- firewalld service XML
- SELinux policy module (type enforcement, file contexts)
- Automatic SELinux labeling in `nextnfs export add`
- Test on Fedora and RHEL with enforcing SELinux
- **Deliverable:** SELinux policy in RPM, firewalld integration

### Phase 3: Cockpit plugin
- JavaScript/HTML Cockpit plugin
- Export management UI
- Stats and monitoring dashboard
- Ships in RPM under `/usr/share/cockpit/nextnfs/`
- **Deliverable:** Cockpit plugin in RPM

### Phase 4: Migration tooling
- `nextnfs migrate-from-exports` command
- Parse `/etc/exports` format
- Generate NextNFS config
- Documentation: "Migrating from kernel NFS to NextNFS"
- **Deliverable:** Migration guide and CLI command

### Phase 5: Podman storage driver (standalone)
- Go `containers/storage` driver for non-Kubernetes Podman
- Separate RPM: `nextnfs-podman-driver`
- Works with rootless Podman
- Test: `podman build` with rpm/apt/npm (EXDEV regression)
- **Deliverable:** `nextnfs-podman-driver` RPM

### Phase 6: Fedora packaging review
- Submit to Fedora package review
- Meet all packaging guidelines
- Find sponsor
- Push to Fedora and EPEL repos
- **Deliverable:** `nextnfs` in official Fedora/EPEL repositories
