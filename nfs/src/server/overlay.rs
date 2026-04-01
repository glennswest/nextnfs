//! OverlayFS-like virtual filesystem backend for NextNFS.
//!
//! Merges multiple read-only lower layers with a writable upper layer,
//! implementing the `vfs::FileSystem` trait. Supports whiteout markers
//! (OCI image spec), copy-up on write, and merged directory listings.

use std::collections::HashSet;
use std::fmt;
use std::io::{Read, Write};

use vfs::error::VfsErrorKind;
use vfs::{FileSystem, VfsFileType, VfsMetadata, VfsPath, VfsResult};

/// Prefix for whiteout markers (OCI image spec).
const WHITEOUT_PREFIX: &str = ".wh.";

/// Opaque whiteout marker — blocks all lower layer entries in a directory.
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// OverlayFS virtual filesystem.
///
/// Merges a writable `upper` layer with zero or more read-only `lower` layers.
/// Write operations go to `upper`. Read operations check `upper` first, then
/// walk lower layers top-down. Deleted entries are tracked via whiteout markers.
pub struct OverlayFS {
    upper: VfsPath,
    /// Lower layers ordered top-down (index 0 is highest priority).
    lower: Vec<VfsPath>,
}

impl fmt::Debug for OverlayFS {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OverlayFS")
            .field("upper", &self.upper.as_str())
            .field("lower_count", &self.lower.len())
            .finish()
    }
}

impl OverlayFS {
    /// Create a new overlay filesystem.
    ///
    /// - `upper`: writable layer (all writes go here)
    /// - `lower`: read-only layers, ordered top-down (index 0 = highest priority)
    pub fn new(upper: VfsPath, lower: Vec<VfsPath>) -> Self {
        Self { upper, lower }
    }

    /// Resolve a path in the overlay — returns the VfsPath from the first
    /// layer that contains it, respecting whiteouts.
    fn resolve(&self, path: &str) -> VfsResult<Option<VfsPath>> {
        // Check if whited out in upper
        if self.is_whiteout(path, &self.upper)? {
            return Ok(None);
        }

        // Check upper
        let upper_path = self.upper.join(path)?;
        if upper_path.exists()? {
            return Ok(Some(upper_path));
        }

        // Check if any ancestor directory in upper has an opaque whiteout
        if self.is_opaque_above(path, &self.upper)? {
            return Ok(None);
        }

        // Walk lower layers
        for layer in &self.lower {
            // Check for whiteout in this layer
            if self.is_whiteout(path, layer)? {
                return Ok(None);
            }

            let layer_path = layer.join(path)?;
            if layer_path.exists()? {
                return Ok(Some(layer_path));
            }

            if self.is_opaque_above(path, layer)? {
                return Ok(None);
            }
        }

        Ok(None)
    }

    /// Check if a file is whited out in the given layer.
    fn is_whiteout(&self, path: &str, layer: &VfsPath) -> VfsResult<bool> {
        let (parent, name) = split_path(path);
        let wh_name = format!("{}{}", WHITEOUT_PREFIX, name);
        let wh_path = if parent.is_empty() {
            wh_name
        } else {
            format!("{}/{}", parent, wh_name)
        };
        let wh = layer.join(&wh_path)?;
        wh.exists()
    }

    /// Check if any ancestor directory of `path` in the given layer has an
    /// opaque whiteout, meaning lower layers should not be consulted.
    fn is_opaque_above(&self, path: &str, layer: &VfsPath) -> VfsResult<bool> {
        let mut current = path.to_string();
        loop {
            let (parent, _) = split_path(&current);
            if parent.is_empty() {
                // Check root directory for opaque whiteout
                let opq = layer.join(OPAQUE_WHITEOUT)?;
                return opq.exists();
            }
            let opq_path = format!("{}/{}", parent, OPAQUE_WHITEOUT);
            let opq = layer.join(&opq_path)?;
            if opq.exists()? {
                return Ok(true);
            }
            current = parent.to_string();
        }
    }

