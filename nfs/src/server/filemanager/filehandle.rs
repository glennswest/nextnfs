use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use multi_index_map::MultiIndexMap;

use vfs::VfsPath;

use nextnfs_proto::nfs4_proto::{Fsid4, NfsFh4, NfsFtype4, Nfstime4};

use super::{handle::WriteCacheHandle, locking::LockingState};

pub type FilehandleDb = MultiIndexFilehandleMap;

#[derive(MultiIndexMap, Debug, Clone)]
#[multi_index_derive(Debug, Clone)]
pub struct Filehandle {
    #[multi_index(hashed_unique)]
    pub id: NfsFh4,
    pub file: VfsPath,
    #[multi_index(hashed_unique)]
    pub path: String,
    pub attr_type: NfsFtype4,
    pub attr_change: u64,
    pub attr_size: u64,
    pub attr_fileid: u64,
    pub attr_fsid: Fsid4,
    pub attr_mode: u32,
    pub attr_nlink: u32,
    pub attr_owner: String,
    pub attr_owner_group: String,
    pub attr_space_used: u64,
    pub attr_time_access: Nfstime4,
    pub attr_time_metadata: Nfstime4,
    pub attr_time_modify: Nfstime4,
    pub verifier: Option<[u8; 8]>,
    pub locks: Vec<LockingState>,
    pub write_cache: Option<WriteCacheHandle>,
    pub version: u64,
}

/// Metadata from a real stat() call
#[derive(Debug, Clone)]
pub struct RealMeta {
    pub ino: u64,
    pub dev: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blocks: u64,
    pub atime: i64,
    pub atime_nsec: i64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
}

impl RealMeta {
    pub fn from_path(real_path: &PathBuf) -> Option<Self> {
        let meta = std::fs::symlink_metadata(real_path).ok()?;
        Some(Self {
            ino: meta.ino(),
            dev: meta.dev(),
            mode: meta.mode(),
            nlink: meta.nlink(),
            uid: meta.uid(),
            gid: meta.gid(),
            size: meta.size(),
            blocks: meta.blocks(),
            atime: meta.atime(),
            atime_nsec: meta.atime_nsec(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
        })
    }
}

impl Filehandle {
    /// Create a filehandle with real filesystem metadata
    pub fn new_real(
        file: VfsPath,
        id: NfsFh4,
        major: u64,
        minor: u64,
        version: u64,
        real_meta: &RealMeta,
    ) -> Self {
        let mut path = file.as_str().to_string();
        if path.is_empty() {
            path = "/".to_string();
        }
        let version = version + 1;

        #[allow(clippy::unnecessary_cast)]
        let file_type = real_meta.mode & (libc::S_IFMT as u32);
        #[allow(clippy::unnecessary_cast)]
        let attr_type = match file_type {
            m if m == libc::S_IFDIR as u32 => NfsFtype4::Nf4dir,
            m if m == libc::S_IFREG as u32 => NfsFtype4::Nf4reg,
            m if m == libc::S_IFLNK as u32 => NfsFtype4::Nf4lnk,
            m if m == libc::S_IFBLK as u32 => NfsFtype4::Nf4blk,
            m if m == libc::S_IFCHR as u32 => NfsFtype4::Nf4chr,
            m if m == libc::S_IFIFO as u32 => NfsFtype4::Nf4fifo,
            m if m == libc::S_IFSOCK as u32 => NfsFtype4::Nf4sock,
            _ => NfsFtype4::Nf4Undef,
        };

        let perm_mode = real_meta.mode & 0o7777;

        Filehandle {
            id,
            path,
            attr_type,
            // Sub-second resolution change attribute. mtime in seconds alone
            // collides under bursts of concurrent ops within the same wall-
            // clock second, leaving the kernel NFS client's readdir cache
            // stale (rmdir then returns ENOTEMPTY locally without consulting
            // the server). Pack secs * 1e9 + nsec for nanosecond resolution.
            attr_change: (real_meta.mtime as u64)
                .wrapping_mul(1_000_000_000)
                .wrapping_add(real_meta.mtime_nsec as u64),
            attr_size: real_meta.size,
            attr_fileid: real_meta.ino,
            attr_fsid: Fsid4 { major, minor },
            attr_mode: perm_mode,
            attr_nlink: real_meta.nlink as u32,
            attr_owner: real_meta.uid.to_string(),
            attr_owner_group: real_meta.gid.to_string(),
            attr_space_used: real_meta.blocks * 512,
            attr_time_access: Nfstime4 {
                seconds: real_meta.atime,
                nseconds: real_meta.atime_nsec as u32,
            },
            attr_time_metadata: Nfstime4 {
                seconds: real_meta.ctime,
                nseconds: real_meta.ctime_nsec as u32,
            },
            attr_time_modify: Nfstime4 {
                seconds: real_meta.mtime,
                nseconds: real_meta.mtime_nsec as u32,
            },
            file,
            verifier: None,
            locks: Vec::new(),
            write_cache: None,
            version,
        }
    }

