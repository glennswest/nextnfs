# Enhancement: Overlay VFS for Container and VM Rootfs

**Status:** Proposed
**Date:** 2026-03-31
**Priority:** High

## Summary

Add an OverlayFS-like virtual filesystem backend to NextNFS that merges multiple read-only lower layer directories with a per-consumer writable upper directory. This enables NextNFS to serve container rootfs and VM root filesystems using OCI image layers — eliminating kernel overlayfs and its well-documented corruption issues.

## Motivation

### Kernel overlayfs is broken at scale

Production Kubernetes and OpenShift deployments suffer from overlayfs bugs that cannot be fixed without kernel changes:

- **EXDEV on directory rename** — `rename(2)` returns cross-device link error for directories on lower layers. Breaks `rpm`, `apt-get`, `npm install`. ([moby#25409](https://github.com/moby/moby/issues/25409), [moby#42055](https://github.com/moby/moby/issues/42055))
- **XFS + overlayfs silent corruption** — data loss after node restart, databases corrupted ([longhorn#3597](https://github.com/longhorn/longhorn/issues/3597), [longhorn#3895](https://github.com/longhorn/longhorn/issues/3895))
- **Copy-up race conditions** — concurrent processes trigger copy-up on the same file, kernel locking bugs cause corruption
- **fuse-overlayfs** — ghost directories, memory allocation failures, unusably slow with large layers ([fuse-overlayfs#189](https://github.com/containers/fuse-overlayfs/issues/189), [fuse-overlayfs#401](https://github.com/containers/fuse-overlayfs/issues/401))
- **Overlapped mount rejection** — newer kernels reject layer configurations that Docker push creates ([moby#39663](https://github.com/moby/moby/issues/39663))
- **Permission corruption** — overlay2 changes directory permissions on commit ([moby#27298](https://github.com/moby/moby/issues/27298))

### NextNFS can solve this in userspace

By implementing overlay merge logic in Rust inside the VFS layer, all kernel overlay bugs are eliminated. The NFS protocol layer is unchanged — clients see a normal NFS export. The overlay is transparent.

## Design

### New export type: `overlay`

Alongside existing `PhysicalFS` exports, add an `OverlayFS` VFS backend:

```toml
# Existing regular exports continue to work unchanged
[[exports]]
name = "shared-data"
path = "/data/shared"
read_only = false

# New overlay export
[[exports]]
name = "container-nginx-1"
type = "overlay"
lower = ["/layers/sha256-aaa", "/layers/sha256-bbb", "/layers/sha256-ccc"]
upper = "/upper/container-nginx-1"
```

Regular exports and overlay exports coexist in the same NextNFS process, same port, same REST API. No interference.

### OverlayFS VFS implementation

Implement the `vfs` crate trait (`VfsPath` / filesystem operations) with overlay semantics:

| Operation | Behavior |
|-----------|----------|
| **Read/Open** | Check upper first, walk lower stack top-down, return first match |
| **Write/Create** | Write directly to upper |
| **Modify existing** | Copy-up: copy file from lower to upper, then modify in upper |
| **Delete** | Create whiteout marker in upper (e.g., `.wh.<filename>`) |
| **Rename (file)** | Copy-up source if needed, rename within upper |
| **Rename (dir)** | Copy-up entire directory tree to upper, rename in upper (no EXDEV) |
| **Readdir** | Merge entries from all layers, filter whiteouts, deduplicate |
| **Stat/Getattr** | Check upper first, fall through to lower stack |
| **Hardlink** | Copy-up target if on lower, create link in upper |
| **Symlink** | Create directly in upper |

### Whiteout handling

Follow OCI image spec whiteout conventions:
- File deletion: create `.wh.<name>` in upper
- Directory deletion (opaque): create `.wh..wh..opq` in upper directory
- Readdir filters out whiteout markers and their targets from lower layers

### REST API extensions

```
POST   /api/v1/exports          — create overlay export (type: "overlay")
DELETE /api/v1/exports/{name}   — remove overlay export
GET    /api/v1/exports/{name}   — includes layer info, upper usage stats
```

Dynamic creation/removal of overlay exports enables orchestrators to manage container lifecycle via API.

### Layer management API

```
POST   /api/v1/layers/extract   — pull layer blob from registry, extract to /layers/
GET    /api/v1/layers            — list extracted layers with size, refcount
DELETE /api/v1/layers/{digest}   — remove layer (only if refcount == 0)
GET    /api/v1/layers/{digest}   — layer metadata, which exports reference it
```

## Use Cases

### 1. Container rootfs (MikroTik RouterOS)

RouterOS has no kernel overlayfs. NextNFS overlay provides layered container rootfs over NFS:

```
RouterOS Container ──NFS mount──→ NextNFS overlay export
                                  ├── lower: [base-alpine, +nginx, +app-config]
                                  └── upper: per-container writable layer
```

### 2. Container rootfs (Kubernetes / OpenShift)

Replace kernel overlayfs as the container storage backend via:
- **containerd snapshotter plugin** — `Prepare()` calls NextNFS REST API to create overlay export
- **CRI-O containers/storage driver** — `Get()` returns NFS mount to overlay export

Per-pod opt-in via `RuntimeClass`:

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: nextnfs
handler: nextnfs
---
apiVersion: v1
kind: Pod
spec:
  runtimeClassName: nextnfs     # opt-in, overlay2 remains default
  containers:
  - name: app
    image: nginx:latest
```

Runs alongside existing overlay2 — zero interference with pods that don't opt in.

### 3. VM rootfs (NFS root boot)

Linux VMs boot with `root=nfs:<nextnfs-ip>:/exports/vm-42`:

```
iPXE/PXE → kernel + initramfs → NFS root mount → VM running
```

Provisioning time: milliseconds (REST API call to create overlay export). No disk image copy. 20 GB base template shared across hundreds of VMs.

### 4. Instant VM/container cloning

Clone = snapshot upper dir + create new overlay with snapshotted upper as additional lower layer + fresh upper. Zero copy time.

### 5. Proxmox LXC

Proxmox LXC natively supports NFS rootfs. `pct create` with NFS-backed rootfs eliminates per-container tarball extraction.

## Architecture

### Per-node deployment (recommended)

Each compute node runs its own NextNFS instance. No shared storage dependency, no single point of failure:

```
Node 1                              Node 2 (standby for Node 1)
├── NextNFS (localhost:2049)        ├── NextNFS (localhost:2049)
│   ├── /layers/ (local SSD)       │   ├── /layers/ (replicated)
│   └── /upper/ (local SSD)        │   └── /upper/ (replicated)
├── containers mount localhost      ├── ready to activate
└── StormBlock (local drives)       └── StormBlock (replica PVCs)
```

- Container I/O is always local — no network dependency
- Layers sync between nodes via registry pull or peer-to-peer
- Standby node keeps layers extracted and upper dirs replicated for fast failover
- Node failure: standby activates exports, containers restart in seconds

### Layer sharing

Layers are content-addressable by SHA256 digest. Same digest = same directory on disk. 20 containers from the same base image share one extracted layer directory. Storage cost: 1x base + per-container upper diffs.

### Integration with OCI registries

NextNFS pulls layers directly from any OCI-compliant registry (standard HTTP):

1. `GET /v2/<name>/manifests/<tag>` → layer list and order
2. For each layer, check if `/layers/sha256-<digest>/` exists (cache hit → skip)
3. `GET /v2/<name>/blobs/sha256:<digest>` → decompress → extract into layer dir
4. Create overlay export with layers in manifest order + empty upper

No container runtime needed for image pulling. NextNFS handles it directly.

## Scalability

| Scale | Deployment | Notes |
|-------|-----------|-------|
| 1-200 containers | Single NextNFS per node | Comfortable headroom |
| 200-500 containers | Single NextNFS, tuned (filehandle cache, whiteout bitmap) | With caching |
| 500-2000 containers | Sharded NextNFS (5-10 instances per node) | Round-robin assignment |
| 2000+ | Sharded + dedicated instances for high-I/O tenants | Per-namespace sharding |

## Performance characteristics

| Aspect | Kernel overlayfs | NextNFS overlay |
|--------|-----------------|-----------------|
| Read (cached) | Kernel page cache | NFS client cache + local disk |
| Read (uncached) | Kernel VFS lookup | NFS round trip (localhost = sub-ms) |
| Write (new file) | Direct to upper | NFS → upper (localhost) |
| Write (copy-up) | Kernel copy, racy | Rust async copy, single writer, safe |
| Directory rename | EXDEV error | Full copy-up + rename, correct |
| Readdir (merged) | Kernel merge | Userspace merge, sorted, filtered |

For typical container workloads (config reads, log writes, small file I/O), the localhost NFS hop is negligible. Heavy I/O workloads (databases) should use direct PVCs regardless.

## Implementation plan

### Phase 1: OverlayFS VFS backend
- Implement `OverlayVfs` struct implementing `vfs` filesystem trait
- Read path: upper-first lookup, lower stack fallback
- Write path: direct-to-upper for new files, copy-up for modifications
- Delete path: whiteout markers (`.wh.<name>`, `.wh..wh..opq`)
- Readdir: merge all layers, filter whiteouts, deduplicate
- Directory rename: full tree copy-up (no EXDEV)
- Unit tests for all operations

### Phase 2: Configuration and REST API
- TOML config: `type = "overlay"`, `lower = [...]`, `upper = "..."`
- REST API: create/delete overlay exports dynamically
- Backwards compatible — existing exports unchanged
- Per-export stats include layer count, upper usage, copy-up count

### Phase 3: Layer management
- Layer extraction from OCI registry blobs (HTTP pull, gunzip, untar)
- Content-addressable layer cache (`/layers/sha256-<digest>/`)
- Reference counting and garbage collection
- REST API for layer lifecycle
- Peer-to-peer layer sync between nodes

### Phase 4: Kubernetes integration
- containerd snapshotter plugin (Go binary, gRPC)
- CRI-O containers/storage driver (Go library)
- Helm charts for DaemonSet deployment
- RuntimeClass for per-pod opt-in
- Documentation and migration guide

## Dependencies

- `vfs` crate — already used by NextNFS, overlay backend implements same trait
- `reqwest` or `ureq` — for OCI registry HTTP pulls (layer extraction)
- `flate2` — gzip decompression for layer blobs
- `tar` — tar extraction for layer blobs

No new external service dependencies. NextNFS remains a single static binary.