    /// Ensure parent directories exist in upper layer (for copy-up).
    fn ensure_upper_parents(&self, path: &str) -> VfsResult<()> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = String::new();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if current.is_empty() {
                current = part.to_string();
            } else {
                current = format!("{}/{}", current, part);
            }
            let dir = self.upper.join(&current)?;
            if !dir.exists()? {
                dir.create_dir()?;
            }
        }
        Ok(())
    }

    /// Copy a file from a lower layer to the upper layer for modification.
    fn copy_up(&self, path: &str) -> VfsResult<VfsPath> {
        let source = self.resolve(path)?
            .ok_or_else(|| vfs::VfsError::from(VfsErrorKind::FileNotFound))?;

        self.ensure_upper_parents(path)?;

        let upper_path = self.upper.join(path)?;
        if source.metadata()?.file_type == VfsFileType::Directory {
            upper_path.create_dir()?;
        } else {
            let mut reader = source.open_file()?;
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;

            let mut writer = upper_path.create_file()?;
            writer.write_all(&data)?;
            writer.flush()?;
        }

        Ok(upper_path)
    }

    /// Remove any whiteout marker for this path in upper.
    fn remove_whiteout(&self, path: &str) -> VfsResult<()> {
        let (parent, name) = split_path(path);
        let wh_name = format!("{}{}", WHITEOUT_PREFIX, name);
        let wh_path = if parent.is_empty() {
            wh_name
        } else {
            format!("{}/{}", parent, wh_name)
        };
        let wh = self.upper.join(&wh_path)?;
        if wh.exists()? {
            wh.remove_file()?;
        }
        Ok(())
    }

    /// Create a whiteout marker in upper for a deleted path.
    fn create_whiteout(&self, path: &str) -> VfsResult<()> {
        let (parent, name) = split_path(path);
        let wh_name = format!("{}{}", WHITEOUT_PREFIX, name);
        let wh_path = if parent.is_empty() {
            wh_name
        } else {
            format!("{}/{}", parent, wh_name)
        };
        self.ensure_upper_parents(&wh_path)?;
        let wh = self.upper.join(&wh_path)?;
        if !wh.exists()? {
            wh.create_file()?;
        }
        Ok(())
    }

    /// Recursively copy a directory from overlay to upper.
    fn copy_dir_recursive(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.ensure_upper_parents(dest)?;
        self.remove_whiteout(dest)?;
        let dest_dir = self.upper.join(dest)?;
        if !dest_dir.exists()? {
            dest_dir.create_dir()?;
        }

        let entries: Vec<String> = FileSystem::read_dir(self, src)?.collect();
        for name in entries {
            let src_child = if src.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", src, name)
            };
            let dest_child = if dest.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", dest, name)
            };

            if let Some(resolved) = self.resolve(&src_child)? {
                let meta = resolved.metadata()?;
                if meta.file_type == VfsFileType::Directory {
                    self.copy_dir_recursive(&src_child, &dest_child)?;
                } else {
                    FileSystem::copy_file(self, &src_child, &dest_child)?;
                }
            }
        }

        Ok(())
    }

    /// Recursively remove a directory (creating whiteouts as needed).
    fn remove_dir_recursive(&self, path: &str) -> VfsResult<()> {
        let entries: Vec<String> = FileSystem::read_dir(self, path)?.collect();
        for name in entries {
            let child = if path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", path, name)
            };
            if let Some(resolved) = self.resolve(&child)? {
                let meta = resolved.metadata()?;
                if meta.file_type == VfsFileType::Directory {
                    self.remove_dir_recursive(&child)?;
                } else {
                    FileSystem::remove_file(self, &child)?;
                }
            }
        }

        // Clean up whiteout files that remove_file created in the upper dir
        let upper_dir = self.upper.join(path)?;
        if upper_dir.exists()? {
            let upper_entries: Vec<VfsPath> = upper_dir.read_dir()?.collect();
            for entry in upper_entries {
                let name = entry.filename();
                if name.starts_with(WHITEOUT_PREFIX) {
                    entry.remove_file()?;
                }
            }
        }

        FileSystem::remove_dir(self, path)?;
        Ok(())
    }
}

impl vfs::FileSystem for OverlayFS {
    fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        let mut whiteouts = HashSet::new();

        // Check if upper has an opaque whiteout for this directory
        let is_opaque = {
            let opq_path = if path.is_empty() {
                OPAQUE_WHITEOUT.to_string()
            } else {
                format!("{}/{}", path, OPAQUE_WHITEOUT)
            };
            let opq = self.upper.join(&opq_path)?;
            opq.exists()?
        };

        // Collect from upper
        let upper_dir = self.upper.join(path)?;
        if upper_dir.exists()? {
            for entry in upper_dir.read_dir()? {
                let name = entry.filename();
                if name.starts_with(WHITEOUT_PREFIX) {
                    if name != OPAQUE_WHITEOUT {
                        let target = name.strip_prefix(WHITEOUT_PREFIX).unwrap_or(&name);
                        whiteouts.insert(target.to_string());
                    }
                    continue;
                }
                seen.insert(name.clone());
                entries.push(name);
            }
        }

