# Enhancement: StormFS — Distributed POSIX Filesystem Backend

**Status:** Proposed
**Date:** 2026-04-27
**Priority:** High
**Depends on:** StormBlock cluster (external crate)

## Summary

Add an optional distributed POSIX filesystem backend to NextNFS, backed by StormBlock's clustered block storage. StormFS replaces the standalone overlay.rs approach for deployments that need proper POSIX metadata, multi-node consistency, and horizontal scale. It is compiled conditionally via `--features stormfs` — without the feature flag, NextNFS compiles and runs exactly as it does today with zero StormBlock code.

StormFS provides:

- **Full POSIX metadata** — uid, gid, mode, timestamps, xattrs, hardlinks, symlinks, device nodes
- **Distributed consistency** — Raft-replicated metadata, quorum reads/writes across 3-5 nodes
- **COW container lifecycle** — create a container rootfs in microseconds via metadata snapshot + chunk refcount bump
- **Scale to 1000+ nodes** — metadata on Raft voters, data chunks on all StormBlock nodes, NFS served locally
- **No external dependencies** — no etcd, no TiKV, no Redis, no ZooKeeper. StormBlock's built-in Raft is the only consensus layer

```
                                    NextNFS (standalone)
                                    ├── PhysicalFS exports     ← unchanged
                                    ├── OverlayFS exports      ← unchanged (overlay.rs)
                                    ├── MemoryFS (tests)       ← unchanged
                                    ├── OCI registry           ← unchanged
                                    └── REST API               ← unchanged

                                    NextNFS --features stormfs
                                    ├── PhysicalFS exports     ← unchanged
                                    ├── OverlayFS exports      ← unchanged (overlay.rs)
                                    ├── MemoryFS (tests)       ← unchanged
                                    ├── OCI registry           ← unchanged
                                    ├── REST API               ← extended with StormFS endpoints
                                    └── StormFS exports        ← NEW
                                        └── links stormblock crate
```

## Motivation

### overlay.rs has fundamental limitations

The current overlay.rs implementation works well for single-node container rootfs serving but cannot scale to distributed deployments:

| Limitation | Impact |
|-----------|--------|
| **No POSIX metadata** | `vfs` crate only exposes `file_type` + `len`. No uid/gid, no mode bits, no timestamps, no xattrs. NFS GETATTR returns synthetic values. |
| **File-level copy-up** | Modifying a 2 GB file in a lower layer reads the entire file into memory, copies to upper, then applies the change. OOM on large files. |
| **No concurrency control** | Two NFS clients writing to the same overlay export can corrupt whiteout state. No locking between copy-up operations. |
| **O(depth x layers) path resolution** | Every path lookup walks all layers top-down checking for whiteouts. 20-layer images with deep directory trees are slow. |
| **Single-node only** | Each overlay export lives on one machine's local disk. No replication, no failover, no shared state between nodes. |
| **No hardlinks** | `vfs` crate has no hardlink support. Containers that use hardlinks (RPM database, Git packfiles) silently break. |

These are not bugs — they are architectural constraints of building on the `vfs` crate with local filesystem backing. Fixing them requires a different foundation.

### What StormFS provides

