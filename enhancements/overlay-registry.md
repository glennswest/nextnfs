# Enhancement: Integrated OCI Overlay Registry

**Status:** Proposed
**Date:** 2026-03-31
**Priority:** High
**Depends on:** [overlay-vfs.md](overlay-vfs.md), [dm-verity-layers.md](dm-verity-layers.md)

## Summary

Embed an OCI Distribution Spec v2 container registry directly into NextNFS. When an image is pushed, layers are extracted directly into the overlay VFS layer cache — no intermediate tarball storage, no separate pull step, no external registry dependency. Push an image, mount an overlay. One binary does it all.

```
podman push myapp:latest localhost:5000/myapp:latest
  → layers extracted into NextNFS VFS immediately
  → overlay export available instantly
  → container mounts nextnfs:/myapp-instance-1
```

## Motivation

### The current pipeline is wasteful

```
Today:
  Build image → push to registry (store tarballs)
    → node pulls from registry (download tarballs)
      → extract tarballs to layer dirs
        → NextNFS serves overlay from layer dirs

4 copies of the data:
  1. Builder's local storage
  2. Registry blob storage (compressed tarball)
  3. Node download (compressed tarball, temporary)
  4. Extracted layer directory (actual files)
```

### The integrated pipeline

```
With overlay registry:
  Build image → push to NextNFS (store blob + extract to VFS in one pass)
    → overlay export available immediately
    → blob retained for pulls (digest must match)

3 copies of the data:
  1. Builder's local storage
  2. Registry blob (compressed tarball, required for correct digest on pull)
  3. Extracted layer directory (actual files for overlay VFS)
```

The layer tarball blob must be retained — the OCI digest is the SHA-256 of the compressed tarball, and you cannot reconstruct a byte-identical gzip stream from extracted files. Pulls must serve the original blob to satisfy digest verification. But the extraction happens during push (not as a separate step), and layer dedup means shared layers are stored and extracted only once.

### One binary, three roles

```
nextnfs
  ├── NFS server    (port 2049)  — serves files and overlay exports
  ├── OCI registry  (port 5000)  — receives and serves container images
  └── REST API      (port 8080)  — management, stats, export lifecycle
```

No external registry needed. No image pull step. No layer extraction pipeline. Push and mount.

## Design

### Storage layout

```
/var/lib/nextnfs/
  blobs/
    sha256/
      aaa...                    ← compressed layer tarball (original push data)
      bbb...                    ← compressed layer tarball
      ccc...                    ← image config JSON
      fff...                    ← manifest (also stored under manifests/)

  layers/
    sha256-aaa.../              ← extracted files (content-addressable)
      diff/
        usr/bin/python3
        usr/lib/libpython3.so
        ...
      .verity/                  ← dm-verity Merkle tree (if enabled)
        root_hash
        tree.bin
        manifest.json

    sha256-bbb.../
      diff/
        etc/nginx/nginx.conf
        ...
      .verity/
        ...

  manifests/
    library/
      nginx/
        latest.json             ← OCI manifest (JSON, tiny)
        sha256-fff....json      ← manifest by digest
        latest.type             ← content type
    mycompany/
      myapp/
        v1.2.3.json
        latest.json

  configs/
    sha256-ccc....json          ← OCI image configs (JSON, tiny)

  upper/
    container-1/                ← per-container writable layer
    container-2/
    vm-42/

  state/
    nfs-state.json              ← NFS client state (grace period recovery)
    registry-state.json         ← tag→digest mappings, GC metadata
```

The `blobs/` directory holds the original compressed tarballs as pushed. This is required because the OCI layer digest is the SHA-256 of the compressed tarball — you cannot reconstruct a byte-identical gzip stream from extracted files (gzip headers, timestamps, compression level all affect the output). Pulls must serve the original blob for digest verification to pass.

The `layers/` directory holds the extracted files for overlay VFS use. Both are keyed by the same SHA-256 digest, so dedup is automatic — pushing a layer that already exists (same digest) skips both the blob write and the extraction.

**Dedup matters:** If 10 images share the same alpine base layer (same digest), there is exactly one blob file and one extracted layer directory. The 10 manifests all reference the same digest. Push of the 10th image stores zero additional bytes.

### Push flow: store blob + extract simultaneously

