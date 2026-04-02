# Enhancement: Kubernetes / OpenShift / Podman Integration

**Status:** Proposed
**Date:** 2026-03-31
**Priority:** High
**Depends on:** [overlay-vfs.md](overlay-vfs.md) (Phase 1-3)

## Summary

Integrate NextNFS overlay as a drop-in alternative to kernel overlayfs for container runtimes (Podman, CRI-O, containerd) on Kubernetes, OpenShift, and Fedora CoreOS (RHCOS). Provides a containerd snapshotter plugin, a CRI-O/Podman containers/storage driver, and a CSI driver for StormBlock-backed persistent volumes. All components are opt-in and coexist with existing overlay2 storage without interference.

## Motivation

Every RHCOS and CoreOS node in an OpenShift/Kubernetes cluster uses kernel overlayfs (via CRI-O or containerd) for container rootfs. This is the source of:

- Silent data corruption on XFS backing filesystems
- EXDEV failures breaking package managers inside containers
- Copy-up race conditions under concurrent workloads
- fuse-overlayfs instability in rootless/nested scenarios
- Per-node full image pulls with no cross-node layer sharing
- 5-10 minute stateful pod failover times

NextNFS overlay eliminates these issues by moving overlay logic into a Rust userspace process, served over localhost NFS. The integration must be:

1. **Zero disruption** — existing pods keep using overlay2 unless explicitly opted in
2. **Standard Kubernetes APIs** — RuntimeClass, StorageClass, PVC — no custom CRDs required
3. **Per-pod opt-in** — mix overlay2 and NextNFS pods on the same node
4. **No fork of any upstream project** — all integration via official plugin interfaces

## Architecture

### Component overview

```
┌─────────────────────────────────────────────────────┐
│  Kubernetes / OpenShift Control Plane               │
│  (unchanged — standard scheduler, API server, etc.) │
└──────────────────────┬──────────────────────────────┘
                       │
        ┌──────────────┼──────────────────┐
        ▼              ▼                  ▼
   ┌─────────┐   ┌─────────┐       ┌─────────┐
   │ Node 1  │   │ Node 2  │  ...  │ Node N  │
   │         │   │         │       │         │
   │ nextnfs │   │ nextnfs │       │ nextnfs │  ← DaemonSet
   │ :2049   │   │ :2049   │       │ :2049   │
   │         │   │         │       │         │
   │ plugin  │   │ plugin  │       │ plugin  │  ← DaemonSet sidecar
   │         │   │         │       │         │
   │ CRI-O / │   │ CRI-O / │       │ CRI-O / │  ← existing runtime
   │ cntrd   │   │ cntrd   │       │ cntrd   │
   └─────────┘   └─────────┘       └─────────┘
```

Every node runs:
- **nextnfs** — NFS server with overlay VFS (Rust binary, ~9 MB)
- **nextnfs-plugin** — snapshotter/storage driver bridging the container runtime to NextNFS (Go binary)

Both run as a single DaemonSet. Container I/O is always localhost — no network dependency, no single point of failure.

### Data flow: container start

```
1. kubelet receives pod with runtimeClassName: nextnfs
2. CRI-O/containerd calls nextnfs-plugin
3. Plugin checks: are all image layers extracted locally?
   ├── Yes → skip to step 5
   └── No  → pull missing layers from registry
4. Plugin calls NextNFS REST API:
   POST /api/v1/layers/extract {registry: "...", digest: "sha256:..."}
   NextNFS extracts layer to /layers/sha256-xxx/
5. Plugin calls NextNFS REST API:
   POST /api/v1/exports {
     name: "pod-abc-container-0",
     type: "overlay",
     lower: ["sha256-aaa", "sha256-bbb", "sha256-ccc"],
     upper: "/upper/pod-abc-container-0"
   }
6. Plugin returns mount instruction:
   mount -t nfs4 127.0.0.1:/pod-abc-container-0 /run/containers/rootfs/xxx
7. Container starts with NFS-backed rootfs
```

### Data flow: container stop

```
1. CRI-O/containerd calls plugin Remove()
2. Plugin calls NextNFS REST API:
   DELETE /api/v1/exports/pod-abc-container-0
3. Plugin removes /upper/pod-abc-container-0/
4. Shared layers remain in /layers/ (refcount decremented)
```

