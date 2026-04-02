# Enhancement: dm-verity Integrity Verification for Read-Only Layers

**Status:** Proposed
**Date:** 2026-03-31
**Priority:** High
**Depends on:** [overlay-vfs.md](overlay-vfs.md) (Phase 1)

## Summary

Add dm-verity-style cryptographic integrity verification to all read-only overlay layers in NextNFS. Every block of every file in a lower layer is verified against a Merkle hash tree on access. Tampering, bitrot, or corruption in any layer is detected immediately and rejected — not served silently to containers or VMs.

This provides the same guarantee as Linux dm-verity (used by Android Verified Boot, Chrome OS, Fedora CoreOS ostree) but implemented in userspace Rust inside NextNFS, with no kernel dm-verity or block device setup required.

## Motivation

### The problem: silent layer corruption

Read-only layers are assumed immutable and trusted. But they can be corrupted by:

- **Bitrot** — silent disk bit flips (URC errors, aging media)
- **Firmware bugs** — drive firmware silently returns wrong data
- **Malicious tampering** — attacker modifies layer files on disk
- **Admin accidents** — someone writes to a "read-only" layer directory
- **Memory errors** — ECC failure during I/O, DMA corruption
- **Filesystem bugs** — XFS/ext4 metadata corruption leaking into data

Without verification, corrupted layer data is served to containers as if it were correct. A flipped bit in `libc.so` causes random segfaults. A tampered `/etc/passwd` grants unauthorized access. A corrupted Python module silently produces wrong results.

### Why dm-verity at the NFS layer

| Approach | Protection | Limitation |
|----------|-----------|------------|
| Filesystem checksums (btrfs, ZFS) | Detects bitrot | Requires specific filesystem, not available everywhere |
| dm-verity (kernel) | Full Merkle tree verification | Requires block device setup, dm target, kernel support |
| IMA/EVM (kernel) | Per-file signature | Heavy, requires key management, TPM integration |
| **NextNFS verity** | Full Merkle tree verification | Userspace, works on any filesystem, no kernel setup |

NextNFS verity gives dm-verity-grade protection without requiring the host to have dm-verity kernel support, block device configuration, or a specific filesystem. It works on ext4, XFS, tmpfs, NFS-backed layers — anything.

### Supply chain security

OCI image layers are signed and verified during pull (cosign, Notary). But after extraction to disk, there is **zero ongoing verification**. The layer sits on disk for days/months — any corruption between pull and use goes undetected.

NextNFS verity closes this gap: verify on every read, for the entire lifetime of the layer.

## Design

### Merkle hash tree per layer

When a layer is extracted (or imported), NextNFS builds a Merkle hash tree:

```
Layer: /layers/sha256-abc123/
Files: usr/bin/python3, usr/lib/libpython3.so, etc/ld.so.cache, ...

Merkle tree:
                    [root hash]
                   /            \
            [hash-01]          [hash-23]
           /        \         /        \
     [hash-0]  [hash-1]  [hash-2]  [hash-3]
        |         |         |         |
    block-0   block-1   block-2   block-3
    (4 KB)    (4 KB)    (4 KB)    (4 KB)
```

The tree covers all file content and metadata (permissions, ownership, timestamps, xattrs) in the layer. The root hash is stored alongside the layer:

```
/layers/sha256-abc123/
  .verity/
    root_hash           ← 32 bytes (SHA-256), signed or pinned
    tree.bin            ← Merkle tree nodes (compact binary format)
    manifest.json       ← file list with per-file hashes and metadata hashes
  diff/
    usr/bin/python3     ← actual layer files
    usr/lib/libpython3.so
    ...
```

### Verification on read

When NextNFS serves a file from a verified layer:

```
NFS READ request for /usr/bin/python3, offset 8192, count 4096
  1. Identify: file is in layer sha256-abc123 (via index)
  2. Read 4 KB block from disk
  3. Hash the block (SHA-256)
  4. Walk Merkle tree: block hash → intermediate nodes → root hash
  5. Compare computed root hash against stored root hash
     ├── Match → serve the data
     └── Mismatch → return NFS4ERR_IO, log alert, refuse to serve
```

