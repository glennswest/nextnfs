# Enhancement: Triple-Replicated Overlay Registry

**Status:** Proposed
**Date:** 2026-03-31
**Priority:** High
**Depends on:** [overlay-registry.md](overlay-registry.md), [dm-verity-layers.md](dm-verity-layers.md)

## Summary

Run three independent NextNFS overlay registry instances, each holding a complete copy of all blobs, extracted layers, and manifests. No shared storage, no common blob store, no single point of failure. When one copy suffers an ECC error, bitrot, or disk corruption, dm-verity detects it immediately, the read is served from a healthy replica, and the bad copy is repaired from the good ones.

## Motivation

### Data must survive hardware failure

Disks fail. SSDs get ECC errors. Firmware has bugs. A single copy of data — no matter how well checksummed — is one hardware failure away from gone. Two copies survive one failure but can't tell which copy is correct when they disagree. Three copies provide:

1. **Quorum** — two out of three agree, the odd one out is wrong
2. **Self-healing** — the bad copy is repaired from the good ones
3. **Continued redundancy** — after healing, you're back to three copies

### dm-verity + replication = detect AND recover

dm-verity (from the verity enhancement) detects corruption. But detection alone isn't enough — you need a healthy copy to serve instead. Together:

```
Read request → Replica A
  → dm-verity check → FAIL (ECC error, bitrot, tampering)
  → don't serve bad data
  → fetch from Replica B → dm-verity check → PASS
  → serve correct data to client
  → background: repair Replica A's bad block from Replica B
```

Without replication, verity can only say "this is corrupt" and return an error. With replication, verity says "this is corrupt" and transparently serves correct data while healing.

## Architecture

### One replica per machine, scales with cluster

The registry is deployed as a **StatefulSet with 1 pod per node**, using required anti-affinity to guarantee no two replicas share a machine. The default is 3 replicas (minimum for quorum), but it scales up to as many nodes as you have — a 50-node cluster can run 50 replicas if desired.

```
Node 1                   Node 2                   Node 3          ...  Node N
├── NextNFS-0            ├── NextNFS-1            ├── NextNFS-2        ├── NextNFS-(N-1)
│   ├── blobs/ (full)    │   ├── blobs/ (full)    │   ├── blobs/ (full)│   ├── blobs/ (full)
│   ├── layers/(full)    │   ├── layers/(full)    │   ├── layers/(full)│   ├── layers/(full)
│   ├── manifests/       │   ├── manifests/       │   ├── manifests/   │   ├── manifests/
│   ├── upper/ (local)   │   ├── upper/ (local)   │   ├── upper/ (local)   ├── upper/ (local)
│   ├── NFS :2049        │   ├── NFS :2049        │   ├── NFS :2049   │   ├── NFS :2049
│   ├── Registry :5000   │   ├── Registry :5000   │   ├── Registry    │   ├── Registry
│   └── local PVC        │   └── local PVC        │   └── local PVC   │   └── local PVC
```

Each instance is a **complete, standalone NextNFS overlay registry**. Any one of them can serve the entire workload independently. They're not shards — they're full replicas.

**Anti-affinity is required, not preferred.** The whole point is that each replica is on different physical hardware with different disks. If two replicas land on the same node, a single disk failure or ECC error takes out multiple copies — defeating the purpose.

### Scaling

| Replicas | Write quorum | Tolerance | Use case |
|----------|-------------|-----------|----------|
| 3 (default) | 2-of-3 | 1 node loss | Small clusters, dev/staging |
| 5 | 3-of-5 | 2 node loss | Production |
| N (1 per node) | majority | (N-1)/2 node loss | Maximum redundancy, every node serves locally |

With 1 replica per node, every container reads from its **local** NextNFS instance — no cross-node NFS traffic for reads. Writes fan out to the write quorum.

### No shared storage

```
❌  Replica-1 ──→ ┐
    Replica-2 ──→ ├── Shared S3 / RWX PVC / network storage
    Replica-3 ──→ ┘

✅  Replica-1 ──→ own local PVC (ReadWriteOnce, local storage)
    Replica-2 ──→ own local PVC (ReadWriteOnce, local storage)
    Replica-3 ──→ own local PVC (ReadWriteOnce, local storage)
```