## Component Specifications

### 1. containerd snapshotter plugin

For vanilla Kubernetes clusters using containerd.

**Interface:** containerd snapshotter gRPC service (proxy plugin)

```go
package main

// Implements containerd's snapshots.Snapshotter interface
type NextNFSSnapshotter struct {
    nextnfsAPI  string   // e.g. "http://127.0.0.1:8080"
    layersDir   string   // e.g. "/var/lib/nextnfs/layers"
    upperDir    string   // e.g. "/var/lib/nextnfs/upper"
    nfsAddr     string   // e.g. "127.0.0.1:2049"
}

// Prepare creates an active (writable) snapshot for a container.
// Called when starting a container.
func (s *NextNFSSnapshotter) Prepare(ctx, key, parent string, opts ...Opt) ([]mount.Mount, error) {
    // 1. Resolve parent chain to get list of layer digests
    // 2. Ensure all layers extracted via NextNFS REST API
    // 3. Create overlay export via NextNFS REST API
    // 4. Return NFS mount instruction
    return []mount.Mount{{
        Type:    "nfs4",
        Source:  fmt.Sprintf("%s:/%s", s.nfsAddr, exportName),
        Options: []string{"vers=4.0", "nolock", "local_lock=all"},
    }}, nil
}

// Commit freezes a snapshot as a new layer (used during image pull/build).
func (s *NextNFSSnapshotter) Commit(ctx, name, key string, opts ...Opt) error {
    // Move upper dir contents to new layer dir
    // Register as content-addressable layer
}

// Remove deletes a snapshot (container stopped).
func (s *NextNFSSnapshotter) Remove(ctx, key string) error {
    // DELETE /api/v1/exports/{name}
    // Remove upper dir
}

// Mounts returns mount instructions for an existing snapshot.
func (s *NextNFSSnapshotter) Mounts(ctx, key string) ([]mount.Mount, error) {
    // Return NFS mount for existing export
}
```

**Registration:**

```toml
# /etc/containerd/config.toml
version = 2

[proxy_plugins.nextnfs]
  type = "snapshot"
  address = "/run/nextnfs/snapshotter.sock"

[plugins."io.containerd.grpc.v1.cri".containerd]
  # Default snapshotter unchanged
  snapshotter = "overlayfs"

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.nextnfs]
  runtime_type = "io.containerd.runc.v2"
  snapshotter = "nextnfs"
```

### 2. CRI-O / Podman containers/storage driver

For OpenShift (CRI-O) and standalone Podman on RHCOS/Fedora.

**Interface:** `containers/storage` `Driver` interface

```go
package nextnfs

import (
    "github.com/containers/storage/drivers"
)

func init() {
    drivers.Register("nextnfs", Init)
}

type Driver struct {
    nextnfsAPI string
    layersDir  string
    upperDir   string
    nfsAddr    string
}

// Create prepares a layer directory.
// parent is the digest of the parent layer (empty for base layers).
func (d *Driver) Create(id, parent string, opts *CreateOpts) error {
    // Create upper dir for this layer/container
}

// CreateReadWrite creates a writable snapshot (container rootfs).
func (d *Driver) CreateReadWrite(id, parent string, opts *CreateOpts) error {
    // 1. Resolve full layer chain from parent
    // 2. Create overlay export via NextNFS REST API
}

// ApplyDiff extracts a layer tarball into the layer directory.
// Called during image pull — each layer is streamed here.
func (d *Driver) ApplyDiff(id, parent string, diff io.Reader) (int64, error) {
    // Stream tar to NextNFS:
    // POST /api/v1/layers/extract (multipart upload)
    // Or extract locally to /layers/sha256-<id>/
}

// Get returns the mount point for a container's rootfs.
func (d *Driver) Get(id string, options graphdriver.MountOpts) (string, error) {
    // 1. Ensure NFS export exists
    // 2. Mount NFS export to local path
    // 3. Return mount path
}

// Put unmounts a container's rootfs.
func (d *Driver) Put(id string) error {
    // Unmount NFS
}

// Remove deletes a container's writable layer.
func (d *Driver) Remove(id string) error {
    // DELETE /api/v1/exports/{id}
    // Remove upper dir
}

// Exists checks if a layer or container exists.
func (d *Driver) Exists(id string) bool {
    // Check /layers/ or /upper/ for this id
}

// Diff produces a tarball of changes in the upper layer.
// Used by podman commit / podman push.
func (d *Driver) Diff(id, parent string) (io.ReadCloser, error) {
    // Tar up upper dir contents, filtering whiteout markers
}
```