This is the same algorithm as dm-verity, fs-verity, and Android Verified Boot.

### Block size and performance

```
Block size: 4 KB (matches filesystem page size)
Hash algorithm: SHA-256 (32 bytes per hash)
Tree overhead: ~0.8% of layer size (1 hash per 4 KB block, tree nodes)

Example: 100 MB layer
  → 25,600 leaf hashes
  → ~800 KB Merkle tree
  → 32 bytes root hash
```

### Caching verified blocks

Once a block is verified, cache the result:

```rust
struct VerifiedBlockCache {
    /// block_id → verified flag (bitset, 1 bit per 4 KB block)
    verified: BitVec,
    /// root hash this cache was built against
    root_hash: [u8; 32],
}
```

First read of a block: hash + verify (~2 µs for 4 KB SHA-256). Subsequent reads: check bitset (~1 ns). Cache is invalidated if the process restarts or the layer is re-imported.

Memory cost: 1 bit per 4 KB block = 3.2 KB per 100 MB layer = 32 KB per 1 GB layer. Negligible.

### Root hash trust anchors

The root hash must be trusted. Options:

**1. Digest-pinned (default):**
The OCI image digest (sha256 of the layer tarball) is the trust anchor. NextNFS verifies:
- Layer tarball digest matches the manifest (already done during pull)
- Merkle root hash matches the extracted content
- Pin the root hash at extraction time

```toml
[verity]
mode = "digest-pinned"    # root hash derived from layer content at extraction
```

**2. Signature-verified:**
Root hashes signed by a trusted key (cosign, GPG, or X.509):

```toml
[verity]
mode = "signed"
trust_keys = ["/etc/nextnfs/verity-keys/"]
```

```
/layers/sha256-abc123/.verity/
  root_hash.sig         ← signature over root hash
```

NextNFS rejects any layer whose root hash signature doesn't verify against a trusted key.

**3. External attestation:**
Root hashes fetched from an external attestation service (Sigstore, Rekor transparency log):

```toml
[verity]
mode = "sigstore"
rekor_url = "https://rekor.sigstore.dev"
```

### Integration with overlay VFS

The overlay VFS reads from lower layers. Verity is transparent — the overlay code doesn't change:

```
NFS READ → OverlayVfs::read()
             → check upper (no verity, it's writable)
             → fallback to lower layer
               → VerifiedLayerVfs::read()
                 → read block from disk
                 → verify against Merkle tree
                 → return data (or error)
```

The `VerifiedLayerVfs` wraps a `PhysicalFS` and adds verification. The overlay VFS uses it as a drop-in replacement for lower layers.

```rust
/// A VFS wrapper that verifies every read against a Merkle hash tree.
struct VerifiedLayerVfs {
    inner: VfsPath,              // actual filesystem
    merkle_tree: MerkleTree,     // pre-loaded tree
    root_hash: [u8; 32],         // trusted root hash
    block_cache: VerifiedBlockCache,  // verified block bitmap
}

impl VerifiedLayerVfs {
    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let n = self.inner.read(path, offset, buf)?;
        self.verify_blocks(path, offset, &buf[..n])?;
        Ok(n)
    }

    fn verify_blocks(&self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        for block in self.blocks_covering(path, offset, data.len()) {
            if self.block_cache.is_verified(block) {
                continue;  // already verified this session
            }
            let computed = sha256(&data[block.range()]);
            if !self.merkle_tree.verify(block.index, &computed, &self.root_hash) {
                error!("verity: corruption detected in layer {} at block {}",
                       self.layer_digest, block.index);
                return Err(NfsError::Io);
            }
            self.block_cache.mark_verified(block);
        }
        Ok(())
    }
}
```

### Metadata verification

File metadata (permissions, ownership, timestamps) is also covered:

```json
// .verity/manifest.json
{
  "files": [
    {
      "path": "usr/bin/python3",
      "mode": 33261,
      "uid": 0,
      "gid": 0,
      "size": 5765624,
      "content_hash": "sha256:a1b2c3...",
      "xattrs_hash": "sha256:d4e5f6..."
    }
  ],
  "manifest_hash": "sha256:..."
}
```

On GETATTR, NextNFS can verify that the returned metadata matches the manifest. A tampered permission (e.g., making a setuid binary) is detected.

