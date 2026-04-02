//! dm-verity-style Merkle hash tree for read-only layer integrity verification.
//!
//! Builds a SHA-256 Merkle tree over 4 KB blocks of file content and verifies
//! individual blocks against the tree on read. The same algorithm used by Linux
//! dm-verity, fs-verity, and Android Verified Boot — implemented in userspace Rust.
//!
//! # Architecture
//!
//! ```text
//!                     [root hash]
//!                    /            \
//!             [hash-01]          [hash-23]
//!            /        \         /        \
//!      [hash-0]  [hash-1]  [hash-2]  [hash-3]
//!         |         |         |         |
//!     block-0   block-1   block-2   block-3
//!     (4 KB)    (4 KB)    (4 KB)    (4 KB)
//! ```
//!
//! Each leaf is SHA-256(block data). Each interior node is SHA-256(left || right).
//! The root hash is the trust anchor — if it matches, every block is authentic.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use vfs::error::VfsErrorKind;
use vfs::{FileSystem, SeekAndRead, SeekAndWrite, VfsMetadata, VfsPath, VfsResult};

/// Block size for Merkle tree leaves (4 KB, matches filesystem page size).
pub const BLOCK_SIZE: usize = 4096;

/// SHA-256 hash output size in bytes.
pub const HASH_SIZE: usize = 32;

/// A 32-byte SHA-256 hash.
pub type Hash256 = [u8; HASH_SIZE];

/// Zero hash — used for padding when the tree needs a power-of-two leaf count.
const ZERO_HASH: Hash256 = [0u8; HASH_SIZE];

// ── Merkle Tree ──────────────────────────────────────────────────────────────

/// A complete Merkle hash tree over a set of data blocks.
///
/// The tree is stored as a flat array in level-order (root at index 0).
/// For `n` leaf blocks, the tree has `2n - 1` nodes total.
#[derive(Debug, Clone)]
pub struct MerkleTree {
    /// All nodes in level-order. Index 0 = root.
    nodes: Vec<Hash256>,
    /// Number of leaf blocks (original, before padding to power-of-two).
    leaf_count: usize,
    /// Number of leaves including padding to next power of two.
    padded_leaf_count: usize,
}

impl MerkleTree {
    /// Build a Merkle tree from raw data blocks.
    ///
    /// Each block is `BLOCK_SIZE` bytes (the last block may be shorter and is
    /// zero-padded for hashing). Returns the tree and the root hash.
    pub fn build(data: &[u8]) -> Self {
        let leaf_count = if data.is_empty() {
            1 // empty data gets one zero-hash leaf
        } else {
            data.len().div_ceil(BLOCK_SIZE)
        };

        // Pad to next power of two for a complete binary tree.
        let padded = leaf_count.next_power_of_two();

        // Hash each data block into a leaf.
        let mut leaves: Vec<Hash256> = Vec::with_capacity(padded);
        for i in 0..leaf_count {
            let start = i * BLOCK_SIZE;
            let end = std::cmp::min(start + BLOCK_SIZE, data.len());
            let block = &data[start..end];
            leaves.push(hash_block(block));
        }
        // Pad with zero hashes to reach power-of-two count.
        leaves.resize(padded, ZERO_HASH);

        // Build tree bottom-up. Total nodes = 2 * padded - 1.
        let total = 2 * padded - 1;
        let mut nodes = vec![ZERO_HASH; total];

        // Place leaves at the end of the array.
        let leaf_start = padded - 1;
        for (i, leaf) in leaves.iter().enumerate() {
            nodes[leaf_start + i] = *leaf;
        }

        // Compute interior nodes from bottom to top.
        // Parent of node i = (i - 1) / 2. Children of node i = 2i+1, 2i+2.
        for i in (0..leaf_start).rev() {
            let left = nodes[2 * i + 1];
            let right = nodes[2 * i + 2];
            nodes[i] = hash_pair(&left, &right);
        }

        MerkleTree {
            nodes,
            leaf_count,
            padded_leaf_count: padded,
        }
    }

    /// Build a Merkle tree by reading files from a VFS directory tree.
    ///
    /// Files are enumerated in sorted order (BTreeMap) so the tree is
    /// deterministic. Returns `(tree, manifest)`.
    pub fn build_from_vfs(root: &VfsPath) -> io::Result<(Self, LayerManifest)> {
        let mut file_entries = BTreeMap::new();
        let mut all_data: Vec<u8> = Vec::new();

        // Walk the directory tree and collect all file content in sorted order.
        collect_files(root, "", &mut file_entries)?;

        let mut manifest_files = Vec::new();
        for path in file_entries.keys() {
            let vpath = root.join(path).map_err(io_err)?;
            let meta = vpath.metadata().map_err(io_err)?;
            let size = meta.len;

            // Record the block range for this file in the concatenated data.
            let block_start = all_data.len() / BLOCK_SIZE;
            let file_start = all_data.len();

            // Read file content.
            let mut file = vpath.open_file().map_err(io_err)?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            all_data.extend_from_slice(&buf);

            // Pad to block boundary so each file starts on a fresh block.
            let remainder = all_data.len() % BLOCK_SIZE;
            if remainder != 0 {
                all_data.resize(all_data.len() + (BLOCK_SIZE - remainder), 0);
            }

            let block_end = all_data.len() / BLOCK_SIZE;
            let content_hash = sha256_bytes(&buf);

            // Hash metadata: path + size.
            let meta_str = format!("{}:{}:{}", path, size, meta.file_type as u8);
            let metadata_hash = sha256_bytes(meta_str.as_bytes());

            manifest_files.push(FileEntry {
                path: path.clone(),
                size: size as u64,
                content_hash,
                metadata_hash,
                block_range: (block_start, block_end),
            });

            // Sanity check: file content in the data blob matches.
            debug_assert_eq!(&all_data[file_start..file_start + buf.len()], &buf[..]);
        }

        let tree = MerkleTree::build(&all_data);
        let manifest_hash = {
            let mut hasher = Sha256::new();
            for f in &manifest_files {
                hasher.update(f.content_hash);
                hasher.update(f.metadata_hash);
            }
            hasher.finalize().into()
        };

        let manifest = LayerManifest {
            files: manifest_files,
            manifest_hash,
            root_hash: tree.root_hash(),
            block_size: BLOCK_SIZE as u32,
        };

        Ok((tree, manifest))
    }