**Configuration:**

```toml
# /etc/containers/storage.conf
[storage]
  # Default driver unchanged
  driver = "overlay"

  [storage.options.nextnfs]
    nextnfs_api = "http://127.0.0.1:8080"
    nfs_addr = "127.0.0.1"
    layers_dir = "/var/lib/nextnfs/layers"
    upper_dir = "/var/lib/nextnfs/upper"
```

**CRI-O configuration:**

```toml
# /etc/crio/crio.conf.d/nextnfs.conf
[crio.runtime.runtimes.nextnfs]
  runtime_path = "/usr/bin/crun"
  runtime_type = "oci"
  container_storage_driver = "nextnfs"
```

### 3. Standalone Podman usage

Works directly with Podman — no Kubernetes required:

```bash
# Run a container using NextNFS overlay instead of kernel overlayfs
podman --storage-driver nextnfs run -it alpine sh

# Or set as default in storage.conf
podman info --format '{{.Store.GraphDriverName}}'
# → nextnfs

# Build images using NextNFS (no EXDEV errors)
podman --storage-driver nextnfs build -t myapp .

# Existing overlay2 images remain accessible
podman --storage-driver overlay images
podman --storage-driver nextnfs images
```

Podman builds benefit immediately — no more EXDEV on `RUN rpm --rebuilddb`, `RUN apt-get install`, `RUN npm install` inside Dockerfiles.

### 4. RuntimeClass for Kubernetes opt-in

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: nextnfs
handler: nextnfs
scheduling:
  nodeSelector:
    nextnfs.io/enabled: "true"
```

Per-pod opt-in:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-app
spec:
  runtimeClassName: nextnfs
  containers:
  - name: app
    image: nginx:latest
```

Per-namespace default (via admission webhook, optional):

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: production
  labels:
    nextnfs.io/default-runtime: "true"
```

### 5. OpenShift MachineConfig for RHCOS

Deploy NextNFS and the CRI-O driver to RHCOS nodes via MachineConfig:

```yaml
apiVersion: machineconfiguration.openshift.io/v1
kind: MachineConfig
metadata:
  name: 99-nextnfs
  labels:
    machineconfiguration.openshift.io/role: worker
spec:
  config:
    ignition:
      version: 3.2.0
    systemd:
      units:
      - name: nextnfs.service
        enabled: true
        contents: |
          [Unit]
          Description=NextNFS Overlay Server
          Before=crio.service
          After=network-online.target

          [Service]
          Type=simple
          ExecStart=/usr/local/bin/nextnfs serve --config /etc/nextnfs/config.toml
          Restart=always
          RestartSec=5

          [Install]
          WantedBy=multi-user.target
    storage:
      files:
      - path: /usr/local/bin/nextnfs
        mode: 0755
        contents:
          source: https://releases.nextnfs.io/v0.12.0/nextnfs-linux-amd64
      - path: /etc/nextnfs/config.toml
        mode: 0644
        contents:
          inline: |
            [server]
            listen = "127.0.0.1:2049"
            api_listen = "127.0.0.1:8080"
            state_dir = "/var/lib/nextnfs/state"
      - path: /etc/crio/crio.conf.d/nextnfs.conf
        mode: 0644
        contents:
          inline: |
            [crio.runtime.runtimes.nextnfs]
            runtime_path = "/usr/bin/crun"
            runtime_type = "oci"
            container_storage_driver = "nextnfs"
```

This deploys via the standard OpenShift MCO pipeline — no SSH, no manual intervention, rolling restart across nodes.

## Deployment

### Helm chart (Kubernetes)

```bash
helm repo add nextnfs https://charts.nextnfs.io
helm install nextnfs nextnfs/nextnfs-snapshotter \
  --set runtime=containerd \
  --set layersDir=/var/lib/nextnfs/layers \
  --set upperDir=/var/lib/nextnfs/upper