        if !is_opaque {
            for layer in &self.lower {
                let layer_dir = layer.join(path)?;
                if layer_dir.exists()? {
                    for entry in layer_dir.read_dir()? {
                        let name = entry.filename();
                        if name.starts_with(WHITEOUT_PREFIX) {
                            if name != OPAQUE_WHITEOUT {
                                let target =
                                    name.strip_prefix(WHITEOUT_PREFIX).unwrap_or(&name);
                                whiteouts.insert(target.to_string());
                            }
                            continue;
                        }
                        if seen.contains(&name) || whiteouts.contains(&name) {
                            continue;
                        }
                        seen.insert(name.clone());
                        entries.push(name);
                    }
                }
            }
        }

        entries.sort();
        Ok(Box::new(entries.into_iter()))
    }

    fn create_dir(&self, path: &str) -> VfsResult<()> {
        self.ensure_upper_parents(path)?;
        self.remove_whiteout(path)?;
        self.upper.join(path)?.create_dir()
    }

    fn open_file(&self, path: &str) -> VfsResult<Box<dyn vfs::SeekAndRead + Send>> {
        match self.resolve(path)? {
            Some(resolved) => resolved.open_file(),
            None => Err(vfs::VfsError::from(VfsErrorKind::FileNotFound)),
        }
    }

    fn create_file(&self, path: &str) -> VfsResult<Box<dyn vfs::SeekAndWrite + Send>> {
        self.ensure_upper_parents(path)?;
        self.remove_whiteout(path)?;
        self.upper.join(path)?.create_file()
    }

    fn append_file(&self, path: &str) -> VfsResult<Box<dyn vfs::SeekAndWrite + Send>> {
        let upper_path = self.upper.join(path)?;
        if upper_path.exists()? {
            return upper_path.append_file();
        }

        let resolved = self.resolve(path)?;
        if resolved.is_some() {
            let copied = self.copy_up(path)?;
            return copied.append_file();
        }

        Err(vfs::VfsError::from(VfsErrorKind::FileNotFound))
    }

    fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
        match self.resolve(path)? {
            Some(resolved) => resolved.metadata(),
            None => Err(vfs::VfsError::from(VfsErrorKind::FileNotFound)),
        }
    }

    fn exists(&self, path: &str) -> VfsResult<bool> {
        match self.resolve(path)? {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }

    fn remove_file(&self, path: &str) -> VfsResult<()> {
        let upper_path = self.upper.join(path)?;
        if upper_path.exists()? {
            upper_path.remove_file()?;
        }

        let exists_in_lower = self.lower.iter().any(|layer| {
            layer
                .join(path)
                .map(|p| p.exists().unwrap_or(false))
                .unwrap_or(false)
        });

        if exists_in_lower {
            self.create_whiteout(path)?;
        }

        Ok(())
    }

    fn remove_dir(&self, path: &str) -> VfsResult<()> {
        let upper_path = self.upper.join(path)?;
        if upper_path.exists()? {
            upper_path.remove_dir()?;
        }

        let exists_in_lower = self.lower.iter().any(|layer| {
            layer
                .join(path)
                .map(|p| p.exists().unwrap_or(false))
                .unwrap_or(false)
        });

        if exists_in_lower {
            self.create_whiteout(path)?;
        }

        Ok(())
    }

    fn copy_file(&self, src: &str, dest: &str) -> VfsResult<()> {
        let source = self
            .resolve(src)?
            .ok_or_else(|| vfs::VfsError::from(VfsErrorKind::FileNotFound))?;

        let mut reader = source.open_file()?;
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        self.ensure_upper_parents(dest)?;
        self.remove_whiteout(dest)?;
        let dest_path = self.upper.join(dest)?;
        let mut writer = dest_path.create_file()?;
        writer.write_all(&data)?;
        writer.flush()?;

        Ok(())
    }

    fn move_file(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.copy_file(src, dest)?;
        self.remove_file(src)?;
        Ok(())
    }

    fn move_dir(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.copy_dir_recursive(src, dest)?;
        self.remove_dir_recursive(src)?;
        Ok(())
    }
}