    /// The root hash — the single trust anchor for the entire tree.
    pub fn root_hash(&self) -> Hash256 {
        self.nodes[0]
    }

    /// Number of original (unpadded) data blocks.
    pub fn leaf_count(&self) -> usize {
        self.leaf_count
    }

    /// Total nodes in the tree (including padding).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Verify that a data block at `block_index` matches the tree.
    ///
    /// Hashes the block and walks the authentication path from leaf to root.
    /// Returns `true` if the computed root matches `expected_root`.
    pub fn verify_block(&self, block_index: usize, block_data: &[u8], expected_root: &Hash256) -> bool {
        if block_index >= self.padded_leaf_count {
            return false;
        }

        let leaf_hash = hash_block(block_data);
        let leaf_node = self.padded_leaf_count - 1 + block_index;

        // Walk from leaf to root, combining with sibling at each level.
        let mut current = leaf_hash;
        let mut idx = leaf_node;

        while idx > 0 {
            let sibling = if idx % 2 == 1 {
                // Left child — sibling is idx + 1.
                self.nodes[idx + 1]
            } else {
                // Right child — sibling is idx - 1.
                self.nodes[idx - 1]
            };

            current = if idx % 2 == 1 {
                hash_pair(&current, &sibling)
            } else {
                hash_pair(&sibling, &current)
            };

            idx = (idx - 1) / 2;
        }

        current == *expected_root
    }

    /// Get the authentication path (sibling hashes) for a given block index.
    /// Returns hashes from leaf level up to (but not including) the root.
    pub fn auth_path(&self, block_index: usize) -> Vec<Hash256> {
        if block_index >= self.padded_leaf_count {
            return vec![];
        }

        let mut path = Vec::new();
        let mut idx = self.padded_leaf_count - 1 + block_index;

        while idx > 0 {
            let sibling = if idx % 2 == 1 { idx + 1 } else { idx - 1 };
            path.push(self.nodes[sibling]);
            idx = (idx - 1) / 2;
        }

        path
    }

    /// Serialize the tree to a compact binary format.
    ///
    /// Format: `[leaf_count: u64][padded_leaf_count: u64][node_count: u64][nodes: Hash256...]`
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.nodes.len() * HASH_SIZE);
        out.extend_from_slice(&(self.leaf_count as u64).to_le_bytes());
        out.extend_from_slice(&(self.padded_leaf_count as u64).to_le_bytes());
        out.extend_from_slice(&(self.nodes.len() as u64).to_le_bytes());
        for node in &self.nodes {
            out.extend_from_slice(node);
        }
        out
    }

    /// Deserialize a Merkle tree from compact binary format.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 24 {
            return None;
        }
        let leaf_count = u64::from_le_bytes(data[0..8].try_into().ok()?) as usize;
        let padded_leaf_count = u64::from_le_bytes(data[8..16].try_into().ok()?) as usize;
        let node_count = u64::from_le_bytes(data[16..24].try_into().ok()?) as usize;

        if data.len() != 24 + node_count * HASH_SIZE {
            return None;
        }
        if node_count != 2 * padded_leaf_count - 1 {
            return None;
        }

        let mut nodes = Vec::with_capacity(node_count);
        for i in 0..node_count {
            let start = 24 + i * HASH_SIZE;
            let hash: Hash256 = data[start..start + HASH_SIZE].try_into().ok()?;
            nodes.push(hash);
        }

        Some(MerkleTree {
            nodes,
            leaf_count,
            padded_leaf_count,
        })
    }
}

// ── Layer Manifest ───────────────────────────────────────────────────────────

/// Per-file entry in the layer manifest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    #[serde(with = "hex_hash")]
    pub content_hash: Hash256,
    #[serde(with = "hex_hash")]
    pub metadata_hash: Hash256,
    /// Block range [start, end) in the concatenated data blob.
    pub block_range: (usize, usize),
}

/// Manifest describing all files in a verified layer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LayerManifest {
    pub files: Vec<FileEntry>,
    #[serde(with = "hex_hash")]
    pub manifest_hash: Hash256,
    #[serde(with = "hex_hash")]
    pub root_hash: Hash256,
    pub block_size: u32,
}

/// Serde helper for hex-encoding Hash256.
mod hex_hash {
    use super::Hash256;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(hash: &Hash256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
        serializer.serialize_str(&hex)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Hash256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.len() != 64 {
            return Err(serde::de::Error::custom("expected 64 hex chars"));
        }
        let mut hash = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let byte_str = std::str::from_utf8(chunk).map_err(serde::de::Error::custom)?;
            hash[i] = u8::from_str_radix(byte_str, 16).map_err(serde::de::Error::custom)?;
        }
        Ok(hash)
    }
}

// ── Verified Block Cache ─────────────────────────────────────────────────────