Each replica uses its own local storage PVC backed by local NVMe/SSD. No network storage between them. The replication protocol keeps them in sync. Local storage is the default — it's the fastest and the only way to guarantee that replicas are on independent failure domains.

## Replication protocol

### Write path: fan-out on push

```
Client: podman push myapp:latest nextnfs-registry:5000/myapp:latest
  │
  ▼
Service (load balancer) → routes to any replica (e.g., Replica-1)
  │
  ▼
Replica-1 (primary for this push):
  1. Receive blob upload
  2. Store blob + extract layer locally (tee-stream)
  3. Verify digest
  4. Fan out to peers:
     → stream blob to Replica-2 (Replica-2 stores + extracts)
     → stream blob to Replica-3 (Replica-3 stores + extracts)
  5. Wait for 2-of-3 ack (including self) → return 201 to client
  6. Third replica catches up asynchronously if slow

Manifest PUT:
  1. Store manifest locally
  2. Replicate manifest to peers
  3. 2-of-3 ack → return 201 to client
```

**Write consistency:** majority quorum acknowledgment before confirming to the client. For 3 replicas: 2-of-3. For 5: 3-of-5. For N: (N/2)+1. The push succeeds when a majority of replicas have the data. The rest catch up asynchronously.

### Read path: local-first with fallback

```
Client: podman pull myapp:latest
  │
  ▼
Service → routes to any replica (e.g., Replica-2)
  │
  ▼
Replica-2:
  1. Look up blob locally → found
  2. If verity enabled: verify block integrity
     ├── PASS → serve to client
     └── FAIL → do NOT serve
              → fetch from Replica-1 or Replica-3
              → verify the fetched copy
              → serve to client
              → background: repair local copy from fetched data
```

The client never sees the corruption. The read transparently falls back to a healthy replica, and the bad copy is repaired.

### Overlay VFS read path

Same principle for NFS reads from overlay layers:

```
Container reads /usr/bin/python3 via NFS
  → NextNFS overlay VFS → lower layer file
    → dm-verity check on block
      ├── PASS → serve via NFS
      └── FAIL → don't serve bad data
               → fetch correct block from peer replica
               → verify peer's copy against verity (it could be bad too)
               → serve correct data via NFS
               → write peer's good block to local disk
               → read back what was written
               → verify read-back against verity
                 ├── PASS → repair confirmed, block trusted
                 └── FAIL → bad sector, mark block as unrepairable locally
                          → log alert, continue serving from peer on future reads
               → if all 3 peers are bad → NFS4ERR_IO (data is unrecoverable)
```

### Peer communication

Replicas talk directly to each other using the OCI registry protocol (for blobs) and a lightweight sync protocol (for manifests and repair):

```toml
[registry.replication]
replicas = [
  "nextnfs-1.nextnfs.svc:5000",
  "nextnfs-2.nextnfs.svc:5000",
  "nextnfs-3.nextnfs.svc:5000",
]
write_quorum = 2          # ack from N replicas before confirming write
read_repair = true        # repair corrupt data from peers on read
```

## Corruption detection and repair

### Detection: dm-verity per block

Every read from a lower layer verifies the block against the Merkle tree:

```
Block read → SHA-256/BLAKE3 hash → compare to Merkle tree → match?
  ├── Yes → serve
  └── No  → corrupt
```

### Repair: fetch from peer