    /// Fallback for when real stat is unavailable
    pub fn new(file: VfsPath, id: NfsFh4, major: u64, minor: u64, version: u64) -> Self {
        let init_time = Self::current_time();
        let mut path = file.as_str().to_string();
        if path.is_empty() {
            path = "/".to_string();
        }
        let version = version + 1;
        let is_dir = file.is_dir().unwrap_or(false);
        let size = file.metadata().map(|m| m.len).unwrap_or(0);

        Filehandle {
            id,
            path: path.clone(),
            attr_type: if is_dir {
                NfsFtype4::Nf4dir
            } else {
                NfsFtype4::Nf4reg
            },
            attr_change: version,
            attr_size: size,
            attr_fileid: {
                use std::hash::{DefaultHasher, Hash, Hasher};
                let mut hasher = DefaultHasher::new();
                path.hash(&mut hasher);
                hasher.finish()
            },
            attr_fsid: Fsid4 { major, minor },
            attr_mode: if is_dir { 0o755 } else { 0o644 },
            attr_nlink: 1,
            attr_owner: "0".to_string(),
            attr_owner_group: "0".to_string(),
            attr_space_used: size,
            attr_time_access: init_time,
            attr_time_metadata: init_time,
            attr_time_modify: init_time,
            file,
            verifier: None,
            locks: Vec::new(),
            write_cache: None,
            version,
        }
    }

    /// Create a synthetic filehandle for the pseudo-filesystem root.
    pub fn pseudo_root(id: NfsFh4) -> Self {
        let now = Self::current_time();
        // Use a MemoryFS path as placeholder — pseudo-root has no real backing file
        let file = vfs::VfsPath::new(vfs::MemoryFS::new());
        Filehandle {
            id,
            path: "/".to_string(),
            attr_type: NfsFtype4::Nf4dir,
            attr_change: 1,
            attr_size: 4096,
            attr_fileid: 1,
            attr_fsid: Fsid4 { major: 0, minor: 0 },
            attr_mode: 0o755,
            attr_nlink: 2,
            attr_owner: "0".to_string(),
            attr_owner_group: "0".to_string(),
            attr_space_used: 4096,
            attr_time_access: now,
            attr_time_metadata: now,
            attr_time_modify: now,
            file,
            verifier: None,
            locks: Vec::new(),
            write_cache: None,
            version: 1,
        }
    }

    pub fn attr_change(file: &VfsPath, default: u64) -> u64 {
        if let Ok(v) = file.metadata() {
            if let Some(v) = v.modified {
                return v.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            }
        }
        default
    }

    pub fn attr_size(file: &VfsPath) -> u64 {
        file.metadata().map(|m| m.len).unwrap_or(0)
    }

    pub fn current_time() -> Nfstime4 {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Nfstime4 {
            seconds: since_epoch.as_secs() as i64,
            nseconds: since_epoch.subsec_nanos(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::MemoryFS;

    fn make_real_meta(mode: u32, size: u64, ino: u64) -> RealMeta {
        RealMeta {
            ino,
            dev: 42,
            mode,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            size,
            blocks: (size + 511) / 512,
            atime: 1700000000,
            atime_nsec: 123456,
            mtime: 1700000100,
            mtime_nsec: 789012,
            ctime: 1700000200,
            ctime_nsec: 345678,
        }
    }

    #[test]
    fn test_new_dir_filehandle() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [1u8; 26];
        let fh = Filehandle::new(vfs, id, 10, 20, 0);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4dir);
        assert_eq!(fh.attr_mode, 0o755);
        assert_eq!(fh.path, "/");
        assert_eq!(fh.version, 1);
        assert_eq!(fh.attr_fsid.major, 10);
        assert_eq!(fh.attr_fsid.minor, 20);
    }

    #[test]
    fn test_new_file_filehandle() {
        let vfs = VfsPath::new(MemoryFS::new());
        // Create a file so it's detected as a file
        vfs.join("testfile").unwrap().create_file().unwrap();
        let file_path = vfs.join("testfile").unwrap();
        let id = [2u8; 26];
        let fh = Filehandle::new(file_path, id, 5, 5, 0);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4reg);
        assert_eq!(fh.attr_mode, 0o644);
        assert_eq!(fh.path, "/testfile");
    }

    #[test]
    fn test_new_empty_path_becomes_root() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [3u8; 26];
        let fh = Filehandle::new(vfs, id, 0, 0, 0);
        assert_eq!(fh.path, "/");
    }