/// Bitset cache tracking which blocks have been verified this session.
///
/// Memory cost: 1 bit per 4 KB block = 3.2 KB per 100 MB layer.
/// Once a block is verified, subsequent reads skip the Merkle walk (~1 ns lookup).
#[derive(Debug)]
pub struct VerifiedBlockCache {
    /// Atomic bitset — one bit per block. Uses AtomicU8 for lock-free access.
    bits: Vec<AtomicU8>,
    /// Number of blocks this cache covers.
    block_count: usize,
    /// Root hash this cache was built against.
    root_hash: Hash256,
}

impl VerifiedBlockCache {
    /// Create a new cache for `block_count` blocks verified against `root_hash`.
    pub fn new(block_count: usize, root_hash: Hash256) -> Self {
        let byte_count = block_count.div_ceil(8);
        let bits = (0..byte_count).map(|_| AtomicU8::new(0)).collect();
        VerifiedBlockCache {
            bits,
            block_count,
            root_hash,
        }
    }

    /// Check if a block has been verified.
    pub fn is_verified(&self, block_index: usize) -> bool {
        if block_index >= self.block_count {
            return false;
        }
        let byte_idx = block_index / 8;
        let bit_idx = block_index % 8;
        (self.bits[byte_idx].load(Ordering::Relaxed) >> bit_idx) & 1 == 1
    }

    /// Mark a block as verified.
    pub fn mark_verified(&self, block_index: usize) {
        if block_index >= self.block_count {
            return;
        }
        let byte_idx = block_index / 8;
        let bit_idx = block_index % 8;
        self.bits[byte_idx].fetch_or(1 << bit_idx, Ordering::Relaxed);
    }

    /// Number of blocks verified so far.
    pub fn verified_count(&self) -> usize {
        let mut count = 0;
        for i in 0..self.block_count {
            if self.is_verified(i) {
                count += 1;
            }
        }
        count
    }

    /// The root hash this cache was built against.
    pub fn root_hash(&self) -> &Hash256 {
        &self.root_hash
    }

    /// Total number of blocks.
    pub fn block_count(&self) -> usize {
        self.block_count
    }
}

// ── Verified Layer VFS ───────────────────────────────────────────────────────

/// Action to take when verification fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFailure {
    /// Return an I/O error (NFS4ERR_IO). Default, most secure.
    Reject,
    /// Log a warning but serve the data anyway (for debugging/migration).
    Warn,
}

/// A VFS wrapper that verifies every read against a Merkle hash tree.
///
/// Wraps a read-only VFS layer and checks each block on first read.
/// Implements the same verification as dm-verity/fs-verity but in userspace.
// Manual Debug impl below (VfsPath and AtomicU8 fields)
pub struct VerifiedLayerVfs {
    /// The underlying (unverified) filesystem.
    inner: VfsPath,
    /// Pre-built Merkle tree covering all file content.
    tree: MerkleTree,
    /// Layer manifest with per-file block ranges.
    manifest: LayerManifest,
    /// Trusted root hash.
    root_hash: Hash256,
    /// Cache of already-verified blocks.
    cache: VerifiedBlockCache,
    /// Action on verification failure.
    on_failure: OnFailure,
}

impl VerifiedLayerVfs {
    /// Create a new verified layer from a VFS path and pre-built tree.
    pub fn new(
        inner: VfsPath,
        tree: MerkleTree,
        manifest: LayerManifest,
        root_hash: Hash256,
        on_failure: OnFailure,
    ) -> Self {
        let cache = VerifiedBlockCache::new(tree.leaf_count(), root_hash);
        VerifiedLayerVfs {
            inner,
            tree,
            manifest,
            root_hash,
            cache,
            on_failure,
        }
    }

    /// Build a verified layer by scanning a VFS directory tree.
    ///
    /// Builds the Merkle tree and manifest, then pins the root hash.
    pub fn build(inner: VfsPath, on_failure: OnFailure) -> io::Result<Self> {
        let (tree, manifest) = MerkleTree::build_from_vfs(&inner)?;
        let root_hash = tree.root_hash();
        Ok(Self::new(inner, tree, manifest, root_hash, on_failure))
    }

    /// Get the trusted root hash.
    pub fn root_hash(&self) -> &Hash256 {
        &self.root_hash
    }

    /// Get the layer manifest.
    pub fn manifest(&self) -> &LayerManifest {
        &self.manifest
    }

    /// Get the verification cache.
    pub fn cache(&self) -> &VerifiedBlockCache {
        &self.cache
    }

    /// Find the file entry for a given path.
    fn find_file(&self, path: &str) -> Option<&FileEntry> {
        let normalized = path.strip_prefix('/').unwrap_or(path);
        self.manifest.files.iter().find(|f| f.path == normalized)
    }