```rust
async fn read_verified_block(&self, path: &str, offset: u64) -> Result<Vec<u8>> {
    // Try local first
    let data = self.local.read_block(path, offset)?;
    if self.verity.verify_block(&data, path, offset) {
        return Ok(data);
    }

    // Local copy corrupt — try peers
    warn!("verity failure: {} offset {} — trying peers", path, offset);

    for peer in &self.peers {
        match peer.fetch_block(path, offset).await {
            Ok(peer_data) => {
                if self.verity.verify_block(&peer_data, path, offset) {
                    // Peer has good copy — write it locally
                    self.local.repair_block(path, offset, &peer_data).await?;

                    // Read back what we just wrote and verify again.
                    // The write could have hit a bad sector, firmware could
                    // have silently dropped it, or the block could have
                    // landed on a failing region of the disk.
                    let readback = self.local.read_block(path, offset)?;
                    if !self.verity.verify_block(&readback, path, offset) {
                        error!("repair verification failed: {} offset {} — \
                                written data does not match on read-back, \
                                possible bad sector", path, offset);
                        // Invalidate the local block cache so we don't
                        // trust this block in the future
                        self.verity.invalidate_block_cache(path, offset);
                        // Still serve the known-good peer data to the client
                        return Ok(peer_data);
                    }

                    // Read-back matches — repair confirmed
                    info!("repaired and verified {} offset {} from peer {}",
                          path, offset, peer.addr);
                    return Ok(peer_data);
                }
                warn!("peer {} also has corrupt copy", peer.addr);
            }
            Err(e) => warn!("peer {} unreachable: {}", peer.addr, e),
        }
    }

    // All copies bad
    error!("UNRECOVERABLE: {} offset {} corrupt on all replicas", path, offset);
    Err(NfsError::Io)
}
```

### Repair protocol

```
Replica-1 detects corrupt block in sha256-abc at offset 16384
  │
  ├── GET /api/v1/repair/sha256-abc?offset=16384&size=4096
  │   → Replica-2 reads block, verifies own verity, returns if good
  │   → Replica-1 verifies received block against verity
  │   → Replica-1 writes good block to local disk
  │   → Replica-1 reads back the written block
  │   → Replica-1 verifies read-back against verity
  │     ├── PASS → repair confirmed
  │     └── FAIL → write landed on bad sector
  │              → mark block as locally unrepairable
  │              → future reads for this block always go to peer
  │              → alert: disk may be degrading
  │
  └── If Replica-2 also bad:
      → try Replica-3
      → if all bad: alert, mark layer as unrecoverable
```

REST API for repair:

```bash
# Fetch a specific block range from a peer (for repair)
GET /api/v1/repair/{layer_digest}?offset={offset}&size={size}
  → Returns raw block data if verity passes
  → Returns 503 if peer's copy is also corrupt

# Trigger full integrity scan and repair against peers
POST /api/v1/repair/full-scan
{
  "layers_checked": 142,
  "blocks_checked": 3686400,
  "blocks_repaired": 2,
  "blocks_unrecoverable": 0,
  "duration_seconds": 47
}

# Repair status
GET /api/v1/repair/status
{
  "last_scan": "2026-03-31T03:00:00Z",
  "pending_repairs": 0,
  "total_repairs": 7,
  "unrecoverable": 0
}
```

### Background scrub

Periodic full verification across all replicas:

```toml
[registry.replication]
scrub_enabled = true
scrub_interval_hours = 24       # full scrub every 24 hours
scrub_rate_limit_mbps = 100     # don't saturate network
scrub_repair = true             # auto-repair during scrub
```

```
Nightly scrub:
  For each layer on this replica:
    For each 4 KB block:
      Verify against local Merkle tree
      If bad:
        → fetch from peer, verify peer's copy
        → write to local disk
        → read back and verify again
          ├── PASS → repair confirmed, count as repaired
          └── FAIL → bad sector, count as locally unrepairable
      If good → continue
  Report: blocks checked, repairs confirmed, verify-after-repair failures, unrecoverable count
```

## Blob store replication

Blobs get the same treatment as layers:

```
Push blob sha256:abc to Replica-1
  → Replica-1 stores to /blobs/sha256/abc
  → Replica-1 streams to Replica-2 → stores to /blobs/sha256/abc
  → Replica-1 streams to Replica-3 → stores to /blobs/sha256/abc
  → 2-of-3 ack → return 201

Pull blob sha256:abc from Replica-2
  → Read local /blobs/sha256/abc
  → Verify SHA-256 of entire blob matches digest (built-in OCI verification)
  → If mismatch → fetch from peer, serve correct copy, repair local
```

Blob verification is simpler than layer verification — the digest IS the hash of the whole file. No Merkle tree needed. Just hash the blob and compare to the filename.