Standard OCI push protocol. On push, the compressed tarball is both stored as a blob AND extracted to the layer directory in a single streaming pass:

```
Client: POST /v2/myapp/blobs/uploads/
Server: 202 Accepted, Location: /v2/myapp/blobs/uploads/<uuid>

Client: PATCH /v2/myapp/blobs/uploads/<uuid>
        Content-Type: application/octet-stream
        Body: <gzipped tar layer data>

Server: Tee-streams through two pipelines simultaneously:
  Pipeline 1 (blob storage):
    1. Write compressed bytes to /blobs/sha256/<digest> (temp file until finalized)
    2. Compute SHA-256 of raw compressed stream (for digest verification)

  Pipeline 2 (layer extraction):
    3. Gunzip the same stream
    4. Untar → write files to /layers/sha256-<digest>/diff/
    5. Build dm-verity Merkle tree as files are written

Client: PUT /v2/myapp/blobs/uploads/<uuid>?digest=sha256:<hex>
Server: Verify computed digest matches declared digest
        → Rename temp blob to final path
        → Pin verity root hash
        → 201 Created (blob stored AND layer extracted)

Client: PUT /v2/myapp/manifests/latest
        Body: <OCI manifest JSON>
Server: Store manifest, resolve layer references
        → 201 Created
```

The push is **streaming with tee** — the HTTP body is split into two concurrent pipelines. One writes the compressed blob to disk (for future pulls), the other decompresses and extracts (for overlay VFS). Single upload, both artifacts produced.

**Dedup on push:** Before starting the upload pipeline, check if `/blobs/sha256/<digest>` already exists. If so, the layer is already stored and extracted — return 201 immediately with zero I/O. This is the standard OCI digest-based dedup that makes cross-image layer sharing efficient.

### Pull flow: serve original blob

When a client pulls, serve the original compressed tarball from the blob store. The digest matches because it's the exact bytes that were pushed:

```
Client: GET /v2/myapp/blobs/sha256:aaa...

Server:
  1. Look up /blobs/sha256/aaa...
  2. Stream the original compressed tarball to client
  3. Client verifies digest matches — it will, it's the original data

  Digest-correct by construction.
```

This is the only correct approach. Regenerating a tarball from extracted files would produce a different gzip stream (different headers, timestamps, compression artifacts) with a different SHA-256 digest, causing every pull client to reject the layer.

**HEAD requests** return the blob size from the stored file, matching the `size` field in the manifest exactly.

### Manifest handling

Manifests are tiny JSON files stored as-is:

```json
{
  "schemaVersion": 2,
  "mediaType": "application/vnd.oci.image.manifest.v1+json",
  "config": {
    "mediaType": "application/vnd.oci.image.config.v1+json",
    "digest": "sha256:ccc...",
    "size": 1234
  },
  "layers": [
    {
      "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
      "digest": "sha256:aaa...",
      "size": 7340032
    },
    {
      "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
      "digest": "sha256:bbb...",
      "size": 2097152
    }
  ]
}
```

The `digest` fields in the manifest reference the same SHA-256 used to name layer directories. The manifest is the link between image tags and overlay layer stacks.

### Multi-platform manifest index

```json
{
  "schemaVersion": 2,
  "mediaType": "application/vnd.oci.image.index.v1+json",
  "manifests": [
    {
      "mediaType": "application/vnd.oci.image.manifest.v1+json",
      "digest": "sha256:fff...",
      "platform": {"architecture": "amd64", "os": "linux"}
    },
    {
      "mediaType": "application/vnd.oci.image.manifest.v1+json",
      "digest": "sha256:eee...",
      "platform": {"architecture": "arm64", "os": "linux"}
    }
  ]
}
```

NextNFS stores manifests for all platforms. When creating an overlay export, it resolves the correct platform manifest automatically based on node architecture.

### Image → overlay export (one API call)

New REST API endpoint that combines "resolve image" + "create overlay export":