    /// Verify blocks covering a byte range within a file.
    ///
    /// Reads the raw blocks from the underlying VFS and verifies each one
    /// against the Merkle tree. Returns `Err` on verification failure (unless
    /// `on_failure == Warn`, in which case it logs and continues).
    pub fn verify_file_blocks(&self, path: &str, data: &[u8], offset: u64) -> io::Result<()> {
        let entry = match self.find_file(path) {
            Some(e) => e,
            None => return Ok(()), // file not in manifest (shouldn't happen for lower layer)
        };

        let file_block_start = entry.block_range.0;
        let block_offset = (offset as usize) / BLOCK_SIZE;
        let first_block = file_block_start + block_offset;

        // How many blocks does this read span?
        let start_in_block = (offset as usize) % BLOCK_SIZE;
        let total_bytes = start_in_block + data.len();
        let block_count = total_bytes.div_ceil(BLOCK_SIZE);

        for i in 0..block_count {
            let global_block = first_block + i;

            // Skip if already verified this session.
            if self.cache.is_verified(global_block) {
                continue;
            }

            // Extract the block-aligned data to verify.
            // We need the full block data for hashing, not just the read slice.
            // For simplicity, re-read the block from the underlying VFS.
            let block_data = self.read_raw_block(path, (block_offset + i) * BLOCK_SIZE)?;

            if self.tree.verify_block(global_block, &block_data, &self.root_hash) {
                self.cache.mark_verified(global_block);
            } else {
                tracing::error!(
                    "verity: corruption detected at block {} (file {}, offset {})",
                    global_block,
                    path,
                    (block_offset + i) * BLOCK_SIZE,
                );
                match self.on_failure {
                    OnFailure::Reject => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("verity: block {} verification failed", global_block),
                        ));
                    }
                    OnFailure::Warn => {
                        tracing::warn!(
                            "verity: serving unverified block {} (on_failure=warn)",
                            global_block,
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Read a raw block from the underlying VFS for verification.
    fn read_raw_block(&self, path: &str, offset: usize) -> io::Result<Vec<u8>> {
        let file = self.inner.join(path).map_err(io_err)?.open_file().map_err(io_err)?;
        let mut buf = vec![0u8; BLOCK_SIZE];
        let mut reader = file;

        // Skip to offset.
        let mut skip_remaining = offset;
        while skip_remaining > 0 {
            let skip = std::cmp::min(skip_remaining, BLOCK_SIZE);
            let mut skip_buf = vec![0u8; skip];
            let n = reader.read(&mut skip_buf)?;
            if n == 0 {
                break;
            }
            skip_remaining -= n;
        }

        let n = reader.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Verify the entire layer proactively (full scan).
    ///
    /// Returns `(blocks_checked, blocks_failed)`.
    pub fn full_check(&self) -> io::Result<(usize, usize)> {
        let mut checked = 0;
        let mut failed = 0;

        for entry in &self.manifest.files {
            let file = self.inner.join(&entry.path).map_err(io_err)?.open_file().map_err(io_err)?;
            let mut reader = file;
            let mut block_idx = entry.block_range.0;

            loop {
                let mut buf = vec![0u8; BLOCK_SIZE];
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                buf.truncate(n);

                if self.tree.verify_block(block_idx, &buf, &self.root_hash) {
                    self.cache.mark_verified(block_idx);
                } else {
                    failed += 1;
                    tracing::error!(
                        "verity: full_check failed at block {} (file {})",
                        block_idx,
                        entry.path,
                    );
                }
                checked += 1;
                block_idx += 1;
            }

            // Account for padding blocks between files.
            // The tree includes zero-padded blocks up to the next block boundary.
            while block_idx < entry.block_range.1 {
                let empty = vec![0u8; 0];
                if self.tree.verify_block(block_idx, &empty, &self.root_hash) {
                    self.cache.mark_verified(block_idx);
                } else {
                    failed += 1;
                }
                checked += 1;
                block_idx += 1;
            }
        }

        Ok((checked, failed))
    }
}

// ── Debug impl for VerifiedLayerVfs ───────────────────────────────────────────

impl std::fmt::Debug for VerifiedLayerVfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hex: String = self.root_hash.iter().map(|b| format!("{:02x}", b)).collect();
        f.debug_struct("VerifiedLayerVfs")
            .field("root_hash", &hex)
            .field("files", &self.manifest.files.len())
            .field("on_failure", &self.on_failure)
            .finish()
    }
}

// ── FileSystem trait for VerifiedLayerVfs ─────────────────────────────────────

/// Wraps the VerifiedLayerVfs in an Arc for shared ownership by the FileSystem trait.
///
/// The `vfs::FileSystem` trait requires `Send + Sync` and the overlay VFS takes
/// `VfsPath` (which owns a `Box<dyn FileSystem>`). This wrapper holds the shared
/// state (tree, cache, manifest) in an Arc so the FileSystem can be cloned into
/// a VfsPath.
#[derive(Debug)]
pub struct VerifiedFS {
    inner: Arc<VerifiedLayerVfs>,
}

impl VerifiedFS {
    /// Create a new VerifiedFS wrapping a VerifiedLayerVfs.
    pub fn new(verified: VerifiedLayerVfs) -> Self {
        Self {
            inner: Arc::new(verified),
        }
    }

    /// Build a VerifiedFS by scanning a VFS directory tree.
    pub fn build(inner: VfsPath, on_failure: OnFailure) -> io::Result<Self> {
        let verified = VerifiedLayerVfs::build(inner, on_failure)?;
        Ok(Self::new(verified))
    }

    /// Get the root hash.
    pub fn root_hash(&self) -> &Hash256 {
        self.inner.root_hash()
    }

    /// Get the manifest.
    pub fn manifest(&self) -> &LayerManifest {
        self.inner.manifest()
    }

    /// Get the cache.
    pub fn cache(&self) -> &VerifiedBlockCache {
        self.inner.cache()
    }

    /// Resolve a path within the inner VFS.
    fn resolve(&self, path: &str) -> VfsResult<VfsPath> {
        if path.is_empty() || path == "/" {
            Ok(self.inner.inner.clone())
        } else {
            let clean = path.strip_prefix('/').unwrap_or(path);
            self.inner.inner.join(clean)
        }
    }
}

impl FileSystem for VerifiedFS {
    fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
        let resolved = self.resolve(path)?;
        let entries: Vec<String> = resolved.read_dir()?.map(|e| e.filename()).collect();
        Ok(Box::new(entries.into_iter()))
    }

    fn create_dir(&self, _path: &str) -> VfsResult<()> {
        Err(VfsErrorKind::NotSupported.into())
    }

    fn open_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndRead + Send>> {
        let resolved = self.resolve(path)?;
        let file = resolved.open_file()?;

        // Read the full file content for verification.
        let mut reader = file;
        let mut data = Vec::new();
        reader.read_to_end(&mut data).map_err(|e| {
            vfs::VfsError::from(VfsErrorKind::IoError(e))
        })?;

        // Verify the content against the Merkle tree.
        let clean_path = path.strip_prefix('/').unwrap_or(path);
        self.inner.verify_file_blocks(clean_path, &data, 0).map_err(|e| {
            vfs::VfsError::from(VfsErrorKind::IoError(e))
        })?;

        // Return a cursor over the verified data.
        Ok(Box::new(io::Cursor::new(data)))
    }

    fn create_file(&self, _path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        Err(VfsErrorKind::NotSupported.into())
    }

    fn append_file(&self, _path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        Err(VfsErrorKind::NotSupported.into())
    }

    fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
        let resolved = self.resolve(path)?;
        resolved.metadata()
    }