```

### Helm chart (OpenShift)

```bash
helm install nextnfs nextnfs/nextnfs-crio \
  --set runtime=crio \
  --set layersDir=/var/lib/nextnfs/layers \
  --set upperDir=/var/lib/nextnfs/upper
```

### DaemonSet spec

```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: nextnfs
  namespace: nextnfs-system
spec:
  selector:
    matchLabels:
      app: nextnfs
  template:
    metadata:
      labels:
        app: nextnfs
    spec:
      nodeSelector:
        nextnfs.io/enabled: "true"
      hostNetwork: true
      hostPID: false
      containers:
      - name: nextnfs
        image: ghcr.io/nextnfs/nextnfs:latest
        securityContext:
          privileged: false
          capabilities:
            add: ["SYS_ADMIN"]   # for NFS bind
        ports:
        - containerPort: 2049
          hostPort: 2049
          protocol: TCP
        - containerPort: 8080
          hostPort: 8080
          protocol: TCP
        volumeMounts:
        - name: layers
          mountPath: /var/lib/nextnfs/layers
        - name: upper
          mountPath: /var/lib/nextnfs/upper
        - name: state
          mountPath: /var/lib/nextnfs/state
        resources:
          requests:
            cpu: 100m
            memory: 128Mi
          limits:
            cpu: "2"
            memory: 1Gi
        readinessProbe:
          httpGet:
            path: /health
            port: 8080
          initialDelaySeconds: 5
        livenessProbe:
          httpGet:
            path: /health
            port: 8080
          initialDelaySeconds: 10

      - name: plugin
        image: ghcr.io/nextnfs/nextnfs-plugin:latest
        volumeMounts:
        - name: snapshotter-socket
          mountPath: /run/nextnfs
        - name: containerd-socket
          mountPath: /run/containerd
          readOnly: true

      volumes:
      - name: layers
        hostPath:
          path: /var/lib/nextnfs/layers
          type: DirectoryOrCreate
      - name: upper
        hostPath:
          path: /var/lib/nextnfs/upper
          type: DirectoryOrCreate
      - name: state
        hostPath:
          path: /var/lib/nextnfs/state
          type: DirectoryOrCreate
      - name: snapshotter-socket
        hostPath:
          path: /run/nextnfs
          type: DirectoryOrCreate
      - name: containerd-socket
        hostPath:
          path: /run/containerd
          type: Directory
```

## Layer lifecycle

### Image pull

```
kubelet: pull nginx:latest
  → plugin: resolve manifest → layers [sha256-aaa, sha256-bbb, sha256-ccc]
  → plugin: GET /api/v1/layers/sha256-aaa → 404 (not cached)
  → plugin: POST /api/v1/layers/extract
            {registry: "registry.local:5000", repo: "nginx", digest: "sha256-aaa"}
  → nextnfs: pulls blob, gunzips, extracts to /layers/sha256-aaa/
  → plugin: GET /api/v1/layers/sha256-bbb → 200 (already cached from another image)
  → plugin: skip
  → plugin: layers ready
```

### Layer garbage collection

NextNFS tracks which exports reference which layers. When no exports reference a layer and it hasn't been used within the GC grace period, it's eligible for removal:

```
GET /api/v1/layers
[
  {"digest": "sha256-aaa", "size": 7340032, "refcount": 15, "last_used": "2026-03-31T10:00:00Z"},
  {"digest": "sha256-bbb", "size": 52428800, "refcount": 0, "last_used": "2026-03-28T10:00:00Z"},
]

# sha256-bbb has refcount 0, unused for 3 days → eligible for GC
DELETE /api/v1/layers/sha256-bbb
```

Configurable:

```toml
[layers]
gc_enabled = true
gc_interval_minutes = 60
gc_grace_period_hours = 24    # keep unused layers for 24h before removal
gc_min_free_bytes = 10737418240  # trigger GC when < 10 GB free
```

### Layer pre-warming

For fast pod scheduling, pre-warm layers to nodes before pods are scheduled:

```bash
# Warm all nodes with the base images
curl -X POST http://127.0.0.1:8080/api/v1/layers/warm \
  -H 'Content-Type: application/json' \
  -d '{"images": ["nginx:latest", "alpine:3.19", "ubuntu:24.04"]}'
