# Add an rspacefs-backed export type to nextnfs

**Status:** proposed — 2026-05-19
**Owner:** glennswest
**Scope:** Add `rspacefs-core` as a Cargo dependency of nextnfs and expose a new export type that serves a `LayerFS` (upper + N lower layers) over NFSv4. Used exclusively by `mkube-registry`'s `/v1/exports` endpoint on RouterOS hosts. nextnfs stays an NFS server — the layered-filesystem logic still lives in rspacefs, just consumed as a library.

## Motivation

`mkube-registry` (per `mkube/enhancements/rspacefs-registry-backend.md`) wants to hand out NFS URLs that RouterOS containers can use as `root-dir`. The URL must resolve to a live merged view of `(per-container upper) + (image's lower layer dirs)` — i.e., an rspacefs `LayerFS`. nextnfs already has a full NFSv4 server, REST API, axum framework, and an `ExportManager` that handles export lifecycle. Letting it take rspacefs as a dependency is the smallest possible change that delivers the capability.

This does **not** re-introduce the overlay/verity code into nextnfs. The earlier extraction (`nextnfs/enhancements/extract-rspacefs.md`) split the code into a standalone library; this change is the inverse direction — nextnfs becomes a consumer of that library. The data flow is one-directional: `nextnfs → rspacefs-core`, never the other way.

## Public API surface

Add one method on `ExportManagerHandle`:

```rust
impl ExportManagerHandle {
    pub async fn add_rspacefs_export(
        &self,
        name: String,
        upper: PathBuf,
        lowers: Vec<PathBuf>,
    ) -> Result<ExportInfo, String>;
}
```

And the corresponding REST endpoint on the nextnfs admin API (axum, port 8080):

```
POST /api/v1/exports
{
  "name": "container-id-or-export-name",
  "type": "rspacefs",
  "upper": "/raid1/registry/exports/<container_id>/upper",
  "lowers": [
    "/raid1/registry/layers/sha256/<layer0_digest>",
    "/raid1/registry/layers/sha256/<layer1_digest>",
    "..."
  ]
}
```

Existing endpoints (`add_export` for plain directories, etc.) are untouched.

## Implementation

```rust
// nfs/Cargo.toml — add dep
[dependencies]
rspacefs-core = { git = "https://github.com/glennswest/rspacefs", rev = "..." }
# or local path during dev: rspacefs-core = { path = "../../rspacefs/crates/rspacefs-core" }
```

```rust
// nfs/src/server/export_manager.rs — add a new variant + handler
use rspacefs_core::LayerFS;

enum ExportManagerMessage {
    AddExport(AddExportRequest),
    AddRspacefsExport(AddRspacefsExportRequest),  // NEW
    RemoveExport(RemoveExportRequest),
    // …
}

struct AddRspacefsExportRequest {
    name: String,
    upper: PathBuf,
    lowers: Vec<PathBuf>,
    respond_to: oneshot::Sender<Result<ExportInfo, String>>,
}

impl ExportManager {
    fn add_rspacefs_export(
        &mut self,
        name: String,
        upper: PathBuf,
        lowers: Vec<PathBuf>,
    ) -> Result<ExportInfo, String> {
        if self.exports.contains_key(&name) {
            return Err(format!("export '{}' already exists", name));
        }
        if lowers.is_empty() {
            return Err("rspacefs export requires at least one lower layer".to_string());
        }

        // Validate each layer dir.
        let upper_canon = upper.canonicalize().map_err(|e| format!("upper: {e}"))?;
        let mut lower_vfs = Vec::with_capacity(lowers.len());
        for l in &lowers {
            let canon = l.canonicalize().map_err(|e| format!("lower {}: {e}", l.display()))?;
            if !canon.is_dir() {
                return Err(format!("{} is not a directory", canon.display()));
            }
            lower_vfs.push(VfsPath::new(PhysicalFS::new(&canon)));
        }
        let upper_vfs = VfsPath::new(PhysicalFS::new(&upper_canon));

        // The whole layered tree is a single VfsPath.
        let layers = LayerFS::new(upper_vfs, lower_vfs);
        let vfs_root: VfsPath = AltrootFS::new(VfsPath::new(layers)).into();

        let export_id = self.next_export_id;
        self.next_export_id += 1;
        let file_manager = FileManagerHandle::new(vfs_root.clone(), Some(export_id as u64), upper_canon.clone());
        // …rest is identical to add_export wiring (stats, rate limit, quota, access control, ExportInfo, insert)
        // …
        Ok(info)
    }
}
```

That's the entire diff in nextnfs. Maybe 60–80 lines counting the actor-message plumbing.

## Why this doesn't violate the original "totally split" intent

The split was about the *layered-filesystem logic* not living inside nextnfs's source tree. After this change:

- `rspacefs-core` still owns 100% of layer / merge / whiteout / copy-up code.
- nextnfs adds a dependency arrow `nextnfs → rspacefs-core` but contains zero of the layer logic itself.
- Other projects (mkube-registry, rspacefs-fuse, dev tools) can use `rspacefs-core` directly without going through nextnfs.
- rspacefs has no awareness of nextnfs.

The dependency is unidirectional. A project that wants a `LayerFS` without an NFS server still gets that.

## Failure modes

| Scenario | Behavior |
|---|---|
| One lower dir disappears mid-flight | `PhysicalFS` returns IO errors; FUSE/NFS clients see EIO on affected paths. nextnfs logs warn and continues. |
| Upper dir runs out of space | Copy-up fails with ENOSPC; container write returns the same. No corruption — upper is a real filesystem. |
| Export name collision | Rejected at `add_rspacefs_export`. `mkube-registry` must use unique `container_id` per export, which it already does. |
| nextnfs restart | Exports do **not** auto-restore from disk in this version. `mkube-registry` re-calls `add_rspacefs_export` on each container creation; idempotent re-attach is the next iteration. |

## Removal API

```
DELETE /api/v1/exports/<name>
```
already exists for plain exports; reuse it. nextnfs side just drops the export from its map. The upper-dir lifecycle is owned by `mkube-registry` (per its `DELETE /v1/exports/<container_id>` policy: immediate delete).

## File layout

```
nfs/src/server/
  export_manager.rs        ← add AddRspacefsExport variant + handler (~80 lines)
  request.rs               ← (no change)
  …
src/main.rs                ← (no change; uses ExportManager via the existing handle)
Cargo.toml                 ← add rspacefs-core dep
```

## Migration

1. Land the dep + new export-type code behind a feature flag (`features = ["rspacefs"]`) initially. Default off.
2. Test in dev with a hand-rolled call to `add_rspacefs_export` against a couple of physical layer dirs.
3. Once `mkube-registry`'s `/v1/exports` is ready, flip the feature flag on by default in nextnfs releases targeting rose1.

## See also

- `mkube/enhancements/rspacefs-registry-backend.md` — the registry side that calls this API.
- `mkube/enhancements/nfs-rootdir-containers.md` — the mkube container-creation change that consumes the NFS URLs nextnfs serves.
- `nextnfs/enhancements/extract-rspacefs.md` — the original split spec. This change explicitly does not re-introduce the extracted code; it adds a one-directional library dep.