## Configuration

```toml
[verity]
# Enable verity for all read-only layers (default: false)
enabled = true

# Verification mode
mode = "digest-pinned"      # "digest-pinned", "signed", "sigstore"

# Hash algorithm
algorithm = "sha256"         # "sha256" (default), "sha512", "blake3"

# Block size for Merkle tree
block_size = 4096            # bytes, must be power of 2

# Trust keys directory (for mode = "signed")
# trust_keys = ["/etc/nextnfs/verity-keys/"]

# Action on verification failure
on_failure = "reject"        # "reject" (return IO error), "warn" (log + serve), "quarantine"

# Verify metadata (permissions, ownership) in addition to content
verify_metadata = true

# Cache verified blocks in memory (recommended)
cache_verified = true

# Log all verification events (verbose, for debugging)
audit_verify = false
```

### Per-export override

```toml
[[exports]]
name = "high-security-app"
type = "overlay"
lower = ["/layers/sha256-aaa", "/layers/sha256-bbb"]
upper = "/upper/app-1"
verity = true                    # enable for this export
verity_on_failure = "reject"     # override global setting

[[exports]]
name = "dev-sandbox"
type = "overlay"
lower = ["/layers/sha256-ccc"]
upper = "/upper/dev-1"
verity = false                   # skip for dev/test
```

## REST API

```bash
# Check verity status of a layer
GET /api/v1/layers/sha256-abc123/verity
{
  "verified": true,
  "root_hash": "sha256:9f86d081884c7d659a2feaa0c55ad015...",
  "algorithm": "sha256",
  "block_size": 4096,
  "total_blocks": 25600,
  "verified_blocks": 18432,      # blocks verified so far this session
  "failures": 0,
  "tree_size": 819200,
  "signed": false
}

# Rebuild verity tree (after manual layer repair)
POST /api/v1/layers/sha256-abc123/verity/rebuild

# Verify entire layer proactively (background full scan)
POST /api/v1/layers/sha256-abc123/verity/full-check
{
  "status": "ok",
  "blocks_checked": 25600,
  "blocks_failed": 0,
  "duration_ms": 1234
}

# Import a layer with a pre-built verity tree
POST /api/v1/layers/import
Content-Type: multipart/form-data
  layer.tar.gz + verity.tree + root_hash.sig
```

## Prometheus metrics

```
# Verity verification
nextnfs_verity_blocks_verified_total          # blocks verified (cumulative)
nextnfs_verity_blocks_cached_total            # cache hits (skipped verification)
nextnfs_verity_failures_total{layer="..."}    # verification failures (CRITICAL alert)
nextnfs_verity_verify_duration_seconds        # histogram of per-block verify time

# Per-layer
nextnfs_verity_layer_coverage_ratio{layer="..."} # fraction of blocks verified this session
nextnfs_verity_tree_size_bytes{layer="..."}      # Merkle tree memory usage
```

Alert rule:
```yaml
- alert: NextNFSVerityFailure
  expr: nextnfs_verity_failures_total > 0
  for: 0s     # immediate
  labels:
    severity: critical
  annotations:
    summary: "Layer integrity violation detected"
    description: "Layer {{ $labels.layer }} failed verity check. Possible corruption or tampering."
```

## Performance impact

### Per-read overhead

```
SHA-256 of 4 KB block:  ~2 µs (modern x86_64, hardware SHA-NI)
Merkle tree walk:       ~0.5 µs (log2(blocks) hash comparisons, cached in memory)
Total first-read:       ~2.5 µs per 4 KB block
Cached re-read:         ~1 ns (bitset lookup)

Throughput impact (first read, uncached):
  Without verity: ~2 GB/s (NVMe + NFS localhost)
  With verity:    ~1.5 GB/s (SHA-256 bottleneck)
  With BLAKE3:    ~1.9 GB/s (BLAKE3 is 3x faster than SHA-256)
  Cached:         ~2 GB/s (no overhead after first verify)
```

For typical container workloads (read configs, libraries, binaries — then work in memory), the one-time verification cost is paid during startup and amortized immediately.

### Hardware acceleration