```

Or via CronJob:

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: nextnfs-warm-layers
spec:
  schedule: "0 */6 * * *"
  jobTemplate:
    spec:
      template:
        spec:
          nodeSelector:
            nextnfs.io/enabled: "true"
          containers:
          - name: warm
            image: curlimages/curl
            command:
            - sh
            - -c
            - |
              curl -X POST http://127.0.0.1:8080/api/v1/layers/warm \
                -H 'Content-Type: application/json' \
                -d '{"images": ["nginx:latest", "alpine:3.19"]}'
          restartPolicy: OnFailure
```

## Failover with standby nodes

### Standby replication

Each node can designate a standby node that keeps a synchronized copy of its layers and upper dirs:

```toml
[replication]
enabled = true
standby_peer = "192.168.1.102:8080"    # standby node's NextNFS API
sync_interval_seconds = 10              # upper dir sync frequency
sync_layers = true                      # replicate extracted layers
sync_uppers = true                      # replicate container writable state
```

### Failover sequence

```
1. Node 1 health check fails                          (~3 seconds)
2. Kubernetes marks node NotReady                      (~40 seconds, configurable)
3. Pods rescheduled to standby Node 2
4. Node 2 NextNFS: all layers already extracted        (0 seconds)
5. Node 2 NextNFS: upper dirs already replicated       (0 seconds)
6. Create overlay exports                              (~milliseconds)
7. Containers start                                    (~1-2 seconds)
8. StormBlock promotes replica PVCs to primary         (~milliseconds)
9. Full service restored
                                              Total pod downtime: ~45 seconds
                                              (dominated by k8s NotReady timeout)
```

Compare to standard Kubernetes failover with image pull:
- Node eviction: 5 minutes (default pod-eviction-timeout)
- Image pull: 30-120 seconds
- Volume reattach: 10-60 seconds
- Total: 6-8 minutes for stateful pods

### With reduced tolerations

```yaml
spec:
  tolerations:
  - key: "node.kubernetes.io/not-ready"
    operator: "Exists"
    effect: "NoExecute"
    tolerationSeconds: 10    # evict after 10s instead of 300s
```

Total failover: ~12 seconds (10s toleration + 2s export creation + container start).

## Observability

### Prometheus metrics

```
# Layer cache
nextnfs_layers_total                     # number of extracted layers
nextnfs_layers_size_bytes                # total layer storage
nextnfs_layer_cache_hits_total           # layer already existed on pull
nextnfs_layer_cache_misses_total         # layer needed extraction
nextnfs_layer_extract_duration_seconds   # histogram of extraction time

# Overlay operations
nextnfs_overlay_exports_active           # current overlay exports
nextnfs_overlay_copyup_total             # copy-up operations
nextnfs_overlay_copyup_bytes_total       # bytes copied during copy-up
nextnfs_overlay_whiteout_total           # whiteout creations
nextnfs_overlay_readdir_merge_duration   # histogram of readdir merge time

# Replication
nextnfs_replication_lag_seconds          # time since last sync to standby
nextnfs_replication_bytes_total          # bytes replicated
nextnfs_replication_errors_total         # replication failures

# NFS server (existing)
nextnfs_nfs_ops_total{op="read|write|create|remove|..."}
nextnfs_nfs_bytes_total{direction="read|write"}
nextnfs_nfs_clients_active
```

### Grafana dashboard

Ship a pre-built Grafana dashboard JSON covering:
- Layer cache hit rate and storage usage
- Copy-up frequency and latency
- Per-node overlay export count
- Replication lag per standby pair
- NFS throughput and latency percentiles

## Testing plan

### Unit tests
- OverlayVfs operations (all file operations through overlay)
- Whiteout creation, filtering, opaque directory handling
- Layer chain resolution from containerd/CRI-O metadata
- Copy-up correctness (permissions, xattrs, timestamps preserved)

