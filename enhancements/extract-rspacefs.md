# Extract Overlay + Verity into Standalone `rspacefs` Project

**Status:** proposed — 2026-05-19
**Owner:** glennswest
**Scope:** Move the userspace OverlayFS implementation and the dm-verity Merkle tree out of nextnfs into a new standalone crate `rspacefs`. nextnfs will **not** depend on rspacefs; the two are completely independent.

## Motivation

The pure-Rust OverlayFS in `nfs/src/server/overlay.rs` (1031 LOC) is currently coupled to nextnfs only because that's where it was developed. It is a general-purpose layered-rootfs primitive — equally useful for direct container rootfs assembly, image-build tooling, and any high-performance layered-storage scenario.

The current path forces every consumer through an NFSv4 protocol hop just to use the overlay logic, which is unacceptable for performance-critical workloads (container rootfs assembly, image-build hot paths). NFS belongs in front of *file serving*, not in front of *the layer-merge primitive itself*.

The dm-verity Merkle tree (`nfs/src/server/verity.rs`, 1389 LOC) is in the same boat — it's a generic integrity-verification primitive that doesn't need NFS to be useful.

## Goal

Two completely independent codebases:

- **nextnfs** — NFSv4 server, no overlay/verity code at all.
- **rspacefs** — userspace overlay + verity library + (optional) CLI.

Neither depends on the other. nextnfs's existing in-tree use of OverlayFS is removed; it falls back to flat exports or is taught to embed rspacefs separately if needed in the future.

## New project layout

```
/Volumes/minihome/gwest/projects/rspacefs/
├── Cargo.toml                 # workspace
├── README.md
├── CLAUDE.md                  # project rules per cross-project standard
├── CHANGELOG.md
├── crates/
│   ├── rspacefs-core/         # the OverlayFS impl (was overlay.rs)
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs         # re-exports OverlayFS
│   │   └── src/overlay.rs     # ← moved from nextnfs/nfs/src/server/overlay.rs
│   └── rspacefs-verity/       # dm-verity Merkle (was verity.rs)
│       ├── Cargo.toml
│       ├── src/lib.rs
│       └── src/verity.rs      # ← moved from nextnfs/nfs/src/server/verity.rs
└── cli/                       # optional standalone CLI
    └── src/main.rs
```

Keep the `vfs::FileSystem` trait as the abstraction boundary — it's already what overlay.rs implements, lets consumers plug in their own backing storage (real disk, memory, network), and doesn't force NFS.

## Migration steps

1. **Create the new project skeleton** at `/Volumes/minihome/gwest/projects/rspacefs/`:
   - Workspace `Cargo.toml`
   - `crates/rspacefs-core/Cargo.toml` with deps: `vfs = "0.12"` (matches nextnfs's current pin)
   - `crates/rspacefs-verity/Cargo.toml` with deps: `sha2`
   - CLAUDE.md, CHANGELOG.md per cross-project standard
   - `git init`, push to a new repo

2. **Move files verbatim:**
   - `nextnfs/nfs/src/server/overlay.rs` → `rspacefs/crates/rspacefs-core/src/overlay.rs`
   - `nextnfs/nfs/src/server/verity.rs` → `rspacefs/crates/rspacefs-verity/src/verity.rs`
   - Adjust `mod` declarations (`pub mod overlay;` in lib.rs)
   - Make `OverlayFS` and helpers `pub`

3. **Bring tests along.** Any inline `#[cfg(test)]` blocks move with the code. If there are external tests in `nfs/tests/` that touch overlay/verity, move those too.

4. **Update nextnfs** — drop the overlay/verity references entirely:
   - Remove `mod overlay;` and `mod verity;` from `nfs/src/server/mod.rs`
   - Remove overlay-backed export type from `export_manager.rs` (or leave a stub returning "unsupported export type" so config compat doesn't crash).
   - Update `CHANGELOG.md` — note the removal.
   - Update `packaging/nextnfs.spec` — drop any overlay/verity-specific references.
   - Update `enhancements/fedora-rhel-integration.md` if it references overlay.

5. **CI** — extend ci-test.sh in rspacefs to run `cargo test -p rspacefs-core -p rspacefs-verity`.

## Performance posture

- **No async, no NFS, no network.** All operations are synchronous file I/O against the configured `vfs::FileSystem` backend. Callers control concurrency.
- **`vfs` is a thin trait.** It doesn't add overhead vs. direct `std::fs` when backed by a real filesystem.
- **Whiteout handling** is a directory scan — acceptable for typical OCI layer counts (< 20). If profiling shows hotspots, follow up with whiteout-set caching per directory.
- **Copy-up on write** does a full file copy when a lower-layer file is first written. Already the standard overlayfs cost; matches kernel semantics.

## Non-goals (initial release)

- Kernel module / FUSE driver — userspace only, library form.
- Multi-writer concurrency safety beyond what `vfs::FileSystem` itself provides.
- Sparse / reflink optimizations for copy-up — measure first.

## Open questions

- Should `rspacefs-core` and `rspacefs-verity` be one crate with feature flags, or stay two crates? Two is cleaner (verity has a sha2 dep; consumers who only want overlay shouldn't pay for it). Default to two.
- Workspace vs. flat crate? Two crates → workspace. Use a workspace.
- Repo: separate GitHub repo `glennswest/rspacefs`? Yes — matches the "totally split" intent.

## Files referenced

- `nextnfs/nfs/src/server/overlay.rs` (1031 LOC) — moves out
- `nextnfs/nfs/src/server/verity.rs` (1389 LOC) — moves out
- `nextnfs/nfs/src/server/mod.rs` — drop module decls
- `nextnfs/nfs/src/server/export_manager.rs` — drop overlay export type
- `nextnfs/packaging/nextnfs.spec` — review for overlay refs
- `nextnfs/CHANGELOG.md` — record the removal