```rust
// Detect and use hardware SHA acceleration
#[cfg(target_arch = "x86_64")]
fn sha256_block(data: &[u8]) -> [u8; 32] {
    // Uses SHA-NI instructions if available (Intel Goldmont+, AMD Zen+)
    // Falls back to software SHA-256
    ring::digest::digest(&ring::digest::SHA256, data)
}
```

The `ring` crate (already a common Rust dependency) automatically uses SHA-NI when available. On modern Intel/AMD CPUs, SHA-256 runs at near memory bandwidth.

### BLAKE3 option

For maximum throughput, offer BLAKE3 as an alternative hash:

```toml
[verity]
algorithm = "blake3"    # ~3x faster than SHA-256, still cryptographically secure
```

BLAKE3 is designed for data integrity verification and can saturate NVMe bandwidth even without hardware acceleration. It's also tree-structured internally, which maps naturally to Merkle trees.

## Use cases

### 1. Supply chain integrity (Kubernetes/OpenShift)

```
Image pull → verify signature (cosign) → extract layers → build Merkle trees
  → pin root hashes → serve via NextNFS overlay
  → every read verified for the lifetime of the layer
  → tampered layer file → NFS4ERR_IO → container fails to start → alert fired
```

Closes the gap between "image was verified at pull time" and "image data is still intact at runtime."

### 2. Compliance (FedRAMP, HIPAA, PCI-DSS)

Regulatory frameworks require data integrity verification. NextNFS verity provides:
- Continuous integrity monitoring (not just point-in-time)
- Tamper detection with audit trail
- Cryptographic proof of layer integrity
- Signed root hashes for chain of custody

### 3. Multi-tenant isolation

In shared infrastructure, one tenant's compromised container could tamper with shared base layers on disk. Verity ensures:
- Shared layers cannot be modified without detection
- Each tenant's containers get cryptographically verified base images
- Compromise of one tenant doesn't affect layer integrity for others

### 4. Bitrot protection

Long-lived layers on spinning disks or aging SSDs accumulate bit errors. Background full-check scans detect and alert before corrupted data is served:

```toml
[verity]
background_scan = true
scan_interval_hours = 24    # full scan every 24 hours
```

### 5. Forensic verification

After a security incident, verify whether any layer files were modified:

```bash
nextnfs verity check --layer sha256-abc123 --full
# Layer sha256-abc123: OK (25600/25600 blocks verified)

nextnfs verity check --layer sha256-def456 --full
# Layer sha256-def456: FAILED
#   Block 14231: expected sha256:a1b2c3... got sha256:x9y8z7...
#   File: usr/sbin/sshd (offset 58245120)
#   ALERT: possible tampering detected
```

## Implementation phases

### Phase 1: Merkle tree builder
- Build Merkle hash tree from extracted layer directory
- SHA-256 and BLAKE3 support
- Compact binary tree format (`.verity/tree.bin`)
- File manifest with per-file content and metadata hashes
- Root hash output
- Unit tests: build tree, verify blocks, detect corruption
- **Deliverable:** `nextnfs-verity` library crate

### Phase 2: Verified layer VFS
- `VerifiedLayerVfs` wrapper implementing `vfs` trait
- On-read block verification against Merkle tree
- Verified block cache (bitset)
- On-failure behavior: reject / warn / quarantine
- Integration with overlay VFS as lower layer backend
- **Deliverable:** Verified reads working end-to-end through NFS

### Phase 3: Layer extraction integration
- Automatic Merkle tree build on layer extraction
- Root hash pinning (digest-pinned mode)
- REST API: verity status, full check, rebuild
- Prometheus metrics
- **Deliverable:** Verity enabled by default for new layers

### Phase 4: Signed root hashes
- Signature verification for root hashes (cosign, GPG, X.509)
- Trust key directory management
- Sigstore/Rekor integration (optional)
- CLI: `nextnfs verity sign`, `nextnfs verity verify`
- **Deliverable:** Signed verity mode

### Phase 5: Background scanning
- Periodic full-layer verification (background task)
- Alerting on detection (Prometheus, webhook)
- Quarantine mode: isolate corrupted layer, refuse to serve, attempt re-pull
- **Deliverable:** Continuous integrity monitoring