```bash
POST /api/v1/exports/from-image
{
  "name": "nginx-pod-1",
  "image": "nginx:latest",
  "platform": "linux/amd64",    # optional, auto-detected
  "upper": "/var/lib/nextnfs/upper/nginx-pod-1"
}

Response:
{
  "name": "nginx-pod-1",
  "type": "overlay",
  "lower": [
    "/var/lib/nextnfs/layers/sha256-aaa.../diff",
    "/var/lib/nextnfs/layers/sha256-bbb.../diff"
  ],
  "upper": "/var/lib/nextnfs/upper/nginx-pod-1",
  "nfs_path": "/nginx-pod-1",
  "verity": true,
  "image_digest": "sha256:fff..."
}
```

The caller doesn't need to know about layer digests, layer ordering, or extraction. Push an image, create an export from it by name — done.

### Layer deduplication

Content-addressable storage means dedup is automatic. Both the blob and the extracted layer are keyed by the same digest:

```
Push nginx:latest    → layers: [sha256-aaa (alpine base), sha256-bbb (nginx)]
  /blobs/sha256/aaa        ← compressed tarball, stored once
  /layers/sha256-aaa/diff/ ← extracted files, extracted once
  /blobs/sha256/bbb
  /layers/sha256-bbb/diff/

Push myapp:latest    → layers: [sha256-aaa (alpine base), sha256-ccc (myapp)]
  sha256-aaa already exists in both blobs/ and layers/ → skip entirely
  /blobs/sha256/ccc        ← only the new layer stored
  /layers/sha256-ccc/diff/ ← only the new layer extracted
```

During push, before processing a layer:
1. Check if `/blobs/sha256/<digest>` exists
2. Yes → blob already stored AND layer already extracted → return 201 immediately (zero I/O)
3. No → tee-stream: store blob + extract layer simultaneously

This is the standard OCI digest-based dedup. 10 images sharing the same alpine base layer = 1 blob file + 1 extracted layer directory. The 10th push of that layer is a no-op.

### Pull-through cache

NextNFS can act as a pull-through cache for upstream registries:

```toml
[registry]
pull_through = true
upstream_registries = [
  "docker.io",
  "ghcr.io",
  "quay.io",
]
```

```
Client: GET /v2/library/alpine/manifests/3.19
Server: Not in local store
  → fetch from docker.io
  → store manifest
  → for each layer:
    → stream from upstream → extract directly to /layers/
  → return manifest to client

Next request for alpine:3.19:
  → served from local store (already extracted)
  → overlay export from pre-extracted layers (instant)
```

First pull extracts layers. Every subsequent use — whether another pull or an overlay export — hits the already-extracted layer cache.

### Webhook notifications

When an image is pushed or updated, notify orchestrators:

```toml
[registry]
webhooks = [
  { url = "http://localhost:8082/api/v1/image-update", events = ["push"] },
]
```

```json
{
  "event": "push",
  "repository": "myapp",
  "tag": "latest",
  "digest": "sha256:fff...",
  "layers": ["sha256:aaa...", "sha256:bbb...", "sha256:ccc..."],
  "timestamp": "2026-03-31T12:00:00Z"
}
```

The orchestrator (mkube, Kubernetes controller, custom) receives the notification and can trigger rolling updates, pre-warm standby nodes, or create new overlay exports.

## OCI Distribution Spec v2 endpoints

Full compliance with the OCI Distribution Spec:

| Method | Endpoint | Purpose |
|--------|----------|---------|
| GET | `/v2/` | API version check |
| HEAD | `/v2/<name>/manifests/<ref>` | Check manifest existence |
| GET | `/v2/<name>/manifests/<ref>` | Fetch manifest |
| PUT | `/v2/<name>/manifests/<ref>` | Push manifest |
| DELETE | `/v2/<name>/manifests/<ref>` | Delete manifest |
| HEAD | `/v2/<name>/blobs/<digest>` | Check blob/layer existence |
| GET | `/v2/<name>/blobs/<digest>` | Fetch blob (tar+gzip on the fly from extracted dir) |
| DELETE | `/v2/<name>/blobs/<digest>` | Delete blob/layer |
| POST | `/v2/<name>/blobs/uploads/` | Start blob upload |
| PATCH | `/v2/<name>/blobs/uploads/<uuid>` | Upload chunk |
| PUT | `/v2/<name>/blobs/uploads/<uuid>?digest=...` | Complete upload (extract on finalize) |
| POST | `/v2/<name>/blobs/uploads/?mount=<digest>&from=<repo>` | Cross-repo blob mount (free — same layer dir) |
| GET | `/v2/<name>/tags/list` | List tags |
| GET | `/v2/_catalog` | List repositories |