    #[test]
    fn test_new_version_increments() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [4u8; 26];
        let fh = Filehandle::new(vfs, id, 0, 0, 5);
        assert_eq!(fh.version, 6);
    }

    #[test]
    fn test_new_real_dir_mode() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [5u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFDIR as u32 | 0o755, 4096, 100);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4dir);
        assert_eq!(fh.attr_mode, 0o755);
        assert_eq!(fh.attr_size, 4096);
        assert_eq!(fh.attr_fileid, 100);
        assert_eq!(fh.attr_owner, "1000");
        assert_eq!(fh.attr_owner_group, "1000");
    }

    #[test]
    fn test_new_real_regular_file_mode() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [6u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFREG as u32 | 0o644, 1024, 200);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4reg);
        assert_eq!(fh.attr_mode, 0o644);
    }

    #[test]
    fn test_new_real_symlink_mode() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [7u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFLNK as u32 | 0o777, 0, 300);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4lnk);
    }

    #[test]
    fn test_new_real_block_device() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [8u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFBLK as u32 | 0o660, 0, 400);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4blk);
    }

    #[test]
    fn test_new_real_char_device() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [9u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFCHR as u32 | 0o666, 0, 500);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4chr);
    }

    #[test]
    fn test_new_real_fifo() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [10u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFIFO as u32 | 0o644, 0, 600);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4fifo);
    }

    #[test]
    fn test_new_real_socket() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [11u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFSOCK as u32 | 0o755, 0, 700);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4sock);
    }

    #[test]
    fn test_new_real_time_attrs() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [12u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFREG as u32 | 0o644, 0, 800);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_time_access.seconds, 1700000000);
        assert_eq!(fh.attr_time_access.nseconds, 123456);
        assert_eq!(fh.attr_time_modify.seconds, 1700000100);
        assert_eq!(fh.attr_time_modify.nseconds, 789012);
        assert_eq!(fh.attr_time_metadata.seconds, 1700000200);
        assert_eq!(fh.attr_time_metadata.nseconds, 345678);
    }

    #[test]
    fn test_new_real_space_used() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [13u8; 26];
        #[allow(clippy::unnecessary_cast)]
        let meta = make_real_meta(libc::S_IFREG as u32 | 0o644, 2048, 900);
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        // blocks * 512
        assert_eq!(fh.attr_space_used, meta.blocks * 512);
    }

    #[test]
    fn test_new_real_nlink() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [14u8; 26];
        let mut meta = make_real_meta(libc::S_IFREG as u32 | 0o644, 0, 1000);
        meta.nlink = 5;
        let fh = Filehandle::new_real(vfs, id, 1, 1, 0, &meta);
        assert_eq!(fh.attr_nlink, 5);
    }

    #[test]
    fn test_pseudo_root() {
        let id = [0xFF; 26];
        let fh = Filehandle::pseudo_root(id);
        assert_eq!(fh.attr_type, NfsFtype4::Nf4dir);
        assert_eq!(fh.attr_mode, 0o755);
        assert_eq!(fh.attr_size, 4096);
        assert_eq!(fh.attr_nlink, 2);
        assert_eq!(fh.path, "/");
        assert_eq!(fh.attr_fsid.major, 0);
        assert_eq!(fh.attr_fsid.minor, 0);
        assert!(fh.locks.is_empty());
        assert!(fh.write_cache.is_none());
    }

    #[test]
    fn test_attr_size_empty_file() {
        let vfs = VfsPath::new(MemoryFS::new());
        vfs.join("empty").unwrap().create_file().unwrap();
        let file = vfs.join("empty").unwrap();
        assert_eq!(Filehandle::attr_size(&file), 0);
    }

    #[test]
    fn test_attr_size_with_data() {
        let vfs = VfsPath::new(MemoryFS::new());
        vfs.join("data").unwrap().create_file().unwrap();
        let file = vfs.join("data").unwrap();
        {
            use std::io::Write;
            let mut f = file.append_file().unwrap();
            f.write_all(b"hello world").unwrap();
        }
        assert_eq!(Filehandle::attr_size(&file), 11);
    }

    #[test]
    fn test_attr_size_nonexistent() {
        let vfs = VfsPath::new(MemoryFS::new());
        let file = vfs.join("nosuch").unwrap();
        assert_eq!(Filehandle::attr_size(&file), 0);
    }

    #[test]
    fn test_current_time_reasonable() {
        let now = Filehandle::current_time();
        // Should be after 2020-01-01 (1577836800)
        assert!(now.seconds > 1577836800);
    }

    #[test]
    fn test_real_meta_from_path_tmp() {
        let meta = RealMeta::from_path(&PathBuf::from("/tmp"));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert!(meta.ino > 0);
        assert!(meta.dev > 0);
    }

    #[test]
    fn test_real_meta_from_path_nonexistent() {
        let meta = RealMeta::from_path(&PathBuf::from("/nonexistent_xyz_abc_123"));
        assert!(meta.is_none());
    }

    #[test]
    fn test_filehandle_initial_no_locks() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [15u8; 26];
        let fh = Filehandle::new(vfs, id, 0, 0, 0);
        assert!(fh.locks.is_empty());
        assert!(fh.verifier.is_none());
        assert!(fh.write_cache.is_none());
    }

    #[test]
    fn test_fileid_is_hash_based() {
        let vfs = VfsPath::new(MemoryFS::new());
        let id = [16u8; 26];
        let fh = Filehandle::new(vfs, id, 0, 0, 0);
        // fileid should be nonzero (hash of path)
        assert!(fh.attr_fileid > 0);
    }
}