## Manifest consistency

Manifests are mutable (tags can be re-pushed). Use vector clocks or last-write-wins with timestamps:

```
Push myapp:latest to Replica-1 at T=100
  → Replica-1: store manifest with timestamp T=100
  → Replicate to Replica-2 and Replica-3

Push myapp:latest to Replica-3 at T=105 (re-push with new digest)
  → Replica-3: store manifest with timestamp T=105
  → Replicate to Replica-1 and Replica-2
  → All replicas now have T=105 version

Conflict (simultaneous push to different replicas):
  → Higher timestamp wins
  → Ties broken by replica ID
  → Consistent across all replicas
```

## Deployment

### StatefulSet (OpenShift / Kubernetes)

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: nextnfs
  namespace: nextnfs-system
spec:
  replicas: 3                        # default 3, scale up to node count
  serviceName: nextnfs
  podManagementPolicy: Parallel      # all pods start simultaneously
  selector:
    matchLabels:
      app: nextnfs
  template:
    metadata:
      labels:
        app: nextnfs
    spec:
      affinity:
        podAntiAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
          - labelSelector:
              matchLabels:
                app: nextnfs
            topologyKey: kubernetes.io/hostname   # exactly 1 per node
      containers:
      - name: nextnfs
        image: ghcr.io/nextnfs/nextnfs:latest
        args:
        - serve
        - --config
        - /etc/nextnfs/config.toml
        ports:
        - containerPort: 2049
          name: nfs
          hostPort: 2049             # containers on this node connect to localhost
        - containerPort: 5000
          name: registry
        - containerPort: 8080
          name: api
        env:
        - name: POD_NAME
          valueFrom:
            fieldRef:
              fieldPath: metadata.name
        - name: NEXTNFS_HEADLESS_SVC
          value: "nextnfs.nextnfs-system.svc"   # peer discovery via DNS SRV
        volumeMounts:
        - name: data
          mountPath: /var/lib/nextnfs
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
        resources:
          requests:
            cpu: 250m
            memory: 256Mi
          limits:
            cpu: "4"
            memory: 4Gi
  volumeClaimTemplates:
  - metadata:
      name: data
    spec:
      accessModes: [ReadWriteOnce]
      storageClassName: local-storage  # LOCAL DISK — default, required
      resources:
        requests:
          storage: 500Gi
---
# Headless service for StatefulSet DNS (peer discovery)
# Pods discover each other via SRV lookup on this service.
# No hardcoded peer list — scales automatically.
apiVersion: v1
kind: Service
metadata:
  name: nextnfs
  namespace: nextnfs-system
spec:
  clusterIP: None                    # headless — returns all pod IPs
  selector:
    app: nextnfs
  ports:
  - port: 5000
    name: registry
  - port: 8080
    name: api
  - port: 2049
    name: nfs
---
# Client-facing service (load balanced, for external push/pull)
apiVersion: v1
kind: Service
metadata:
  name: nextnfs-registry
  namespace: nextnfs-system
spec:
  selector:
    app: nextnfs
  ports:
  - port: 5000
    name: registry
  - port: 8080
    name: api