### Cross-repo mount optimization

When pushing an image that shares layers with an existing image:

```
Client: POST /v2/myapp-v2/blobs/uploads/?mount=sha256:aaa...&from=myapp
Server: /blobs/sha256/aaa already exists, /layers/sha256-aaa/ already extracted
  → 201 Created (zero work, zero copy, zero extraction)
```

This is a no-op because both blobs and layers are content-addressable by digest. "Mounting" a blob from another repo is just confirming the digest exists — no data movement. The same blob serves pulls for both repos, and the same extracted layer serves overlays for both.

## REST API extensions

### Image-aware export management

```bash
# Create overlay from image (resolves layers automatically)
POST /api/v1/exports/from-image
{
  "name": "my-container",
  "image": "myapp:latest"
}

# List images in the registry
GET /api/v1/registry/images
[
  {
    "repository": "myapp",
    "tags": ["latest", "v1.2.3"],
    "digest": "sha256:fff...",
    "layers": 3,
    "size": 45678912,
    "pushed": "2026-03-31T12:00:00Z",
    "exports_using": 5
  }
]

# Show which exports use which image
GET /api/v1/registry/images/myapp:latest/exports
[
  {"name": "my-container-1", "created": "2026-03-31T12:01:00Z"},
  {"name": "my-container-2", "created": "2026-03-31T12:02:00Z"}
]

# Check if an image has been updated upstream
GET /api/v1/registry/images/myapp:latest/check-update
{
  "current_digest": "sha256:fff...",
  "upstream_digest": "sha256:ggg...",
  "update_available": true
}

# Trigger re-pull from upstream (pull-through mode)
POST /api/v1/registry/images/myapp:latest/refresh
{
  "previous_digest": "sha256:fff...",
  "new_digest": "sha256:ggg...",
  "new_layers": ["sha256:ddd..."],
  "reused_layers": ["sha256:aaa...", "sha256:bbb..."]
}
```

### Garbage collection

```bash
# Preview what would be cleaned
POST /api/v1/registry/gc?dry_run=true
{
  "unreferenced_layers": [
    {"digest": "sha256:old...", "size": 12345678, "last_used": "2026-03-15T00:00:00Z"}
  ],
  "reclaimable_bytes": 12345678
}

# Run GC
POST /api/v1/registry/gc
{
  "removed_layers": 3,
  "reclaimed_bytes": 45678912
}
```

GC rules:
1. A layer is referenced if any manifest in any repository includes its digest
2. A layer is in-use if any active overlay export uses it as a lower layer
3. Both the blob (`/blobs/sha256/<digest>`) and the extracted layer (`/layers/sha256-<digest>/`) are deleted together — they share the same lifecycle
4. A layer is eligible for GC if: unreferenced AND not in-use AND older than grace period
5. Manifests are eligible for GC if: untagged AND not referenced by any index AND older than grace period

```toml
[registry.gc]
enabled = true
interval_hours = 6
grace_period_hours = 24
keep_untagged_manifests = false
```

## Peer replication

For multi-node deployments, NextNFS instances replicate layers between each other:

```
Node 1 (NextNFS)                    Node 2 (NextNFS)
  registry :5000  ◄──push sync──►   registry :5000
  layers/   ◄──layer replicate──►   layers/
  NFS :2049                          NFS :2049
```

### Push sync

When an image is pushed to Node 1, the manifest and layer references are synced to Node 2. Layers are replicated on-demand or eagerly:

```toml
[registry.replication]
peers = ["192.168.1.102:5000"]
mode = "eager"          # "eager" (replicate immediately) or "lazy" (on first use)
```

**Eager:** Push to Node 1 → layers extracted on Node 1 → layers streamed to Node 2 → extracted on Node 2. Both nodes ready.

**Lazy:** Push to Node 1 → manifest synced to Node 2. When Node 2 needs a layer for an overlay export, it pulls from Node 1 (peer-to-peer, already extracted, tar on the fly, extract on receive).

### Peer pull optimization

When Node 2 needs a layer, it prefers pulling from a peer that already has it, rather than going upstream:

```
Node 2 needs sha256-aaa:
  1. Check local: /blobs/sha256/aaa exists? → already have it
  2. Check peers: Node 1 has it? → pull blob from Node 1 via OCI protocol (LAN speed)
  3. Check upstream: docker.io has it? → pull from upstream (internet speed)
```

Peer-to-peer transfer uses the standard OCI pull protocol — Node 2 pulls the blob from Node 1's registry port. The receiving node stores the blob and extracts the layer in one pass (same tee-stream as a normal push). This preserves the original compressed tarball so the digest stays correct for future pulls from Node 2.

```
Node 1 registry :5000 → GET /v2/myapp/blobs/sha256:aaa
  → Node 2 receives blob → tee-stream → store blob + extract layer
```

LAN transfer of a 50 MB compressed layer: ~0.5 seconds at 1 Gbps.

## TLS and authentication

### Registry TLS

```toml
[registry]
listen = "0.0.0.0:5000"
tls_cert = "/etc/nextnfs/registry-tls.crt"
tls_key = "/etc/nextnfs/registry-tls.key"
```

### Authentication

```toml
[registry.auth]
# No auth (default for local/private registries)
mode = "none"

# Basic auth (htpasswd)
mode = "basic"
htpasswd_file = "/etc/nextnfs/htpasswd"

# Token auth (OAuth2 Bearer, compatible with Docker Hub/GHCR flow)
mode = "token"
token_realm = "https://auth.example.com/token"
token_service = "nextnfs-registry"
token_issuer = "nextnfs"
token_key = "/etc/nextnfs/token-key.pem"
```

### Push/pull authorization

```toml
[registry.auth.policy]
# Allow anonymous pull, require auth for push
anonymous_pull = true
push_requires_auth = true

# Per-repository access control (optional)
# [[registry.auth.policy.rules]]
# repository = "mycompany/*"
# actions = ["pull", "push"]
# users = ["admin", "ci-bot"]
```

## Web UI integration

Extend NextNFS's existing web dashboard with a registry view:

```
┌─────────────────────────────────────────────────────┐
│  NextNFS Dashboard                                   │
│  ┌─────┬──────────┬──────────┬─────────┐            │
│  │ NFS │ Registry │ Overlays │ Verity  │            │
│  └─────┴──────────┴──────────┴─────────┘            │
│                                                      │
│  Registry                                            │
│  ┌───────────┬─────────┬────────┬─────────┬───────┐ │
│  │ Image     │ Tags    │ Layers │ Size    │ Used  │ │
│  ├───────────┼─────────┼────────┼─────────┼───────┤ │
│  │ nginx     │ latest  │ 3      │ 45 MB   │ 12    │ │
│  │ alpine    │ 3.19    │ 1      │ 7 MB    │ 25    │ │
│  │ myapp     │ v1.2.3  │ 5      │ 120 MB  │ 3     │ │
│  │           │ latest  │        │         │       │ │
│  └───────────┴─────────┴────────┴─────────┴───────┘ │
│                                                      │
│  Layer Cache                                         │
│  Total: 14 layers, 340 MB                            │
│  Shared: 8 layers referenced by multiple images      │
│  Dedup savings: 210 MB (38% reduction)               │
│                                                      │
│  [Run GC]  [Refresh from upstream]                   │
└─────────────────────────────────────────────────────┘
```

## Configuration

### Full example

```toml
[server]
listen = "0.0.0.0:2049"
api_listen = "0.0.0.0:8080"
state_dir = "/var/lib/nextnfs"

[registry]
enabled = true
listen = "0.0.0.0:5000"
tls_cert = "/etc/nextnfs/tls.crt"
tls_key = "/etc/nextnfs/tls.key"

# Pull-through cache for upstream registries
pull_through = true
upstream_registries = ["docker.io", "ghcr.io", "quay.io"]

# Tarball cache for repeated pulls by external clients
cache_tarballs = false

# Replication to peer nodes
[registry.replication]
peers = ["192.168.1.102:5000", "192.168.1.103:5000"]
mode = "eager"

# Garbage collection
[registry.gc]
enabled = true
interval_hours = 6
grace_period_hours = 24

# Webhooks for push notifications
[[registry.webhooks]]
url = "http://localhost:8082/api/v1/image-update"
events = ["push", "delete"]

# Verity for all layers (from dm-verity enhancement)
[verity]
enabled = true
algorithm = "blake3"

# Regular NFS exports (unchanged, coexist with registry)
[[exports]]
name = "shared-data"
path = "/srv/shared"
read_only = false

# Overlay exports reference images by name
# (or create dynamically via REST API)
```