StormFS is a clean filesystem implementation (inspired by JuiceFS's architecture, not its code) built directly on StormBlock's clustered block storage:

- **Inode table with full POSIX fields** — replicated via Raft, queryable by inode number
- **B-tree directories** — O(log n) lookup, efficient range scans for READDIR, atomic rename
- **4 MB chunk storage in StormBlock slabs** — aligned to slab slot size, COW via GEM refcounts
- **COW snapshots** — container creation clones metadata + bumps chunk refcounts, no data copy
- **Multi-node** — any NextNFS instance can serve any StormFS volume, metadata is Raft-consistent

### Target deployments

| Deployment | How it works |
|-----------|-------------|
| **1000+ OpenShift nodes** | 3-5 StormBlock Raft voters hold metadata. All nodes serve NFS locally. Chunks distributed across all StormBlock storage nodes. |
| **MikroTik containers** | RouterOS mounts `nextnfs:/container-nginx-1` via NFS. Golden image is a StormFS volume. Per-container instance is a COW snapshot. |
| **CI/CD build farms** | Each build job gets a COW snapshot of the base toolchain volume. Build artifacts written to the snapshot. Job ends, snapshot deleted, chunks freed. |
| **Dev environments** | Developer gets a COW snapshot of the production database volume. Full copy semantics, zero copy time, independent writes. |

### Why not JuiceFS directly?

JuiceFS requires Redis/TiKV/etcd for metadata and S3/MinIO for data. That's 3 separate distributed systems to operate. StormFS uses StormBlock for both metadata (Raft state machine) and data (slab storage), eliminating all external dependencies. The entire stack is two Rust binaries: `stormblock` and `nextnfs`.

## Architecture

### High-level data flow

```
NFS Clients (mount -t nfs4)
        │
        ▼
NextNFS (NFSv4 protocol layer — unchanged)
        │
        ▼
StormFsBackend (new — implements FileManager dispatch)
        │
        ├── Metadata operations (lookup, create, setattr, readdir, rename, link, ...)
        │   │
        │   ▼
        │   StormFS Client Library (in-process, async Rust)
        │   │
        │   ▼
        │   StormBlock Raft State Machine (extends ClusterCommand)
        │   ├── InodeTable: HashMap<u64, Inode>
        │   ├── DirEntries: BTreeMap<(u64, String), u64>  (parent_ino, name) → child_ino
        │   └── SymlinkTargets: HashMap<u64, String>
        │
        └── Data operations (read, write)
            │
            ▼
            StormFS Client Library
            │
            ▼
            StormBlock BlockDevice trait (async read/write)
            ├── ThinVolumeHandle (COW snapshots)
            ├── GEM (Global Extent Map — chunk→slab mapping)
            ├── Slab storage (4 MB slots, RAID-backed)
            └── Placement engine (replication, cold copies)
```

### Metadata engine

All filesystem metadata lives in StormBlock's Raft state machine as new `ClusterCommand` variants. This gives linearizable consistency across all nodes with no additional consensus layer.

#### Inode structure

```rust
/// Full POSIX inode — replicated via Raft.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inode {
    pub ino: u64,
    pub file_type: FileType,
    pub mode: u32,          // permission bits (0o755, etc.)
    pub nlink: u32,         // hardlink count
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blocks: u64,        // 512-byte blocks allocated
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub xattrs: BTreeMap<String, Vec<u8>>,
    /// Chunk map: file offset (chunk index) → StormBlock extent ID.
    /// Only for regular files. Each chunk is 4 MB.
    pub chunks: BTreeMap<u64, ChunkRef>,
    /// Symlink target (only for Nf4lnk).
    pub symlink_target: Option<String>,
    /// Device major/minor (only for Nf4blk/Nf4chr).
    pub rdev: Option<(u32, u32)>,
    /// Volume ID this inode belongs to.
    pub volume_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkRef {
    /// StormBlock extent ID (maps to a slab slot via GEM).
    pub extent_id: u64,
    /// Offset within the extent (for sub-chunk packing).
    pub offset: u32,
    /// Length of data in this chunk (≤ CHUNK_SIZE).
    pub length: u32,
    /// Reference count for COW. When a snapshot is created,
    /// refcount is bumped. Writes to a shared chunk trigger
    /// copy-on-write (allocate new extent, copy, update mapping).
    pub refcount: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    BlockDevice,
    CharDevice,
    Fifo,
    Socket,
}

/// Nanosecond-precision timestamp.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: u32,
}
```

#### Raft commands for metadata

StormBlock's existing `ClusterCommand` enum is extended with filesystem operations:

```rust
/// New ClusterCommand variants for StormFS metadata.
enum ClusterCommand {
    // ... existing StormBlock commands ...

    // Inode operations
    FsCreateInode { volume_id: u64, inode: Inode },
    FsDeleteInode { volume_id: u64, ino: u64 },
    FsSetAttr { volume_id: u64, ino: u64, attrs: SetAttrArgs },
    FsUpdateChunkMap { volume_id: u64, ino: u64, chunk_idx: u64, chunk_ref: ChunkRef },
    FsSetXattr { volume_id: u64, ino: u64, name: String, value: Vec<u8> },
    FsRemoveXattr { volume_id: u64, ino: u64, name: String },

    // Directory operations
    FsDirLink { volume_id: u64, parent_ino: u64, name: String, child_ino: u64 },
    FsDirUnlink { volume_id: u64, parent_ino: u64, name: String },
    FsDirRename {
        volume_id: u64,
        src_parent: u64, src_name: String,
        dst_parent: u64, dst_name: String,
        /// If set, the destination entry is replaced (POSIX rename semantics).
        replace_ino: Option<u64>,
    },

    // Volume operations
    FsCreateVolume { volume_id: u64, name: String, root_inode: Inode },
    FsDeleteVolume { volume_id: u64 },
    FsSnapshotVolume { src_volume_id: u64, dst_volume_id: u64, dst_name: String },
}
```

Each command is applied atomically to the Raft state machine. Directory rename is a single Raft log entry — no two-phase commit, no intermediate states visible to other nodes.

#### Directory B-tree

Directories are stored as a B-tree indexed by `(parent_ino, name)`:

```
DirEntries BTreeMap:
  (2, "bin")     → 100
  (2, "etc")     → 101
  (2, "home")    → 102
  (2, "usr")     → 103
  (100, "sh")    → 200
  (100, "ls")    → 201
  (101, "passwd") → 300
  (101, "group")  → 301
```

Operations:
- **LOOKUP**: `dir_entries.get(&(parent_ino, name))` → O(log n)
- **READDIR**: `dir_entries.range((parent_ino, "")..(parent_ino + 1, ""))` → O(log n + k) where k = entries returned
- **CREATE**: Insert `(parent_ino, name) → new_ino` + create inode → single Raft command
- **REMOVE**: Delete `(parent_ino, name)` + decrement nlink → single Raft command
- **RENAME**: Atomic swap in B-tree → single Raft command (no EXDEV, works across directories)
- **LINK**: Insert `(parent_ino, name) → existing_ino` + increment nlink → single Raft command

### Data engine

File data is stored as 4 MB chunks in StormBlock slabs. The chunk size matches StormBlock's `slot_size` (4 MB per slab slot), so each chunk occupies exactly one slab slot with no internal fragmentation.

#### Chunk layout

```
File (14 MB):
  Chunk 0: bytes [0, 4MB)       → extent 1001 (slab 5, slot 12)
  Chunk 1: bytes [4MB, 8MB)     → extent 1002 (slab 5, slot 13)
  Chunk 2: bytes [8MB, 12MB)    → extent 1003 (slab 7, slot 0)
  Chunk 3: bytes [12MB, 14MB)   → extent 1004 (slab 7, slot 1)  [2 MB, partial]

Inode.chunks BTreeMap:
  0 → ChunkRef { extent_id: 1001, offset: 0, length: 4194304, refcount: 1 }
  1 → ChunkRef { extent_id: 1002, offset: 0, length: 4194304, refcount: 1 }
  2 → ChunkRef { extent_id: 1003, offset: 0, length: 4194304, refcount: 1 }
  3 → ChunkRef { extent_id: 1004, offset: 0, length: 2097152, refcount: 1 }
```

#### Read path

```
NFS READ(fh, offset=5242880, count=65536)
  │
  ▼
StormFsBackend:
  1. Resolve fh → inode 500
  2. chunk_idx = offset / CHUNK_SIZE = 1
  3. chunk_offset = offset % CHUNK_SIZE = 1048576
  4. Look up inode.chunks[1] → ChunkRef { extent_id: 1002, ... }
  5. StormBlock::read(extent_id=1002, offset=1048576, len=65536)
     → async BlockDevice::read() → returns 65536 bytes
  6. Return data to NFS layer
```

No full-file read. No copy-up. Direct chunk access at the exact offset.

#### Write path

```
NFS WRITE(fh, offset=5242880, data=[65536 bytes])
  │
  ▼
StormFsBackend:
  1. Resolve fh → inode 500
  2. chunk_idx = offset / CHUNK_SIZE = 1
  3. chunk_offset = offset % CHUNK_SIZE = 1048576
  4. Look up inode.chunks[1] → ChunkRef { extent_id: 1002, refcount: 1 }
  5. refcount == 1 → write in place (no COW needed)
     StormBlock::write(extent_id=1002, offset=1048576, data)
  6. Update inode.size if offset + len > current size
  7. Update inode.mtime
  8. Raft commit: FsSetAttr { size, mtime } (metadata only — data already written)
```

#### Copy-on-write path (shared chunk)

When a chunk has `refcount > 1` (shared between a golden image and a container snapshot):

```
NFS WRITE(fh, offset=0, data=[4096 bytes])
  │
  ▼
StormFsBackend:
  1. Resolve fh → inode 500 (in snapshot volume)
  2. chunk_idx = 0
  3. Look up inode.chunks[0] → ChunkRef { extent_id: 1001, refcount: 3 }
  4. refcount > 1 → COW required
     a. Allocate new extent via StormBlock → extent_id 2001
     b. Copy 4 MB from extent 1001 to extent 2001
     c. Apply write: overwrite first 4096 bytes in extent 2001
     d. Raft commit: FsUpdateChunkMap {
          ino: 500, chunk_idx: 0,
          chunk_ref: ChunkRef { extent_id: 2001, refcount: 1 }
        }
     e. Decrement refcount on extent 1001 (via GEM)
        → 1001 refcount drops from 3 to 2 (still shared by 2 other snapshots)
```

The 4 MB COW granularity means modifying one byte copies one 4 MB chunk — not the entire file. A 2 GB file modified at offset 0 copies 4 MB, not 2 GB.

### Volume and snapshot model

A **StormFS volume** is a complete filesystem namespace: one root inode, one inode table, one directory tree. Each volume has a unique `volume_id` that scopes all metadata operations.

```
Volume "golden-nginx" (volume_id: 100)
  ├── / (ino 2)
  │   ├── bin/ (ino 100)
  │   ├── etc/ (ino 101)
  │   │   └── nginx/ (ino 200)
  │   │       └── nginx.conf (ino 300) → chunk extents [5001, 5002]
  │   └── usr/ (ino 102)
  │       └── sbin/
  │           └── nginx (ino 400) → chunk extents [6001..6010]
  └── Inode table: { 2, 100, 101, 102, 200, 300, 400, ... }
```

#### COW snapshot (container creation)

Creating a container instance from a golden image is a metadata-only operation:

```
POST /api/v1/stormfs/volumes/golden-nginx/snapshot
  { "name": "container-nginx-42" }

Raft command: FsSnapshotVolume {
  src_volume_id: 100,
  dst_volume_id: 142,
  dst_name: "container-nginx-42"
}

State machine applies:
  1. Deep-copy the inode table: volume 142 gets its own copy of all inodes
  2. Deep-copy the directory B-tree entries scoped to volume 100 → volume 142
  3. For every ChunkRef in every inode: bump refcount += 1
     (no data copied — just refcount increments on GEM extents)
  4. Create root inode for volume 142
  5. Volume 142 is immediately usable as an NFS export

Time: O(inodes) metadata operations, zero data I/O
  100K inodes × 200 bytes/inode ≈ 20 MB of Raft state — applied in milliseconds
```

#### Container deletion

```
DELETE /api/v1/stormfs/volumes/container-nginx-42

Raft command: FsDeleteVolume { volume_id: 142 }

State machine applies:
  1. For every inode in volume 142:
     For every ChunkRef:
       Decrement refcount on GEM extent
       If refcount reaches 0 → slab slot freed (data reclaimed)
  2. Delete all directory entries for volume 142
  3. Delete all inodes for volume 142

Chunks shared with golden image (refcount was 2+) → refcount decremented, not freed.
Chunks unique to this container (refcount was 1) → freed immediately.
```

No garbage collection sweep needed. Refcount-based cleanup is immediate and precise.

### NextNFS integration

StormFS integrates into NextNFS at the FileManager level, alongside existing PhysicalFS and OverlayFS backends.

#### Export configuration

```toml
# Existing exports — completely unchanged
[[exports]]
name = "shared-data"
path = "/data/shared"
read_only = false

# Existing overlay export — completely unchanged
[[exports]]
name = "container-legacy-1"
type = "overlay"
lower = ["/layers/sha256-aaa", "/layers/sha256-bbb"]
upper = "/upper/container-legacy-1"

# NEW: StormFS export (only available with --features stormfs)
[[exports]]
name = "container-nginx-42"
type = "stormfs"
volume_id = 142
stormblock_cluster = "192.168.200.10:9300,192.168.200.11:9300,192.168.200.12:9300"
```

#### StormFsBackend

A new backend module that translates NFS operations into StormFS metadata + data operations:

```rust
/// StormFS backend for NextNFS — one per StormFS export.
///
/// Translates NFS4 operations into StormFS metadata (Raft) and
/// data (BlockDevice) operations.
pub struct StormFsBackend {
    /// StormFS client library handle (shared across all exports on this node).
    client: Arc<StormFsClient>,
    /// Volume ID for this export.
    volume_id: u64,
    /// Local chunk cache (LRU, configurable size).
    chunk_cache: Arc<ChunkCache>,
}

impl StormFsBackend {
    /// LOOKUP: resolve name in directory.
    pub async fn lookup(&self, parent_ino: u64, name: &str) -> Result<Inode> {
        self.client.dir_lookup(self.volume_id, parent_ino, name).await
    }

    /// GETATTR: return full POSIX metadata from inode table.
    pub async fn getattr(&self, ino: u64) -> Result<Inode> {
        self.client.inode_get(self.volume_id, ino).await
    }

    /// READ: read data from chunk storage.
    pub async fn read(&self, ino: u64, offset: u64, count: u32) -> Result<Vec<u8>> {
        let inode = self.client.inode_get(self.volume_id, ino).await?;
        self.read_chunks(&inode, offset, count).await
    }

    /// WRITE: write data to chunk storage (COW if shared).
    pub async fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let inode = self.client.inode_get(self.volume_id, ino).await?;
        self.write_chunks(&inode, offset, data).await
    }

    /// READDIR: scan directory B-tree range.
    pub async fn readdir(&self, dir_ino: u64, cookie: u64, count: u32)
        -> Result<Vec<DirEntry>>
    {
        self.client.dir_list(self.volume_id, dir_ino, cookie, count).await
    }

    /// CREATE: allocate inode + link into directory (single Raft command).
    pub async fn create(&self, parent_ino: u64, name: &str, attrs: &CreateAttrs)
        -> Result<Inode>
    {
        self.client.create(self.volume_id, parent_ino, name, attrs).await
    }

    /// RENAME: atomic directory entry swap (single Raft command).
    pub async fn rename(
        &self,
        src_parent: u64, src_name: &str,
        dst_parent: u64, dst_name: &str,
    ) -> Result<()> {
        self.client.rename(
            self.volume_id,
            src_parent, src_name,
            dst_parent, dst_name,
        ).await
    }
}
```

#### FileManager dual-mode dispatch

The FileManager actor dispatches to the appropriate backend based on export type:

```rust
// In filemanager/mod.rs — conceptual dispatch
match export.backend {
    ExportBackend::Physical(ref path) => {
        // Existing PhysicalFS path — unchanged
        self.handle_physical(request, path).await
    }
    ExportBackend::Overlay(ref overlay) => {
        // Existing OverlayFS path — unchanged
        self.handle_overlay(request, overlay).await
    }
    #[cfg(feature = "stormfs")]
    ExportBackend::StormFs(ref backend) => {
        // NEW: StormFS path
        self.handle_stormfs(request, backend).await
    }
}
```

#### Filehandle mapping

NFS filehandles (`NfsFh4 = [u8; 26]`) encode the volume ID and inode number:

```
StormFS filehandle layout (26 bytes):
  [0..2]   — export index (u16, same as existing)
  [2..4]   — flags (u16: 0x0002 = StormFS)
  [4..12]  — volume_id (u64)
  [12..20] — inode number (u64)
  [20..26] — generation counter (u48, for stale handle detection)
```

This is a deterministic mapping — the same inode always produces the same filehandle. No fhdb cache needed for StormFS exports. Filehandle resolution is a single inode table lookup.

### Chunk cache

Each NextNFS node maintains a local LRU chunk cache to avoid re-reading hot chunks from StormBlock:

```toml
[stormfs.cache]
# Maximum cache size in memory (default 1 GB)
memory_size = "1G"
# Optional on-disk L2 cache (for larger working sets)
disk_path = "/var/cache/nextnfs/chunks"
disk_size = "50G"
```

Cache behavior:
- **Read hit**: return from cache, no StormBlock I/O
- **Read miss**: fetch from StormBlock, insert into cache, return
- **Write**: write-through to StormBlock, update cache entry
- **COW write**: invalidate old cache entry, write new chunk, cache new entry
- **Eviction**: LRU, background eviction when cache reaches 90% capacity

For container workloads, the cache hit rate is extremely high — containers read the same base image files repeatedly (libc, python, node, etc.), and those chunks stay hot in cache across all container instances sharing the golden image.

### Multi-node topology

```
                    StormBlock Cluster
                    ┌─────────────────────────────────────┐
                    │  Raft Voters (3 or 5 nodes)         │
                    │  ├── Node A: metadata + data slabs  │
                    │  ├── Node B: metadata + data slabs  │
                    │  └── Node C: metadata + data slabs  │
                    │                                     │
                    │  Storage Nodes (N nodes)             │
                    │  ├── Node D: data slabs only        │
                    │  ├── Node E: data slabs only        │
                    │  └── ...                            │
                    └─────────────────────────────────────┘
                         ▲           ▲           ▲
                         │           │           │
              ┌──────────┤     ┌─────┤     ┌─────┤
              ▼           ▼     ▼     ▼     ▼     ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐
        │ NextNFS  │ │ NextNFS  │ │ NextNFS  │ │ NextNFS  │
        │ Node 1   │ │ Node 2   │ │ Node 3   │ │ Node N   │
        │          │ │          │ │          │ │          │
        │ :2049 NFS│ │ :2049    │ │ :2049    │ │ :2049    │
        │ StormFS  │ │ StormFS  │ │ StormFS  │ │ StormFS  │
        │ client   │ │ client   │ │ client   │ │ client   │
        │ + cache  │ │ + cache  │ │ + cache  │ │ + cache  │
        └──────────┘ └──────────┘ └──────────┘ └──────────┘
              ▲           ▲           ▲           ▲
              │           │           │           │
         NFS clients  NFS clients  NFS clients  NFS clients
         (localhost)  (localhost)  (localhost)  (localhost)
```

Each NextNFS node:
1. Connects to the StormBlock cluster as a client
2. Sends metadata operations to Raft leader (linearizable)
3. Reads/writes chunk data directly from/to the StormBlock node that owns the slab
4. Caches hot chunks locally
5. Serves NFS to local containers on localhost:2049

NFS traffic is always localhost. StormBlock traffic is intra-cluster. The NFS client never crosses the network — only the StormFS client library does.

## REST API extensions

### Volume lifecycle

```bash
# Create a new empty StormFS volume
POST /api/v1/stormfs/volumes
{
  "name": "golden-nginx",
  "quota_bytes": 10737418240     # optional: 10 GB quota
}
# → { "volume_id": 100, "name": "golden-nginx", "created": "2026-04-27T..." }

# List all StormFS volumes
GET /api/v1/stormfs/volumes
# → [{ "volume_id": 100, "name": "golden-nginx", "size": 157286400, ... }, ...]

# Get volume details
GET /api/v1/stormfs/volumes/golden-nginx
# → { "volume_id": 100, "inodes_used": 12847, "bytes_used": 157286400,
#     "chunks_total": 38, "chunks_shared": 0, "snapshots": [] }

# Create COW snapshot (container instance)
POST /api/v1/stormfs/volumes/golden-nginx/snapshot
{
  "name": "container-nginx-42"
}
# → { "volume_id": 142, "name": "container-nginx-42",
#     "parent": "golden-nginx", "created": "2026-04-27T..." }

# Delete volume (frees unshared chunks)
DELETE /api/v1/stormfs/volumes/container-nginx-42
# → 204 No Content

# Create NFS export for a StormFS volume
POST /api/v1/exports
{
  "name": "container-nginx-42",
  "type": "stormfs",
  "volume_id": 142
}
# → { "name": "container-nginx-42", "type": "stormfs", "path": "/container-nginx-42" }
```

### Volume stats

```bash
GET /api/v1/stormfs/volumes/golden-nginx/stats
{
  "inodes_used": 12847,
  "inodes_free": 4294954449,    # 2^32 - used
  "bytes_used": 157286400,
  "bytes_quota": 10737418240,
  "chunks_total": 38,
  "chunks_unique": 38,          # chunks with refcount == 1
  "chunks_shared": 0,           # chunks with refcount > 1
  "snapshots": [
    { "volume_id": 142, "name": "container-nginx-42", "bytes_unique": 8192 },
    { "volume_id": 143, "name": "container-nginx-43", "bytes_unique": 0 }
  ]
}
```

### Cluster health

```bash
GET /api/v1/stormfs/health
{
  "cluster_state": "healthy",
  "raft_leader": "192.168.200.10:9300",
  "raft_voters": 3,
  "total_volumes": 47,
  "total_inodes": 582341,
  "total_chunks": 18472,
  "cache_hit_rate": 0.94,
  "cache_size_bytes": 1073741824
}
```

## OCI registry integration

When the OCI registry receives an image push, StormFS mode flattens the layers directly into a StormFS volume instead of extracting to directories:

```
Standalone mode (existing):
  podman push myapp:latest → registry receives layers
    → extract each layer to /layers/sha256-xxx/ (directories on local disk)
    → create overlay export with lower=[layer dirs] + upper

StormFS mode (new):
  podman push myapp:latest → registry receives layers
    → create StormFS volume "golden-myapp"
    → stream each layer's tar entries directly into StormFS inodes + chunks
       (no intermediate directory extraction, no local disk for layers)
    → volume is ready for COW snapshots

  Container start:
    → POST /api/v1/stormfs/volumes/golden-myapp/snapshot
       { "name": "container-myapp-1" }
    → POST /api/v1/exports
       { "name": "container-myapp-1", "type": "stormfs", "volume_id": ... }
    → mount -t nfs4 localhost:/container-myapp-1 /rootfs
```

### Layer deduplication

StormFS volumes support content-addressable chunk dedup:

1. Each 4 MB chunk is hashed (BLAKE3)
2. Before writing, check if an identical chunk already exists in the GEM
3. If match found: bump refcount, point inode chunk map to existing extent
4. If no match: write new chunk, insert into GEM

Two images sharing `python:3.12` as a base layer will share all identical chunks at the block level — even if they were pushed independently, even if the layer digests differ (e.g., different compression). This is more granular than OCI layer-level dedup.

## Comparison with overlay.rs

| Aspect | overlay.rs | StormFS |
|--------|-----------|---------|
| **Metadata** | `vfs` crate (file_type + len only) | Full POSIX (uid, gid, mode, timestamps, xattrs, nlink) |
| **Write granularity** | Full file copy-up | 4 MB chunk COW |
| **Concurrency** | No locking | Raft linearizability |
| **Path resolution** | O(depth x layers) | O(log n) B-tree lookup |
| **Hardlinks** | Not supported | Full support (nlink tracking) |
| **Rename** | Copy-up entire tree | Atomic B-tree swap |
| **Scale** | Single node | 1000+ nodes |
| **Container creation** | Extract layers + create dirs | COW snapshot (microseconds) |
| **Container deletion** | rm -rf upper dir | Refcount decrement (instant) |
| **Dependencies** | None (vfs crate) | StormBlock cluster |
| **Deployment** | Single binary | nextnfs + stormblock cluster |
| **Use case** | Standalone, MikroTik, small clusters | Large clusters, OpenShift, CI/CD farms |

**overlay.rs is not deprecated.** It remains the right choice for standalone NextNFS deployments, MikroTik containers, and environments without StormBlock. StormFS is for deployments that need distributed scale and full POSIX semantics.

## Performance characteristics

### Latency (expected)

| Operation | overlay.rs (local SSD) | StormFS (cached) | StormFS (uncached) |
|-----------|----------------------|-------------------|---------------------|
| LOOKUP | ~5 us (stat syscall) | ~2 us (local B-tree cache) | ~200 us (Raft read) |
| GETATTR | ~5 us (stat syscall) | ~1 us (inode cache) | ~200 us (Raft read) |
| READ 4K | ~10 us | ~5 us (chunk cache hit) | ~300 us (StormBlock read) |
| WRITE 4K | ~15 us | ~500 us (Raft commit + data write) | same |
| CREATE | ~20 us | ~500 us (Raft commit) | same |
| READDIR 100 entries | ~50 us | ~10 us (B-tree range scan, cached) | ~300 us |
| Container create | ~2s (layer extract) | ~10 ms (COW snapshot) | same |
| Container delete | ~500 ms (rm -rf) | ~5 ms (refcount decrement) | same |

Writes are slower due to Raft consensus (majority quorum commit). Reads are competitive when cached, which is the common case for container workloads (repeated reads of base image files).

### Throughput

| Workload | StormFS target |
|----------|---------------|
| Sequential read (single file) | StormBlock slab throughput (NVMe line rate) |
| Sequential write (single file) | ~500 MB/s (Raft commit + slab write, pipelined) |
| Random 4K read (cached) | ~500K IOPS per NextNFS node (memory cache) |
| Random 4K write | ~10K IOPS per NextNFS node (Raft-limited) |
| Container create rate | ~100/sec (metadata-only, Raft batching) |
| READDIR (large dir, 10K entries) | ~2 ms (B-tree range scan) |

Write IOPS are Raft-limited. For write-heavy workloads (databases), use PhysicalFS exports with direct StormBlock volumes — StormFS adds metadata overhead that databases don't need.

## Configuration reference

### Cargo feature

```toml
# In nextnfs/Cargo.toml
[features]
default = []
stormfs = ["dep:stormblock"]

[dependencies]
stormblock = { path = "../stormblock", optional = true }
```

### Runtime configuration

```toml
# nextnfs config.toml

[stormfs]
# StormBlock cluster connection
cluster = ["192.168.200.10:9300", "192.168.200.11:9300", "192.168.200.12:9300"]

# Chunk size (must match StormBlock slab slot_size)
chunk_size = "4M"

# Local chunk cache
[stormfs.cache]
memory_size = "1G"          # L1 in-memory LRU cache
disk_path = "/var/cache/nextnfs/chunks"
disk_size = "50G"           # L2 on-disk cache (optional)

# Metadata cache (inode + directory entries)
[stormfs.metadata_cache]
max_entries = 1000000       # ~200 MB at 200 bytes/inode
ttl_seconds = 30            # TTL for cached metadata (0 = no caching)

# OCI integration
[stormfs.oci]
# When true, image pushes create StormFS volumes instead of directory extractions
enabled = true
# Chunk-level dedup via BLAKE3 hashing
dedup = true
```

## Implementation phases

### Phase 1: Metadata engine (StormBlock side)

Extend StormBlock's Raft state machine with filesystem metadata operations.

**New files in StormBlock:**
- `src/stormfs/inode.rs` — Inode struct, FileType, Timespec, ChunkRef
- `src/stormfs/dir.rs` — Directory B-tree operations (link, unlink, rename, list)
- `src/stormfs/volume.rs` — Volume create/delete/snapshot, inode allocator
- `src/stormfs/mod.rs` — Module root, StormFs state machine integration

**Modified files in StormBlock:**
- `src/cluster/raft/state.rs` — Add `Fs*` variants to ClusterCommand enum
- `src/cluster/raft/state.rs` — Apply handler for each Fs command

**Scope:** ~3000 LOC Rust

**Deliverable:** `cargo test` passes with inode CRUD, directory operations, volume snapshot/delete all exercised through Raft command application. No NFS integration yet — pure metadata engine tests.

**Tests (100+):**
- Inode create/get/update/delete
- Directory link/unlink/lookup/list
- Rename within directory, across directories
- Rename replacing existing entry (POSIX overwrite semantics)
- Hardlink create, nlink increment/decrement
- Symlink create with target
- Volume create with root inode
- Volume snapshot (verify inode deep copy, refcount bumps)
- Volume delete (verify refcount decrements, unique chunk freeing)
- Concurrent Raft command application (ordering, linearizability)
- Edge cases: empty directory readdir, rename to self, remove non-existent entry

### Phase 2: Data engine (StormBlock side)

Add chunk read/write with COW semantics, operating through StormBlock's existing BlockDevice/ThinVolumeHandle infrastructure.

**New files in StormBlock:**
- `src/stormfs/chunk.rs` — Chunk read/write, COW logic, refcount management
- `src/stormfs/dedup.rs` — BLAKE3 chunk hashing, dedup table lookup

**Scope:** ~2000 LOC Rust

**Deliverable:** End-to-end file read/write through StormFS (no NFS). Create volume → create file → write data → read data → verify. COW snapshot → write to snapshot → verify original unchanged.

**Tests:**
- Write single chunk, read back, verify
- Write spanning multiple chunks, read back
- Write partial chunk (< 4 MB), verify padding/length
- Read from sparse file (missing chunks return zeroes)
- COW: snapshot, write to snapshot, verify original untouched
- COW: write to original after snapshot, verify snapshot untouched
- Refcount lifecycle: create → snapshot (refcount=2) → delete snapshot (refcount=1) → delete original (refcount=0, chunk freed)
- Dedup: write identical chunk twice, verify single extent with refcount=2
- Concurrent writes to different chunks of same file
- Large file: 1 GB write + read-back integrity check

### Phase 3: NextNFS integration

Connect StormFS to NextNFS's FileManager and NFS operation dispatch.

**New files in NextNFS:**
- `nfs/src/server/stormfs_backend.rs` — StormFsBackend struct, NFS operation implementations

**Modified files in NextNFS:**
- `nfs/src/server/filemanager/mod.rs` — Add `#[cfg(feature = "stormfs")]` dispatch arm
- `nfs/src/server/filemanager/filehandle.rs` — StormFS filehandle encoding/decoding
- `nfs/src/server/export_manager.rs` — `add_stormfs_export()`, config parsing
- `Cargo.toml` — `stormfs` feature flag, optional `stormblock` dependency
- `nfs/Cargo.toml` — Same feature flag passthrough

**Scope:** ~2500 LOC Rust

**Deliverable:** `mount -t nfs4 localhost:/stormfs-export /mnt` works. PUTFH, GETFH, GETATTR, LOOKUP, READDIR, READ, WRITE, CREATE, REMOVE, RENAME, SETATTR, LINK, READLINK all functional against a StormFS volume.

**Tests:**
- Extend `test_utils.rs` with `create_stormfs_server()` helper
- All existing NFS operation tests duplicated for StormFS backend
- StormFS-specific: hardlinks, full POSIX attrs, COW write verification
- Wire tests (nfstest): basic operations against StormFS export

### Phase 4: OCI registry integration

Extend the OCI registry to flatten image layers directly into StormFS volumes.

**Modified files in NextNFS:**
- `nfs/src/server/registry.rs` — StormFS flattening path alongside directory extraction
- REST API: snapshot endpoints, instance lifecycle

**Scope:** ~1500 LOC Rust

**Deliverable:** `podman push myapp:latest localhost:5000/myapp:latest` creates a StormFS golden volume. `POST /api/v1/stormfs/volumes/myapp/snapshot` creates a container instance. The container mounts it via NFS and operates normally.

**Tests:**
- Push image → verify StormFS volume contains correct file tree
- Push multi-layer image → verify layer ordering (later layers override earlier)
- Snapshot golden → write to snapshot → verify golden unchanged
- Delete snapshot → verify shared chunks not freed, unique chunks freed
- Push same base image twice → verify chunk dedup

### Phase 5: Multi-node replication

Ensure chunk routing and metadata access work correctly across multiple NextNFS nodes talking to the same StormBlock cluster.

**Modified files:**
- `nfs/src/server/stormfs_backend.rs` — Chunk routing to correct StormBlock storage node
- Cache coherency: metadata TTL, chunk invalidation on COW

**Scope:** ~2000 LOC Rust

**Deliverable:** Two NextNFS nodes serve the same StormFS volume. Client A writes via Node 1, Client B reads via Node 2, sees the update (within metadata cache TTL). Failover: Node 1 dies, Client A reconnects to Node 2, continues operation.

**Tests:**
- Two NextNFS clients on different nodes, concurrent read/write to same volume
- Metadata cache invalidation after write on another node
- Node failover: kill one NextNFS, verify other continues serving
- Chunk routing: verify reads go to the StormBlock node that owns the slab

### Phase 6: Hardening

Production readiness: integrity verification, small-file optimization, garbage collection, consistency checking, CSI driver, observability.

**Components:**
- **Verity integration**: Merkle tree verification for golden image chunks (read-only volumes)
- **Small-file packing**: Files < 64 KB stored inline in inode (no chunk allocation)
- **GC**: Background sweep for orphaned extents (crash recovery — refcount leak)
- **FSCK**: Offline consistency checker (inode↔directory cross-reference, chunk refcount audit)
- **CSI driver**: Kubernetes CSI plugin for StormFS volume provisioning
- **Metrics**: Prometheus counters for chunk cache hit/miss, Raft latency, COW operations

**Scope:** ~3000 LOC Rust + Go (CSI driver)

**Deliverable:** Production-grade StormFS deployment running on a 10+ node cluster with monitoring, alerting, and automated volume lifecycle.

## Relationship to existing enhancements

| Enhancement | Standalone NextNFS | StormFS-enabled NextNFS |
|------------|-------------------|------------------------|
| **overlay-vfs.md** | Active. overlay.rs is the primary container backend. | Active. overlay.rs stays for standalone exports. StormFS is an alternative backend for distributed deployments. |
| **overlay-registry.md** | Active. Layers extracted to directories, overlay exports created. | Active for standalone mode. StormFS mode adds an alternative: layers flattened directly into StormFS volumes instead of directory extraction. |
| **replicated-overlay-registry.md** | Active. Custom blob fan-out protocol, per-node full replicas, quorum writes. | Simplified. StormBlock cluster handles chunk replication natively. Blob storage can still use the fan-out protocol for OCI compatibility, but layer data lives in StormBlock (already replicated). |
| **dm-verity-layers.md** | Active. Per-block Merkle verification for read-only layer directories. | Active for overlay.rs layers. StormFS golden volumes use the same Merkle verification applied to chunks instead of files. |
| **kubernetes-integration.md** | Active. containerd snapshotter, CRI-O driver, CSI for StormBlock PVCs. | Extended. CSI driver provisions StormFS volumes directly. Snapshotter creates COW snapshots instead of overlay exports. |

## Failure modes

| Scenario | Behavior | Data loss? |
|----------|----------|-----------|
| NextNFS node crash | NFS clients reconnect to another NextNFS node. StormFS volumes served by any node. Chunk cache lost (rebuilt on read). | No |
| StormBlock Raft leader crash | Raft elects new leader (~1s). Metadata operations stall briefly. In-flight writes may need retry. | No |
| StormBlock storage node crash | Chunks on failed node served from replicas (placement engine). Degraded until rebalanced. | No |
| Network partition (NextNFS ↔ StormBlock) | Affected NextNFS node serves cached reads. Writes fail with NFS4ERR_DELAY. Clients retry or failover. | No |
| Disk corruption (StormBlock slab) | Detected by slab checksum. Chunk served from replica. Bad slab repaired from good copy. | No |
| Full Raft quorum loss | Metadata operations unavailable. Cached reads may still work (stale). Requires manual recovery. | Risk |

## Security considerations

- **Chunk encryption at rest**: StormBlock slab encryption (AES-256-XTS) protects chunks on disk. Key management via StormBlock's existing key hierarchy.
- **Transport encryption**: NextNFS↔StormBlock communication over TLS (mutual auth). NFS clients connect via localhost (no encryption needed) or RPC-over-TLS for remote mounts.
- **Volume isolation**: Each volume has a unique `volume_id`. Inode numbers are scoped per volume. A filehandle from volume A cannot access data in volume B — the volume_id in the filehandle is verified on every operation.
- **Quota enforcement**: Per-volume quotas enforced at the Raft state machine level. WRITE and CREATE operations that exceed quota return NFS4ERR_DQUOT.
- **Snapshot immutability**: Golden image volumes can be marked read-only at the Raft level. No write commands accepted. Snapshots inherit writable state.

## Dependencies

**StormBlock (existing crate, extended):**
- `openraft` 0.9 — Raft consensus (already a dependency)
- `serde` + `bincode` — serialization for Raft log entries (already a dependency)

**NextNFS (optional dependency):**
- `stormblock` crate — only pulled in with `--features stormfs`
- `blake3` — chunk hashing for dedup (small, pure Rust, no C dependencies)

No new external service dependencies. No etcd. No Redis. No TiKV. No ZooKeeper. The entire distributed filesystem is two Rust binaries.