---
# Local storage class (if not already present)
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: local-storage
provisioner: kubernetes.io/no-provisioner
volumeBindingMode: WaitForFirstConsumer   # bind PVC to node only when pod is scheduled
reclaimPolicy: Retain
```

**Key design decisions:**

- **`ReadWriteOnce` local PVCs** — each pod gets its own independent local disk. No network storage. This is the default and the only recommended configuration.
- **`requiredDuringScheduling` anti-affinity** — hard requirement, not preferred. Scheduler will refuse to place two replicas on the same node. If you scale replicas beyond node count, excess pods stay Pending.
- **`hostPort: 2049`** — containers on the same node connect to `localhost:2049` for NFS. No cross-node NFS traffic for reads.
- **`WaitForFirstConsumer`** — local PVCs bind to the node where the pod is scheduled, not before. This works with anti-affinity to spread across nodes.
- **`podManagementPolicy: Parallel`** — all replicas start simultaneously. No waiting for ordinal 0 before starting ordinal 1.

### Dynamic peer discovery

No hardcoded peer list. Peers are discovered via DNS SRV lookup on the headless service:

```rust
// Discover peers by querying headless service
async fn discover_peers(headless_svc: &str) -> Vec<SocketAddr> {
    // DNS SRV lookup: _registry._tcp.nextnfs.nextnfs-system.svc
    // Returns all pod IPs in the StatefulSet
    // Filter out self (by POD_NAME)
    // Result: list of live peers, updates as pods come and go
}
```

When you scale from 3 to 5 replicas:
1. Two new pods start on two new nodes
2. They query the headless service, discover 3 existing peers
3. They bootstrap from existing peers (full sync)
4. Existing peers discover the new ones via DNS
5. Write quorum automatically adjusts to 3-of-5
6. No config change, no restart, no manual peer list update

When a node dies:
1. Pod is gone, DNS removes it
2. Remaining peers detect fewer peers
3. Write quorum adjusts (e.g., 2-of-4 if one of 5 is gone)
4. Replacement pod starts on new node, bootstraps, joins

### Scaling up

```bash
# Start with 3
kubectl scale statefulset nextnfs -n nextnfs-system --replicas=3

# Scale to 1-per-node (e.g., 10-node cluster)
kubectl scale statefulset nextnfs -n nextnfs-system --replicas=10

# New pods auto-discover peers, bootstrap, join quorum
# Anti-affinity ensures 1 per node
# If replicas > nodes, excess pods stay Pending (correct behavior)
```

### Scaling down

```bash
# Scale down from 10 to 5
kubectl scale statefulset nextnfs -n nextnfs-system --replicas=5

# Pods 5-9 are terminated
# Their data is still on their local PVCs (Retain policy)
# Remaining 5 pods have full copies — no data loss
# Write quorum adjusts to 3-of-5
```

### Peer discovery via DNS

StatefulSet provides stable DNS names:
```
nextnfs-0.nextnfs.nextnfs-system.svc
nextnfs-1.nextnfs.nextnfs-system.svc
nextnfs-2.nextnfs.nextnfs-system.svc
```

Each instance discovers peers via headless service SRV records or the `NEXTNFS_PEERS` env var.

## Failure scenarios

| Scenario | Behavior (N replicas, majority quorum) | Data loss? |
|----------|----------------------------------------|-----------|
| 1 node disk dies | Other N-1 serve traffic. Replacement syncs from peers. | No |
| 1 node has ECC error in one block | Detected by verity, served from peer, repaired and re-verified | No |
| K nodes have same block corrupt | Any healthy replica serves correct data, repairs the K bad ones | No (if K < N) |
| All N nodes have same block corrupt | NFS4ERR_IO, alert fired, manual investigation | Yes (that block) |
| Up to (N-1)/2 nodes offline | Majority still up, reads and writes continue normally | No |
| Majority offline | Minority serves reads only, writes rejected (no quorum) | No |
| Network partition | Majority side continues read+write, minority side read-only | No |
| Scale up (add nodes) | New pods bootstrap from peers, join quorum automatically | No |
| Scale down (remove nodes) | Removed pods gone, remaining have full copies, quorum adjusts | No |
| Node replacement | New pod on new node, syncs from peers, anti-affinity enforced | No |

## Monitoring

### Prometheus metrics

```
# Replication health
nextnfs_replication_peers_up                           # how many peers are reachable
nextnfs_replication_sync_lag_seconds{peer="..."}       # time since last sync from peer
nextnfs_replication_bytes_sent_total{peer="..."}       # data sent to peer
nextnfs_replication_bytes_received_total{peer="..."}   # data received from peer

# Repair
nextnfs_repair_blocks_repaired_total                   # blocks repaired from peers and verified on read-back
nextnfs_repair_blocks_readback_failed_total            # repairs where read-back verification failed (bad sector)
nextnfs_repair_blocks_unrecoverable_total              # blocks corrupt on all replicas (CRITICAL)
nextnfs_repair_scrub_duration_seconds                  # last scrub duration
nextnfs_repair_scrub_last_timestamp                    # last scrub time