### Minimal configuration

```toml
[server]
listen = "0.0.0.0:2049"
api_listen = "0.0.0.0:8080"

[registry]
enabled = true
listen = "0.0.0.0:5000"
```

That's it. Push images, create overlay exports, serve over NFS. Everything else has sensible defaults.

## CLI

```bash
# Registry operations
nextnfs registry images                              # list all images
nextnfs registry tags myapp                           # list tags for an image
nextnfs registry inspect myapp:latest                 # show manifest + layer details
nextnfs registry delete myapp:v1.0.0                  # delete a tag
nextnfs registry gc --dry-run                         # preview GC
nextnfs registry gc                                   # run GC

# Combined workflow
nextnfs registry pull docker.io/library/nginx:latest  # pull-through from upstream
nextnfs export from-image --name web-1 --image nginx:latest  # create overlay export
nextnfs export from-image --name web-2 --image nginx:latest  # another, shares layers

# Replication
nextnfs registry replicate --to 192.168.1.102:5000    # push all images to peer
nextnfs registry replicate --from 192.168.1.102:5000  # pull all images from peer
```

## Comparison

| Feature | External registry + NextNFS | Integrated overlay registry |
|---------|----------------------------|----------------------------|
| Push | Store tarball blob | Store blob + extract to VFS (one pass) |
| Pull for overlay | Download tarball → extract | Already extracted (instant) |
| Pull for podman/skopeo | Serve blob | Serve same blob (digest-correct) |
| Storage | Tarball + extracted dir (2x, separate systems) | Blob + extracted dir (2x, same system, deduped by digest) |
| Layer sharing | By digest (separate stores) | By digest (single content-addressable store) |
| Components | 2 services | 1 binary |
| Configuration | Registry config + NextNFS config | 1 TOML file |
| Network hops | Push → registry → NextNFS pull → extract | Push → done |
| Time to mountable | Pull time + extract time | Push time (streaming tee) |
| Cross-repo mount | Copy tarball | No-op (same blob + same layer dir) |
| Verity | Build tree after extract | Build tree during extract |

## Implementation phases

### Phase 1: OCI Distribution Spec server
- HTTP endpoints: manifest CRUD, blob upload (chunked), tags list, catalog
- Extract-on-push: streaming gunzip → untar → write to layer dir
- Digest verification on upload finalize
- Tar-on-pull: generate tarball from extracted layer dir on GET
- Content-type handling for OCI and Docker v2 manifests
- **Deliverable:** `podman push/pull` works against NextNFS registry

### Phase 2: Image-to-export API
- `POST /api/v1/exports/from-image` endpoint
- Resolve image tag → manifest → layer list → create overlay export
- Multi-platform manifest index support
- Web UI registry tab
- **Deliverable:** Push image → create export → mount NFS in one flow

### Phase 3: Pull-through cache
- Upstream registry fetching with OAuth2 token exchange
- Streaming extract from upstream (no intermediate tarball)
- Manifest caching and digest change detection
- Configurable upstream registries
- **Deliverable:** `pull_through = true` works for docker.io, ghcr.io, quay.io

### Phase 4: Peer replication
- Manifest sync between peers
- Peer-to-peer layer transfer (tar-on-the-fly, LAN optimized)
- Eager and lazy replication modes
- Peer discovery and health checking
- **Deliverable:** Multi-node layer availability without upstream dependency

### Phase 5: Authentication and authorization
- Basic auth (htpasswd)
- Token auth (OAuth2 Bearer, Docker-compatible)
- Per-repository push/pull policy
- TLS for registry port
- **Deliverable:** Secured registry suitable for production

### Phase 6: Garbage collection and lifecycle
- Reference counting: manifest → layer, export → layer
- Automatic GC with grace period
- Webhook notifications on push/delete
- Image update checking against upstream
- **Deliverable:** Self-maintaining storage with no manual cleanup