/// Split a path into (parent, name) components.
/// Returns ("", name) for top-level paths.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::MemoryFS;

    fn make_overlay() -> VfsPath {
        let upper = VfsPath::new(MemoryFS::new());
        let lower1 = VfsPath::new(MemoryFS::new());
        let lower2 = VfsPath::new(MemoryFS::new());

        // Lower2 (base layer): /etc/passwd, /etc/hostname
        lower2.join("etc").unwrap().create_dir().unwrap();
        lower2
            .join("etc/passwd")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"root:x:0:0:root:/root:/bin/bash\n")
            .unwrap();
        lower2
            .join("etc/hostname")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"base-host\n")
            .unwrap();

        // Lower1 (app layer): /etc/passwd override, /app/main.py
        lower1.join("etc").unwrap().create_dir().unwrap();
        lower1
            .join("etc/passwd")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"root:x:0:0:root:/root:/bin/bash\napp:x:1000:1000::/app:/bin/sh\n")
            .unwrap();
        lower1.join("app").unwrap().create_dir().unwrap();
        lower1
            .join("app/main.py")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"print('hello')\n")
            .unwrap();

        VfsPath::new(OverlayFS::new(upper, vec![lower1, lower2]))
    }

    /// Helper to collect readdir entries as filenames.
    fn readdir_names(path: &VfsPath) -> Vec<String> {
        path.read_dir()
            .unwrap()
            .map(|e| e.filename())
            .collect()
    }

    // ── Basic resolution ──────────────────────────────────────────

    #[test]
    fn test_read_from_lower() {
        let ov = make_overlay();
        let mut buf = String::new();
        ov.join("etc/passwd")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert!(buf.contains("app:x:1000:1000"));
    }

    #[test]
    fn test_read_from_base_layer() {
        let ov = make_overlay();
        let mut buf = String::new();
        ov.join("etc/hostname")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "base-host\n");
    }

    #[test]
    fn test_upper_overrides_lower() {
        let upper = VfsPath::new(MemoryFS::new());
        let lower = VfsPath::new(MemoryFS::new());

        lower
            .join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"lower")
            .unwrap();
        upper
            .join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"upper")
            .unwrap();

        let ov = VfsPath::new(OverlayFS::new(upper, vec![lower]));
        let mut buf = String::new();
        ov.join("file.txt")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "upper");
    }

    #[test]
    fn test_exists_in_lower() {
        let ov = make_overlay();
        assert!(ov.join("app/main.py").unwrap().exists().unwrap());
        assert!(ov.join("etc").unwrap().exists().unwrap());
    }

    #[test]
    fn test_not_exists() {
        let ov = make_overlay();
        assert!(!ov.join("nonexistent").unwrap().exists().unwrap());
    }

    // ── Write operations ──────────────────────────────────────────

    #[test]
    fn test_create_file_in_upper() {
        let ov = make_overlay();
        ov.join("newfile.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"new content")
            .unwrap();
        assert!(ov.join("newfile.txt").unwrap().exists().unwrap());
        let mut buf = String::new();
        ov.join("newfile.txt")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "new content");
    }

    #[test]
    fn test_create_dir_in_upper() {
        let ov = make_overlay();
        ov.join("newdir").unwrap().create_dir().unwrap();
        assert!(ov.join("newdir").unwrap().exists().unwrap());
        assert_eq!(
            ov.join("newdir").unwrap().metadata().unwrap().file_type,
            VfsFileType::Directory
        );
    }

    #[test]
    fn test_create_file_in_new_subdir() {
        let ov = make_overlay();
        ov.join("newdir").unwrap().create_dir().unwrap();
        ov.join("newdir/test.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"nested")
            .unwrap();
        let mut buf = String::new();
        ov.join("newdir/test.txt")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "nested");
    }

    // ── Copy-up on append ─────────────────────────────────────────

    #[test]
    fn test_append_triggers_copy_up() {
        let ov = make_overlay();
        ov.join("app/main.py")
            .unwrap()
            .append_file()
            .unwrap()
            .write_all(b"print('world')\n")
            .unwrap();
        let mut buf = String::new();
        ov.join("app/main.py")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert!(buf.contains("hello"));
    }

    // ── Delete (whiteout) ─────────────────────────────────────────

    #[test]
    fn test_delete_file_from_lower() {
        let ov = make_overlay();
        assert!(ov.join("etc/hostname").unwrap().exists().unwrap());
        ov.join("etc/hostname").unwrap().remove_file().unwrap();
        assert!(!ov.join("etc/hostname").unwrap().exists().unwrap());
    }

    #[test]
    fn test_delete_file_from_upper() {
        let ov = make_overlay();
        ov.join("temp.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"tmp")
            .unwrap();
        assert!(ov.join("temp.txt").unwrap().exists().unwrap());
        ov.join("temp.txt").unwrap().remove_file().unwrap();
        assert!(!ov.join("temp.txt").unwrap().exists().unwrap());
    }

    #[test]
    fn test_readdir_hides_deleted_files() {
        let ov = make_overlay();
        ov.join("etc/hostname").unwrap().remove_file().unwrap();
        let entries = readdir_names(&ov.join("etc").unwrap());
        assert!(!entries.contains(&"hostname".to_string()));
        assert!(entries.contains(&"passwd".to_string()));
    }

    // ── Readdir merging ───────────────────────────────────────────

    #[test]
    fn test_readdir_merges_layers() {
        let ov = make_overlay();
        let entries = readdir_names(&ov);
        assert!(entries.contains(&"etc".to_string()));
        assert!(entries.contains(&"app".to_string()));
    }

    #[test]
    fn test_readdir_deduplicates() {
        let ov = make_overlay();
        let entries = readdir_names(&ov);
        let etc_count = entries.iter().filter(|e| *e == "etc").count();
        assert_eq!(etc_count, 1);
    }

    #[test]
    fn test_readdir_includes_upper() {
        let ov = make_overlay();
        ov.join("upper_only.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"hi")
            .unwrap();
        let entries = readdir_names(&ov);
        assert!(entries.contains(&"upper_only.txt".to_string()));
    }

    #[test]
    fn test_readdir_sorted() {
        let ov = make_overlay();
        let entries = readdir_names(&ov);
        let mut sorted = entries.clone();
        sorted.sort();
        assert_eq!(entries, sorted);
    }

    // ── Opaque whiteout ───────────────────────────────────────────

    #[test]
    fn test_opaque_whiteout_blocks_lower() {
        let upper = VfsPath::new(MemoryFS::new());
        let lower = VfsPath::new(MemoryFS::new());

        lower.join("etc").unwrap().create_dir().unwrap();
        lower
            .join("etc/passwd")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"lower")
            .unwrap();
        lower
            .join("etc/hosts")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"lower")
            .unwrap();

        upper.join("etc").unwrap().create_dir().unwrap();
        upper
            .join("etc/.wh..wh..opq")
            .unwrap()
            .create_file()
            .unwrap();
        upper
            .join("etc/resolv.conf")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"upper")
            .unwrap();

        let ov = VfsPath::new(OverlayFS::new(upper, vec![lower]));
        let entries = readdir_names(&ov.join("etc").unwrap());
        assert!(entries.contains(&"resolv.conf".to_string()));
        assert!(!entries.contains(&"passwd".to_string()));
        assert!(!entries.contains(&"hosts".to_string()));
    }

    // ── Metadata ──────────────────────────────────────────────────

    #[test]
    fn test_metadata_file() {
        let ov = make_overlay();
        let meta = ov.join("app/main.py").unwrap().metadata().unwrap();
        assert_eq!(meta.file_type, VfsFileType::File);
        assert!(meta.len > 0);
    }

    #[test]
    fn test_metadata_directory() {
        let ov = make_overlay();
        let meta = ov.join("etc").unwrap().metadata().unwrap();
        assert_eq!(meta.file_type, VfsFileType::Directory);
    }

    #[test]
    fn test_metadata_nonexistent() {
        let ov = make_overlay();
        assert!(ov.join("nope").unwrap().metadata().is_err());
    }

    // ── Copy and move ─────────────────────────────────────────────

    #[test]
    fn test_copy_file_from_lower() {
        let ov = make_overlay();
        ov.join("etc/passwd")
            .unwrap()
            .copy_file(&ov.join("passwd_copy").unwrap())
            .unwrap();
        assert!(ov.join("passwd_copy").unwrap().exists().unwrap());
        let mut buf = String::new();
        ov.join("passwd_copy")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert!(buf.contains("app:x:1000:1000"));
    }

    #[test]
    fn test_move_file_removes_source() {
        let ov = make_overlay();
        ov.join("moveme.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"data")
            .unwrap();
        ov.join("moveme.txt")
            .unwrap()
            .move_file(&ov.join("moved.txt").unwrap())
            .unwrap();
        assert!(!ov.join("moveme.txt").unwrap().exists().unwrap());
        assert!(ov.join("moved.txt").unwrap().exists().unwrap());
    }

    // ── Directory rename (the EXDEV fix) ──────────────────────────

    #[test]
    fn test_move_dir_no_exdev() {
        let upper = VfsPath::new(MemoryFS::new());
        let lower = VfsPath::new(MemoryFS::new());

        lower.join("olddir").unwrap().create_dir().unwrap();
        lower
            .join("olddir/a.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"aaa")
            .unwrap();
        lower
            .join("olddir/b.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"bbb")
            .unwrap();

        let ov = VfsPath::new(OverlayFS::new(upper, vec![lower]));

        ov.join("olddir")
            .unwrap()
            .move_dir(&ov.join("newdir").unwrap())
            .unwrap();

        assert!(ov.join("newdir").unwrap().exists().unwrap());
        assert!(ov.join("newdir/a.txt").unwrap().exists().unwrap());
        assert!(ov.join("newdir/b.txt").unwrap().exists().unwrap());
        assert!(!ov.join("olddir").unwrap().exists().unwrap());
    }

    // ── Edge cases ────────────────────────────────────────────────

    #[test]
    fn test_empty_overlay() {
        let upper = VfsPath::new(MemoryFS::new());
        let ov = VfsPath::new(OverlayFS::new(upper, vec![]));
        let entries = readdir_names(&ov);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_single_lower_layer() {
        let upper = VfsPath::new(MemoryFS::new());
        let lower = VfsPath::new(MemoryFS::new());
        lower
            .join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"data")
            .unwrap();

        let ov = VfsPath::new(OverlayFS::new(upper, vec![lower]));
        assert!(ov.join("file.txt").unwrap().exists().unwrap());
    }

    #[test]
    fn test_recreate_deleted_file() {
        let upper = VfsPath::new(MemoryFS::new());
        let lower = VfsPath::new(MemoryFS::new());
        lower
            .join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"lower")
            .unwrap();

        let ov = VfsPath::new(OverlayFS::new(upper, vec![lower]));

        ov.join("file.txt").unwrap().remove_file().unwrap();
        assert!(!ov.join("file.txt").unwrap().exists().unwrap());

        ov.join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"new")
            .unwrap();
        assert!(ov.join("file.txt").unwrap().exists().unwrap());

        let mut buf = String::new();
        ov.join("file.txt")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "new");
    }

    #[test]
    fn test_delete_nonexistent_file_ok() {
        let ov = make_overlay();
        let result = ov.join("does_not_exist").unwrap().remove_file();
        assert!(result.is_ok());
    }

    #[test]
    fn test_split_path_top_level() {
        let (parent, name) = split_path("file.txt");
        assert_eq!(parent, "");
        assert_eq!(name, "file.txt");
    }

    #[test]
    fn test_split_path_nested() {
        let (parent, name) = split_path("etc/nginx/nginx.conf");
        assert_eq!(parent, "etc/nginx");
        assert_eq!(name, "nginx.conf");
    }

    // ── Three-layer stacking ──────────────────────────────────────

    #[test]
    fn test_three_layer_priority() {
        let upper = VfsPath::new(MemoryFS::new());
        let layer1 = VfsPath::new(MemoryFS::new());
        let layer2 = VfsPath::new(MemoryFS::new());
        let layer3 = VfsPath::new(MemoryFS::new());

        layer3
            .join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"three")
            .unwrap();
        layer2
            .join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"two")
            .unwrap();
        layer1
            .join("file.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"one")
            .unwrap();

        let ov = VfsPath::new(OverlayFS::new(upper, vec![layer1, layer2, layer3]));
        let mut buf = String::new();
        ov.join("file.txt")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "one");
    }

    #[test]
    fn test_three_layer_unique_files() {
        let upper = VfsPath::new(MemoryFS::new());
        let layer1 = VfsPath::new(MemoryFS::new());
        let layer2 = VfsPath::new(MemoryFS::new());

        layer1
            .join("a.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"a")
            .unwrap();
        layer2
            .join("b.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"b")
            .unwrap();
        upper
            .join("c.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"c")
            .unwrap();

        let ov = VfsPath::new(OverlayFS::new(upper, vec![layer1, layer2]));
        let entries = readdir_names(&ov);
        assert_eq!(entries.len(), 3);
        assert!(entries.contains(&"a.txt".to_string()));
        assert!(entries.contains(&"b.txt".to_string()));
        assert!(entries.contains(&"c.txt".to_string()));
    }
}
