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
            attr_change: real_meta.mtime as u64,
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
                return v.duration_since(UNIX_EPOCH).unwrap().as_secs();
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
            .unwrap();
        Nfstime4 {
            seconds: since_epoch.as_secs() as i64,
            nseconds: since_epoch.subsec_nanos(),
        }
    }
}