# Write quorum
nextnfs_replication_writes_total                       # total write operations
nextnfs_replication_writes_quorum_met_total             # writes where quorum was met
nextnfs_replication_writes_degraded_total               # writes with fewer than N replicas
```

### Alert rules

```yaml
- alert: NextNFSReplicaDown
  expr: nextnfs_replication_peers_up < 2
  for: 5m
  labels:
    severity: warning
  annotations:
    summary: "NextNFS replica count degraded"

- alert: NextNFSUnrecoverableCorruption
  expr: nextnfs_repair_blocks_unrecoverable_total > 0
  for: 0s
  labels:
    severity: critical
  annotations:
    summary: "Data corruption detected on all replicas"

- alert: NextNFSScrubOverdue
  expr: time() - nextnfs_repair_scrub_last_timestamp > 172800
  for: 0s
  labels:
    severity: warning
  annotations:
    summary: "Integrity scrub hasn't run in 48 hours"

- alert: NextNFSRepairRateHigh
  expr: rate(nextnfs_repair_blocks_repaired_total[1h]) > 10
  for: 15m
  labels:
    severity: warning
  annotations:
    summary: "High repair rate — possible disk degradation"
```

## New replica bootstrap

When adding a replacement replica (disk failure, scale-up):

```
New Replica-3 joins:
  1. Discover peers via DNS
  2. GET /api/v1/registry/catalog from Replica-1 → list of all layers + blobs
  3. For each blob/layer:
     → GET blob from Replica-1 (or Replica-2, round-robin for speed)
     → Store blob locally
     → Extract layer locally
     → Build verity tree
  4. Sync manifests
  5. Mark self as ready
  6. Join quorum for writes
```

Progress reported via API:

```bash
GET /api/v1/replication/bootstrap
{
  "state": "syncing",
  "blobs_total": 142,
  "blobs_synced": 87,
  "bytes_total": 15032385536,
  "bytes_synced": 9217432576,
  "estimated_remaining_seconds": 340
}
```

## Configuration

```toml
[registry.replication]
# Peer discovery via headless service DNS (default, recommended)
# No hardcoded peer list — scales automatically with StatefulSet replicas
discovery = "dns"
headless_service = "nextnfs.nextnfs-system.svc"

# Or hardcoded peers for non-Kubernetes deployments:
# discovery = "static"
# peers = ["192.168.1.101:5000", "192.168.1.102:5000", "192.168.1.103:5000"]

# Write quorum: "majority" (default) or explicit number
# majority = (N/2)+1 where N is discovered replica count
write_quorum = "majority"

# Read repair: fetch from peer and repair when verity fails
read_repair = true

# Background scrub
scrub_enabled = true
scrub_interval_hours = 24
scrub_rate_limit_mbps = 100

# Bootstrap throttle (don't saturate network during initial sync)
bootstrap_rate_limit_mbps = 500
```

## Implementation phases

### Phase 1: Peer discovery and blob fan-out
- StatefulSet DNS-based peer discovery
- Write fan-out: push blob to self + stream to peers
- Write quorum: 2-of-3 ack
- Manifest replication with last-write-wins
- **Deliverable:** Three replicas stay in sync on push

### Phase 2: Read repair with verity
- On verity failure, fetch block from peer
- Verify peer's copy before serving
- Overwrite corrupt local block
- Logging and metrics for repairs
- **Deliverable:** Transparent corruption recovery

### Phase 3: Background scrub
- Periodic full-layer verity scan
- Cross-replica block comparison
- Auto-repair during scrub
- Rate limiting to avoid I/O saturation
- **Deliverable:** Proactive corruption detection

### Phase 4: New replica bootstrap
- Full catalog sync from existing peer
- Blob + layer transfer with progress tracking
- Quorum join after sync complete
- **Deliverable:** Zero-downtime replica replacement

### Phase 5: Partition tolerance
- Network partition detection (peer health checks)
- Majority side continues read+write
- Minority side read-only (no quorum for writes)
- Automatic rejoin and reconciliation after partition heals
- **Deliverable:** Split-brain safe operation