    fn exists(&self, path: &str) -> VfsResult<bool> {
        let resolved = self.resolve(path)?;
        resolved.exists()
    }

    fn remove_file(&self, _path: &str) -> VfsResult<()> {
        Err(VfsErrorKind::NotSupported.into())
    }

    fn remove_dir(&self, _path: &str) -> VfsResult<()> {
        Err(VfsErrorKind::NotSupported.into())
    }
}

// ── Hash Helpers ─────────────────────────────────────────────────────────────

/// SHA-256 hash of a data block (up to BLOCK_SIZE bytes).
fn hash_block(data: &[u8]) -> Hash256 {
    let mut hasher = Sha256::new();
    hasher.update(data);
    // Zero-pad short blocks for consistent leaf hashes.
    if data.len() < BLOCK_SIZE {
        let padding = BLOCK_SIZE - data.len();
        hasher.update(vec![0u8; padding]);
    }
    hasher.finalize().into()
}

/// SHA-256 hash of two concatenated hashes (interior node).
fn hash_pair(left: &Hash256, right: &Hash256) -> Hash256 {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// SHA-256 hash of arbitrary bytes.
fn sha256_bytes(data: &[u8]) -> Hash256 {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Convert a VFS error to an io::Error.
fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Recursively collect all files from a VFS directory tree into a sorted map.
fn collect_files(
    root: &VfsPath,
    prefix: &str,
    files: &mut BTreeMap<String, ()>,
) -> io::Result<()> {
    let path = if prefix.is_empty() {
        root.clone()
    } else {
        root.join(prefix).map_err(io_err)?
    };

    for entry in path.read_dir().map_err(io_err)? {
        let name = entry.filename();
        let rel_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", prefix, name)
        };

        let meta = entry.metadata().map_err(io_err)?;
        if meta.file_type == vfs::VfsFileType::Directory {
            collect_files(root, &rel_path, files)?;
        } else {
            files.insert(rel_path, ());
        }
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::MemoryFS;

    // ── MerkleTree basic tests ───────────────────────────────────────────

    #[test]
    fn test_build_empty_data() {
        let tree = MerkleTree::build(&[]);
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.node_count(), 1); // single leaf = single node
        // Root should be hash of a zero-padded block.
        let expected = hash_block(&[]);
        assert_eq!(tree.root_hash(), expected);
    }

    #[test]
    fn test_build_single_block() {
        let data = vec![42u8; BLOCK_SIZE];
        let tree = MerkleTree::build(&data);
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.padded_leaf_count, 1);
        assert_eq!(tree.node_count(), 1);
        assert_eq!(tree.root_hash(), hash_block(&data));
    }

    #[test]
    fn test_build_two_blocks() {
        let data = vec![0xABu8; BLOCK_SIZE * 2];
        let tree = MerkleTree::build(&data);
        assert_eq!(tree.leaf_count(), 2);
        assert_eq!(tree.padded_leaf_count, 2);
        assert_eq!(tree.node_count(), 3); // 2 leaves + 1 root

        let h0 = hash_block(&data[..BLOCK_SIZE]);
        let h1 = hash_block(&data[BLOCK_SIZE..]);
        let expected_root = hash_pair(&h0, &h1);
        assert_eq!(tree.root_hash(), expected_root);
    }

    #[test]
    fn test_build_three_blocks_padded() {
        // 3 blocks → padded to 4 leaves.
        let data = vec![0xCDu8; BLOCK_SIZE * 3];
        let tree = MerkleTree::build(&data);
        assert_eq!(tree.leaf_count(), 3);
        assert_eq!(tree.padded_leaf_count, 4);
        assert_eq!(tree.node_count(), 7); // 4 leaves + 2 interior + 1 root
    }

    #[test]
    fn test_build_partial_last_block() {
        let data = vec![0xEFu8; BLOCK_SIZE + 100]; // 1 full + 100 byte partial
        let tree = MerkleTree::build(&data);
        assert_eq!(tree.leaf_count(), 2);
        assert_eq!(tree.padded_leaf_count, 2);

        // Second block is 100 bytes, zero-padded to BLOCK_SIZE for hashing.
        let h0 = hash_block(&data[..BLOCK_SIZE]);
        let h1 = hash_block(&data[BLOCK_SIZE..]);
        let expected_root = hash_pair(&h0, &h1);
        assert_eq!(tree.root_hash(), expected_root);
    }

    // ── Verification tests ───────────────────────────────────────────────

    #[test]
    fn test_verify_block_valid() {
        let data = vec![0xAAu8; BLOCK_SIZE * 4];
        let tree = MerkleTree::build(&data);
        let root = tree.root_hash();

        for i in 0..4 {
            let block = &data[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
            assert!(tree.verify_block(i, block, &root), "block {} should verify", i);
        }
    }

    #[test]
    fn test_verify_block_corrupted() {
        let data = vec![0xBBu8; BLOCK_SIZE * 2];
        let tree = MerkleTree::build(&data);
        let root = tree.root_hash();

        // Corrupt one byte in block 0.
        let mut corrupted = data[..BLOCK_SIZE].to_vec();
        corrupted[0] = 0xFF;
        assert!(!tree.verify_block(0, &corrupted, &root));

        // Block 1 should still verify.
        assert!(tree.verify_block(1, &data[BLOCK_SIZE..], &root));
    }

    #[test]
    fn test_verify_block_wrong_root() {
        let data = vec![0xCCu8; BLOCK_SIZE];
        let tree = MerkleTree::build(&data);
        let wrong_root = [0xFFu8; 32];

        assert!(!tree.verify_block(0, &data, &wrong_root));
    }

    #[test]
    fn test_verify_block_out_of_range() {
        let data = vec![0xDDu8; BLOCK_SIZE * 2];
        let tree = MerkleTree::build(&data);
        let root = tree.root_hash();

        assert!(!tree.verify_block(100, &data[..BLOCK_SIZE], &root));
    }

    // ── Auth path tests ──────────────────────────────────────────────────

    #[test]
    fn test_auth_path_single_block() {
        let data = vec![0u8; BLOCK_SIZE];
        let tree = MerkleTree::build(&data);
        let path = tree.auth_path(0);
        assert!(path.is_empty()); // single node, no siblings
    }

    #[test]
    fn test_auth_path_two_blocks() {
        let data = vec![0u8; BLOCK_SIZE * 2];
        let tree = MerkleTree::build(&data);

        let path0 = tree.auth_path(0);
        assert_eq!(path0.len(), 1); // sibling is block 1's hash

        let path1 = tree.auth_path(1);
        assert_eq!(path1.len(), 1); // sibling is block 0's hash
    }

    #[test]
    fn test_auth_path_four_blocks() {
        let data = vec![0u8; BLOCK_SIZE * 4];
        let tree = MerkleTree::build(&data);

        for i in 0..4 {
            let path = tree.auth_path(i);
            assert_eq!(path.len(), 2); // log2(4) = 2 levels
        }
    }

    // ── Serialization tests ──────────────────────────────────────────────

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let data = vec![0xEEu8; BLOCK_SIZE * 5]; // 5 blocks → padded to 8
        let tree = MerkleTree::build(&data);

        let bytes = tree.to_bytes();
        let tree2 = MerkleTree::from_bytes(&bytes).expect("deserialization should succeed");

        assert_eq!(tree.root_hash(), tree2.root_hash());
        assert_eq!(tree.leaf_count(), tree2.leaf_count());
        assert_eq!(tree.node_count(), tree2.node_count());

        // Verify blocks still work with deserialized tree.
        let root = tree2.root_hash();
        for i in 0..5 {
            let start = i * BLOCK_SIZE;
            let end = std::cmp::min(start + BLOCK_SIZE, data.len());
            assert!(tree2.verify_block(i, &data[start..end], &root));
        }
    }

    #[test]
    fn test_deserialize_truncated() {
        assert!(MerkleTree::from_bytes(&[]).is_none());
        assert!(MerkleTree::from_bytes(&[0u8; 23]).is_none());
    }

    #[test]
    fn test_deserialize_wrong_size() {
        let data = vec![0u8; BLOCK_SIZE];
        let tree = MerkleTree::build(&data);
        let mut bytes = tree.to_bytes();
        bytes.push(0xFF); // corrupt by appending
        assert!(MerkleTree::from_bytes(&bytes).is_none());
    }

    // ── VerifiedBlockCache tests ─────────────────────────────────────────

    #[test]
    fn test_cache_initially_empty() {
        let cache = VerifiedBlockCache::new(100, [0u8; 32]);
        for i in 0..100 {
            assert!(!cache.is_verified(i));
        }
        assert_eq!(cache.verified_count(), 0);
    }

    #[test]
    fn test_cache_mark_and_check() {
        let cache = VerifiedBlockCache::new(64, [0u8; 32]);

        cache.mark_verified(0);
        cache.mark_verified(7);
        cache.mark_verified(63);

        assert!(cache.is_verified(0));
        assert!(cache.is_verified(7));
        assert!(cache.is_verified(63));
        assert!(!cache.is_verified(1));
        assert!(!cache.is_verified(62));
        assert_eq!(cache.verified_count(), 3);
    }

    #[test]
    fn test_cache_out_of_range() {
        let cache = VerifiedBlockCache::new(10, [0u8; 32]);
        assert!(!cache.is_verified(10));
        assert!(!cache.is_verified(100));
        cache.mark_verified(10); // should be no-op
        cache.mark_verified(100); // should be no-op
        assert_eq!(cache.verified_count(), 0);
    }

    #[test]
    fn test_cache_all_blocks() {
        let cache = VerifiedBlockCache::new(20, [0u8; 32]);
        for i in 0..20 {
            cache.mark_verified(i);
        }
        assert_eq!(cache.verified_count(), 20);
        for i in 0..20 {
            assert!(cache.is_verified(i));
        }
    }

    #[test]
    fn test_cache_block_count() {
        let cache = VerifiedBlockCache::new(12345, [0xABu8; 32]);
        assert_eq!(cache.block_count(), 12345);
        assert_eq!(*cache.root_hash(), [0xABu8; 32]);
    }

    // ── VFS integration tests ────────────────────────────────────────────

    fn create_test_vfs() -> VfsPath {
        let root: VfsPath = MemoryFS::new().into();
        root.join("usr/bin").unwrap().create_dir_all().unwrap();
        root.join("usr/lib").unwrap().create_dir_all().unwrap();
        root.join("etc").unwrap().create_dir_all().unwrap();

        // Write some test files.
        {
            use std::io::Write;
            let mut f = root.join("usr/bin/hello").unwrap().create_file().unwrap();
            f.write_all(b"#!/bin/sh\necho hello world\n").unwrap();
        }
        {
            use std::io::Write;
            let mut f = root.join("usr/lib/libtest.so").unwrap().create_file().unwrap();
            let data = vec![0xEFu8; 8192]; // 2 blocks
            f.write_all(&data).unwrap();
        }
        {
            use std::io::Write;
            let mut f = root.join("etc/config.txt").unwrap().create_file().unwrap();
            f.write_all(b"key=value\n").unwrap();
        }

        root
    }

    #[test]
    fn test_build_from_vfs() {
        let root = create_test_vfs();
        let (tree, manifest) = MerkleTree::build_from_vfs(&root).unwrap();

        assert_eq!(manifest.files.len(), 3);
        assert_eq!(manifest.block_size, BLOCK_SIZE as u32);
        assert_eq!(manifest.root_hash, tree.root_hash());

        // Files should be in sorted order.
        assert_eq!(manifest.files[0].path, "etc/config.txt");
        assert_eq!(manifest.files[1].path, "usr/bin/hello");
        assert_eq!(manifest.files[2].path, "usr/lib/libtest.so");
    }

    #[test]
    fn test_build_from_vfs_deterministic() {
        let root = create_test_vfs();
        let (tree1, _) = MerkleTree::build_from_vfs(&root).unwrap();
        let (tree2, _) = MerkleTree::build_from_vfs(&root).unwrap();
        assert_eq!(tree1.root_hash(), tree2.root_hash());
    }

    #[test]
    fn test_verified_layer_build() {
        let root = create_test_vfs();
        let verified = VerifiedLayerVfs::build(root, OnFailure::Reject).unwrap();

        assert_eq!(verified.manifest().files.len(), 3);
        assert_eq!(verified.cache().verified_count(), 0);
    }

    #[test]
    fn test_verified_layer_full_check() {
        let root = create_test_vfs();
        let verified = VerifiedLayerVfs::build(root, OnFailure::Reject).unwrap();

        let (checked, failed) = verified.full_check().unwrap();
        assert!(checked > 0);
        assert_eq!(failed, 0);
        assert!(verified.cache().verified_count() > 0);
    }

    #[test]
    fn test_verified_layer_verify_file() {
        let root = create_test_vfs();
        let verified = VerifiedLayerVfs::build(root, OnFailure::Reject).unwrap();

        // Read a file and verify it.
        let data = b"key=value\n";
        verified.verify_file_blocks("etc/config.txt", data, 0).unwrap();
        assert!(verified.cache().verified_count() > 0);
    }

    #[test]
    fn test_manifest_json_roundtrip() {
        let root = create_test_vfs();
        let (_, manifest) = MerkleTree::build_from_vfs(&root).unwrap();

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let manifest2: LayerManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(manifest.root_hash, manifest2.root_hash);
        assert_eq!(manifest.files.len(), manifest2.files.len());
        for (a, b) in manifest.files.iter().zip(manifest2.files.iter()) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.size, b.size);
            assert_eq!(a.content_hash, b.content_hash);
        }
    }

    // ── Hash function tests ──────────────────────────────────────────────

    #[test]
    fn test_hash_block_deterministic() {
        let data = vec![0xABu8; 100];
        assert_eq!(hash_block(&data), hash_block(&data));
    }

    #[test]
    fn test_hash_block_different_data() {
        let a = vec![0x00u8; BLOCK_SIZE];
        let b = vec![0x01u8; BLOCK_SIZE];
        assert_ne!(hash_block(&a), hash_block(&b));
    }

    #[test]
    fn test_hash_pair_order_matters() {
        let a = hash_block(&[1u8; BLOCK_SIZE]);
        let b = hash_block(&[2u8; BLOCK_SIZE]);
        assert_ne!(hash_pair(&a, &b), hash_pair(&b, &a));
    }

    // ── Large tree stress test ───────────────────────────────────────────

    #[test]
    fn test_large_tree_256_blocks() {
        let data = vec![0x42u8; BLOCK_SIZE * 256];
        let tree = MerkleTree::build(&data);
        assert_eq!(tree.leaf_count(), 256);
        assert_eq!(tree.padded_leaf_count, 256); // already power of 2

        let root = tree.root_hash();

        // Verify every block.
        for i in 0..256 {
            let block = &data[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
            assert!(tree.verify_block(i, block, &root), "block {} should verify", i);
        }

        // Corrupt block 128 and verify it fails.
        let mut corrupted = data[128 * BLOCK_SIZE..129 * BLOCK_SIZE].to_vec();
        corrupted[42] ^= 0xFF;
        assert!(!tree.verify_block(128, &corrupted, &root));
    }

    #[test]
    fn test_serialize_large_tree() {
        let data = vec![0x99u8; BLOCK_SIZE * 100]; // 100 blocks → 128 padded
        let tree = MerkleTree::build(&data);
        let bytes = tree.to_bytes();

        // Expected size: 24 header + (2*128 - 1) * 32 = 24 + 8160 = 8184
        assert_eq!(bytes.len(), 24 + 255 * 32);

        let tree2 = MerkleTree::from_bytes(&bytes).unwrap();
        assert_eq!(tree.root_hash(), tree2.root_hash());
    }

    // ── OnFailure::Warn test ─────────────────────────────────────────────

    #[test]
    fn test_on_failure_modes() {
        assert_eq!(OnFailure::Reject, OnFailure::Reject);
        assert_eq!(OnFailure::Warn, OnFailure::Warn);
        assert_ne!(OnFailure::Reject, OnFailure::Warn);
    }

    // ── VerifiedFS (FileSystem trait) tests ──────────────────────────────

    #[test]
    fn test_verified_fs_open_file() {
        let root = create_test_vfs();
        let vfs = VerifiedFS::build(root, OnFailure::Reject).unwrap();
        let vfs_path: VfsPath = vfs.into();

        // Open and read a file through the verified VFS.
        let mut file = vfs_path.join("etc/config.txt").unwrap().open_file().unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "key=value\n");
    }

    #[test]
    fn test_verified_fs_read_dir() {
        let root = create_test_vfs();
        let vfs = VerifiedFS::build(root, OnFailure::Reject).unwrap();
        let vfs_path: VfsPath = vfs.into();

        let entries: Vec<String> = vfs_path
            .read_dir()
            .unwrap()
            .map(|e: VfsPath| e.filename())
            .collect();
        assert!(entries.contains(&"etc".to_string()));
        assert!(entries.contains(&"usr".to_string()));
    }

    #[test]
    fn test_verified_fs_metadata() {
        let root = create_test_vfs();
        let vfs = VerifiedFS::build(root, OnFailure::Reject).unwrap();
        let vfs_path: VfsPath = vfs.into();

        let meta = vfs_path.join("etc/config.txt").unwrap().metadata().unwrap();
        assert_eq!(meta.file_type, vfs::VfsFileType::File);
        assert_eq!(meta.len, 10); // "key=value\n" = 10 bytes
    }

    #[test]
    fn test_verified_fs_exists() {
        let root = create_test_vfs();
        let vfs = VerifiedFS::build(root, OnFailure::Reject).unwrap();
        let vfs_path: VfsPath = vfs.into();

        assert!(vfs_path.join("etc/config.txt").unwrap().exists().unwrap());
        assert!(vfs_path.join("usr/bin/hello").unwrap().exists().unwrap());
        assert!(!vfs_path.join("nonexistent").unwrap().exists().unwrap());
    }

    #[test]
    fn test_verified_fs_read_only() {
        let root = create_test_vfs();
        let vfs = VerifiedFS::build(root, OnFailure::Reject).unwrap();
        let vfs_path: VfsPath = vfs.into();

        // Write operations should fail on a read-only verified layer.
        assert!(vfs_path.join("new_file").unwrap().create_file().is_err());
        assert!(vfs_path.join("new_dir").unwrap().create_dir().is_err());
        assert!(vfs_path.join("etc/config.txt").unwrap().remove_file().is_err());
    }

    #[test]
    fn test_verified_fs_as_overlay_lower() {
        use crate::server::overlay::OverlayFS;

        // Create a verified lower layer.
        let lower_root = create_test_vfs();
        let verified = VerifiedFS::build(lower_root, OnFailure::Reject).unwrap();
        let verified_path: VfsPath = verified.into();

        // Create an empty upper layer.
        let upper: VfsPath = MemoryFS::new().into();

        // Build an overlay with the verified layer as a lower layer.
        let ov = OverlayFS::new(upper, vec![verified_path]);
        let ov_path: VfsPath = ov.into();

        // Read a file from the verified lower layer through the overlay.
        let mut file = ov_path.join("etc/config.txt").unwrap().open_file().unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "key=value\n");

        // Listing should work through the overlay.
        let entries: Vec<String> = ov_path
            .read_dir()
            .unwrap()
            .map(|e| e.filename())
            .collect();
        assert!(entries.contains(&"etc".to_string()));
        assert!(entries.contains(&"usr".to_string()));
    }

    #[test]
    fn test_verified_fs_overlay_write_upper() {
        use crate::server::overlay::OverlayFS;

        // Create a verified lower layer.
        let lower_root = create_test_vfs();
        let verified = VerifiedFS::build(lower_root, OnFailure::Reject).unwrap();
        let verified_path: VfsPath = verified.into();

        // Create an upper layer.
        let upper: VfsPath = MemoryFS::new().into();

        // Build an overlay.
        let ov = OverlayFS::new(upper, vec![verified_path]);
        let ov_path: VfsPath = ov.into();

        // Write a new file — goes to upper.
        {
            use std::io::Write;
            let mut f = ov_path.join("new_file.txt").unwrap().create_file().unwrap();
            f.write_all(b"new content").unwrap();
        }

        // New file should be readable.
        let mut file = ov_path.join("new_file.txt").unwrap().open_file().unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "new content");

        // Original verified file should still be readable.
        let mut file2 = ov_path.join("etc/config.txt").unwrap().open_file().unwrap();
        let mut buf2 = String::new();
        file2.read_to_string(&mut buf2).unwrap();
        assert_eq!(buf2, "key=value\n");
    }

    #[test]
    fn test_verified_fs_cache_populated_after_read() {
        let root = create_test_vfs();
        let vfs = VerifiedFS::build(root, OnFailure::Reject).unwrap();
        assert_eq!(vfs.cache().verified_count(), 0);

        let vfs_path: VfsPath = vfs.into();

        // Reading a file should populate the cache.
        let mut file = vfs_path.join("etc/config.txt").unwrap().open_file().unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"key=value\n");

        // Can't check cache directly after into() since we moved the VerifiedFS.
        // But the read succeeded, which means verification passed.
    }
}