### Integration tests
- containerd snapshotter: pull image → create container → read/write files → remove
- CRI-O driver: same lifecycle via CRI-O
- Podman: `podman --storage-driver nextnfs run/build/commit`
- Layer sharing: two containers from same image, verify shared lower dirs
- GC: remove all containers, verify layers cleaned up after grace period

### Stress tests
- 200 concurrent container starts on single node
- Parallel image pulls (10 images simultaneously)
- Copy-up storm (all containers modify same base-layer file)
- Readdir with 10-layer deep images

### Compatibility tests
- `rpm --rebuilddb` inside container (EXDEV regression test)
- `apt-get install` inside container
- `npm install` with node_modules (heavy directory renames)
- `podman build` multi-stage Dockerfile
- Nested container builds (podman-in-podman)

### Platform tests
- x86_64 RHCOS (OpenShift)
- x86_64 Fedora CoreOS (vanilla Kubernetes)
- aarch64 Fedora CoreOS
- x86_64 Ubuntu 24.04 (containerd)

## Implementation phases

### Phase 1: containers/storage driver (CRI-O + Podman)
- Go library implementing `graphdriver.Driver`
- Talks to NextNFS REST API for export management
- Local NFS mount/unmount
- Standalone Podman testing: `podman --storage-driver nextnfs run alpine sh`
- Compatibility tests: rpm, apt-get, npm inside containers
- **Deliverable:** Go module `github.com/nextnfs/containers-storage-nextnfs`

### Phase 2: containerd snapshotter plugin
- Go binary implementing containerd snapshotter gRPC interface
- Proxy plugin running as Unix socket
- Registration in containerd config
- **Deliverable:** Binary `nextnfs-snapshotter`, containerd config snippets

### Phase 3: Kubernetes deployment
- DaemonSet (NextNFS server + plugin sidecar)
- RuntimeClass definition
- Helm chart with configurable values
- Node labeling and scheduling
- **Deliverable:** Helm chart `nextnfs/nextnfs-snapshotter`

### Phase 4: OpenShift integration
- MachineConfig for RHCOS deployment
- CRI-O configuration via MachineConfig
- Operator (optional) for lifecycle management
- OpenShift console plugin for visibility
- **Deliverable:** Helm chart `nextnfs/nextnfs-crio`, MachineConfig manifests

### Phase 5: Standby replication
- Peer-to-peer layer sync between NextNFS instances
- Upper dir async replication
- Health-check-driven failover
- Integration with Kubernetes node health
- **Deliverable:** Replication config, failover documentation

### Phase 6: Observability
- Prometheus metrics exporter
- Grafana dashboard JSON
- Alert rules (replication lag, GC failures, high copy-up rate)
- **Deliverable:** Monitoring stack manifests

## Compatibility matrix

| Platform | Runtime | Plugin type | Tested |
|----------|---------|-------------|--------|
| RHCOS 4.x (OpenShift) | CRI-O | containers/storage driver | Planned |
| Fedora CoreOS | CRI-O | containers/storage driver | Planned |
| Fedora CoreOS | containerd | snapshotter proxy plugin | Planned |
| Ubuntu 24.04 | containerd | snapshotter proxy plugin | Planned |
| RHEL 9 | Podman (standalone) | containers/storage driver | Planned |
| Fedora 40+ | Podman (standalone) | containers/storage driver | Planned |
| MikroTik RouterOS | mkube | NextNFS REST API direct | Planned |
| Any Linux | containerd 1.7+ | snapshotter proxy plugin | Planned |
| Any Linux | CRI-O 1.28+ | containers/storage driver | Planned |

## Risks and mitigations

| Risk | Mitigation |
|------|-----------|
| NFS mount adds latency vs local overlayfs | Localhost mount, sub-millisecond. Benchmark and document. |
| NextNFS process crash takes down all containers | Existing NFS mounts survive brief server restart (grace period recovery). Systemd auto-restart. |
| SELinux labeling for NFS-backed rootfs | NFS supports security labels. Test with enforcing SELinux on RHCOS. |
| containerd/CRI-O API changes | Pin to stable API versions. Snapshotter interface stable since containerd 1.5. |
| Upstream skepticism | Ship as optional, additive, fully compatible. Let benchmark results speak. |
| Layer extraction disk space | GC with configurable grace period and minimum free space trigger. |
