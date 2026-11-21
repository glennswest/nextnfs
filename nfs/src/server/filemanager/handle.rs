use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};
use vfs::VfsPath;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;

use crate::server::beneath::ExportRoot;

use nextnfs_proto::nfs4_proto::{
    Attrlist4, FileAttr, FileAttrValue, NfsLease4, NfsLockType4, Nfsace4, NfsStat4, Nfstime4,
    Stateid4, ACE4_ACCESS_ALLOWED_ACE_TYPE, ACE4_APPEND_DATA, ACE4_DELETE, ACE4_DELETE_CHILD,
    ACE4_EXECUTE, ACE4_IDENTIFIER_GROUP, ACE4_READ_ACL, ACE4_READ_ATTRIBUTES, ACE4_READ_DATA,
    ACE4_SYNCHRONIZE, ACE4_WRITE_ACL, ACE4_WRITE_ATTRIBUTES, ACE4_WRITE_DATA,
    ACE4_WRITE_OWNER, ACL4_SUPPORT_ALLOW_ACL, FH4_PERSISTENT, FsLocations4,
};

use super::{
    caching::run_file_write_cache, caching::WriteCache, filehandle::Filehandle,
    filehandle::RealMeta, locking::LockingState, run_file_manager, FileManager,
};
use crate::server::filemanager::NfsFh4;

pub enum FileManagerMessage {
    GetRootFilehandle(GetRootFilehandleRequest),
    GetFilehandle(GetFilehandleRequest),
    GetFilehandleAttrs(GetFilehandleAttrsRequest),
    CreateFile(CreateFileRequest),
    CreateOpenState(CreateOpenStateRequest),
    DowngradeOpen(DowngradeOpenRequest),
    RemoveFile(RemoveFileRequest),
    TouchFile(TouchFileRequest),
    UpdateFilehandle(Filehandle),
    RenamePath(RenamePathRequest),
    LockFile(LockFileRequest),
    UnlockFile(UnlockFileRequest),
    TestLock(TestLockRequest),
    ReleaseLockOwner(ReleaseLockOwnerRequest),
    OpenNamedAttrDir(OpenNamedAttrDirRequest),
    GrantDelegation(GrantDelegationRequest),
    ReturnDelegation(ReturnDelegationRequest),
    GetDelegation(GetDelegationRequest),
    CloseFile(CloseFileRequest),
    GetWriteCacheHandle(WriteCacheHandleRequest),
    DropWriteCacheHandle(DropCacheHandleRequest),
}

pub struct GetRootFilehandleRequest {
    pub respond_to: oneshot::Sender<Filehandle>,
}

pub struct GetFilehandleRequest {
    pub path: Option<String>,
    pub filehandle: Option<NfsFh4>,
    pub respond_to: oneshot::Sender<Option<Filehandle>>,
}

pub struct GetFilehandleAttrsRequest {
    pub filehandle_id: NfsFh4,
    pub attrs_request: Vec<FileAttr>,
    pub respond_to: oneshot::Sender<Option<(Vec<FileAttr>, Vec<FileAttrValue>)>>,
}

pub struct CreateFileRequest {
    pub path: VfsPath,
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub share_access: u32,
    pub share_deny: u32,
    pub verifier: Option<[u8; 8]>,
    /// The file with its open stateid in `locks[0]`, or why the OPEN fails
    /// (`NFS4ERR_SHARE_DENIED`, ...).
    pub respond_to: oneshot::Sender<Result<Filehandle, NfsStat4>>,
}

/// Request to create open state on an existing file (OPEN without create,
/// CLAIM_PREVIOUS reclaim).
pub struct CreateOpenStateRequest {
    pub path: VfsPath,
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub share_access: u32,
    pub share_deny: u32,
    pub respond_to: oneshot::Sender<Result<LockingState, NfsStat4>>,
}

/// OPEN_DOWNGRADE of the open stateid `stateid` on `filehandle_id`.
pub struct DowngradeOpenRequest {
    pub filehandle_id: NfsFh4,
    pub stateid: [u8; 12],
    pub share_access: u32,
    pub share_deny: u32,
    pub respond_to: oneshot::Sender<Result<LockingState, NfsStat4>>,
}

/// Atomic change-info returned by directory-modifying ops so the client
/// can update its cached readdir/dentry state without a follow-up GETATTR.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirChangeInfo {
    pub before: u64,
    pub after: u64,
    pub atomic: bool,
}

pub struct RemoveFileRequest {
    pub path: VfsPath,
    pub respond_to: oneshot::Sender<Result<DirChangeInfo, NfsStat4>>,
}

pub struct TouchFileRequest {
    pub id: NfsFh4,
}

pub struct RenamePathRequest {
    pub old_path: String,
    pub new_path: String,
    pub new_vfs_path: VfsPath,
    pub respond_to: oneshot::Sender<()>,
}

pub struct WriteCacheHandleRequest {
    pub filemanager: FileManagerHandle,
    pub filehandle: Filehandle,
    pub respond_to: oneshot::Sender<WriteCacheHandle>,
}

pub struct DropCacheHandleRequest {
    pub filehandle_id: NfsFh4,
}

pub struct CloseFileRequest {
    pub stateid: [u8; 12],
    pub respond_to: oneshot::Sender<()>,
}

pub struct OpenNamedAttrDirRequest {
    pub fileid: u64,
    pub createdir: bool,
    pub respond_to: oneshot::Sender<Option<Filehandle>>,
}

pub struct GrantDelegationRequest {
    pub filehandle_id: NfsFh4,
    pub client_id: u64,
    pub is_write: bool,
    pub respond_to: oneshot::Sender<Option<Stateid4>>,
}

pub struct ReturnDelegationRequest {
    pub filehandle_id: NfsFh4,
    pub deleg_stateid: Stateid4,
    pub respond_to: oneshot::Sender<NfsStat4>,
}

pub struct GetDelegationRequest {
    pub filehandle_id: NfsFh4,
    pub respond_to: oneshot::Sender<Option<super::DelegationState>>,
}

pub struct LockFileRequest {
    pub filehandle_id: NfsFh4,
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub lock_type: NfsLockType4,
    pub offset: u64,
    pub length: u64,
    pub respond_to: oneshot::Sender<LockResult>,
}

pub struct UnlockFileRequest {
    pub lock_stateid: [u8; 12],
    pub offset: u64,
    pub length: u64,
    pub respond_to: oneshot::Sender<UnlockResult>,
}

pub struct TestLockRequest {
    pub filehandle_id: NfsFh4,
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub lock_type: NfsLockType4,
    pub offset: u64,
    pub length: u64,
    pub respond_to: oneshot::Sender<TestLockResult>,
}

pub struct ReleaseLockOwnerRequest {
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub respond_to: oneshot::Sender<NfsStat4>,
}

#[derive(Debug)]
pub enum LockResult {
    Ok(Stateid4),
    Denied {
        offset: u64,
        length: u64,
        lock_type: NfsLockType4,
        owner_clientid: u64,
        owner: Vec<u8>,
    },
    Error(NfsStat4),
}

#[derive(Debug)]
pub enum UnlockResult {
    Ok(Stateid4),
    Error(NfsStat4),
}

#[derive(Debug)]
pub enum TestLockResult {
    Ok,
    Denied {
        offset: u64,
        length: u64,
        lock_type: NfsLockType4,
        owner_clientid: u64,
        owner: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct FileManagerError {
    pub nfs_error: NfsStat4,
}

/// Synthesize NFSv4 ACL entries from POSIX mode bits.
///
/// Generates three ALLOW ACEs (owner, group, everyone) matching
/// the traditional POSIX rwx permission model.
pub fn mode_to_acl(mode: u32, owner: &str, group: &str) -> Vec<Nfsace4> {
    fn mode_bits_to_mask(bits: u32) -> u32 {
        let mut mask = ACE4_READ_ATTRIBUTES | ACE4_READ_ACL | ACE4_SYNCHRONIZE;
        if bits & 4 != 0 {
            mask |= ACE4_READ_DATA;
        }
        if bits & 2 != 0 {
            mask |= ACE4_WRITE_DATA | ACE4_APPEND_DATA | ACE4_WRITE_ATTRIBUTES
                | ACE4_WRITE_ACL | ACE4_WRITE_OWNER | ACE4_DELETE | ACE4_DELETE_CHILD;
        }
        if bits & 1 != 0 {
            mask |= ACE4_EXECUTE;
        }
        mask
    }

    let owner_bits = (mode >> 6) & 7;
    let group_bits = (mode >> 3) & 7;
    let other_bits = mode & 7;

    vec![
        Nfsace4 {
            acetype: ACE4_ACCESS_ALLOWED_ACE_TYPE,
            flag: 0,
            access_mask: mode_bits_to_mask(owner_bits),
            who: owner.to_string(),
        },
        Nfsace4 {
            acetype: ACE4_ACCESS_ALLOWED_ACE_TYPE,
            flag: ACE4_IDENTIFIER_GROUP,
            access_mask: mode_bits_to_mask(group_bits),
            who: group.to_string(),
        },
        Nfsace4 {
            acetype: ACE4_ACCESS_ALLOWED_ACE_TYPE,
            flag: 0,
            access_mask: mode_bits_to_mask(other_bits),
            who: "EVERYONE@".to_string(),
        },
    ]
}

#[derive(Debug, Clone)]
pub struct FileManagerHandle {
    sender: mpsc::Sender<FileManagerMessage>,
    export_root: PathBuf,
    /// Every path of the export is resolved beneath this fd (#123).
    beneath: Arc<ExportRoot>,
    /// The vfs tree is not the disk under `beneath` (MemoryFS, unit tests):
    /// an object missing beneath the root may be served from the vfs. Never
    /// for a real export, where "missing" means it is gone, or is mid-swap.
    vfs_only: bool,
    lease_time: u32,
    hard_link_support: bool,
    symlink_support: bool,
    unique_handles: bool,
    /// Bumped whenever an fsync of unstable data fails (#136): the write
    /// verifier changes, so a client whose unstable WRITEs carried the old
    /// one sends them again instead of taking a later COMMIT's word.
    write_epoch: Arc<AtomicU64>,
}

impl FileManagerHandle {
    /// A file manager over a vfs tree that is not the disk at `export_root`
    /// (MemoryFS, unit tests). `export_root` is opened as the confining root
    /// (`ExportRoot::open_or_closed`: if it cannot be, every disk op fails);
    /// an object not found under it is served from the vfs tree.
    pub fn new(root: VfsPath, fsid: Option<u64>, export_root: PathBuf) -> Self {
        let mut h = Self::with_root(root, fsid, Arc::new(ExportRoot::open_or_closed(&export_root)));
        h.vfs_only = true;
        h
    }

    /// A file manager whose paths are resolved beneath `beneath`, and whose
    /// vfs root is that same disk (`BeneathFS`): no vfs fallback.
    pub fn with_root(root: VfsPath, fsid: Option<u64>, beneath: Arc<ExportRoot>) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        let export_root = beneath.path().to_path_buf();
        let fmanager = FileManager::new(receiver, root, fsid, beneath.clone());
        tokio::spawn(run_file_manager(fmanager));

        Self {
            sender,
            export_root,
            beneath,
            vfs_only: false,
            lease_time: 90,
            hard_link_support: true,
            symlink_support: true,
            unique_handles: true,
            write_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The current write epoch (0 until an fsync of unstable data fails).
    pub fn write_epoch(&self) -> u64 {
        self.write_epoch.load(AtomicOrdering::SeqCst)
    }

    /// A failed fsync may have lost unstable data: change the verifier.
    pub fn bump_write_epoch(&self) -> u64 {
        self.write_epoch.fetch_add(1, AtomicOrdering::SeqCst) + 1
    }

    /// The WRITE/COMMIT verifier for `boot_time` under write epoch `epoch`:
    /// the boot time's big-endian bytes, epoch added from bit 32 up (epoch 0
    /// is the boot time alone).
    pub fn write_verifier(boot_time: u64, epoch: u64) -> [u8; 8] {
        boot_time.wrapping_add(epoch.wrapping_shl(32)).to_be_bytes()
    }

    /// Get the export root path for constructing real filesystem paths.
    pub fn export_root(&self) -> &PathBuf {
        &self.export_root
    }

    /// The export root fd that every op resolves its path beneath (#123).
    pub fn beneath(&self) -> &ExportRoot {
        &self.beneath
    }

    /// Whether an object missing beneath the root may be served from the
    /// vfs tree instead: only when that tree is not the disk (unit tests).
    pub fn vfs_only(&self) -> bool {
        self.vfs_only
    }

    /// lstat of the export path `path` ("/a/b"), resolved beneath the root.
    pub fn meta(&self, path: &str) -> Option<RealMeta> {
        self.beneath.symlink_metadata(path).ok().map(|m| RealMeta::from_metadata(&m))
    }

    /// The server path of an export path, for messages only: ops never open
    /// it (a symlink swapped in on the way would be followed), they go
    /// through `beneath()`.
    pub fn real_path(&self, vfs_path: &str) -> PathBuf {
        if vfs_path.is_empty() || vfs_path == "/" {
            self.export_root.clone()
        } else {
            self.export_root.join(vfs_path.trim_start_matches('/'))
        }
    }

    /// Read the directory's current change attribute (mtime in nanoseconds)
    /// directly from the underlying filesystem. Used by op_open/op_create to
    /// populate ChangeInfo4 before/after for directory-modifying operations
    /// that execute outside the FileManager actor (so cinfo.atomic = false).
    pub fn dir_change_attr(&self, vfs_path: &str) -> u64 {
        self.meta(vfs_path)
            .map(|m| {
                (m.mtime as u64)
                    .wrapping_mul(1_000_000_000)
                    .wrapping_add(m.mtime_nsec as u64)
            })
            .unwrap_or(0)
    }

    async fn send_filehandle_request(
        &self,
        path: Option<String>,
        filehandle: Option<NfsFh4>,
    ) -> Result<Filehandle, FileManagerError> {
        let filehandle_set = filehandle.is_some();
        let (tx, rx) = oneshot::channel();
        let req = GetFilehandleRequest {
            path: path.clone(),
            filehandle,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::GetFilehandle(req)).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(fh) => {
                if let Some(fh) = fh {
                    return Ok(fh);
                }
                if let Some(path) = path {
                    debug!("File not found: {:?}", path);
                    if !filehandle_set {
                        Err(FileManagerError {
                            nfs_error: NfsStat4::Nfs4errNoent,
                        })
                    } else {
                        Err(FileManagerError {
                            nfs_error: NfsStat4::Nfs4errStale,
                        })
                    }
                } else {
                    debug!("Filehandle not found");
                    // https://datatracker.ietf.org/doc/html/rfc7530#section-4.2.3
                    // If the server can definitively determine that a
                    // volatile filehandle refers to an object that has been removed, the
                    // server should return NFS4ERR_STALE to the client (as is the case for
                    // persistent filehandles)
                    Err(FileManagerError {
                        nfs_error: NfsStat4::Nfs4errStale,
                    })
                }
            }
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    pub async fn get_root_filehandle(&self) -> Result<Filehandle, FileManagerError> {
        self.send_filehandle_request(None, None).await
    }

    pub async fn get_filehandle_for_id(&self, id: NfsFh4) -> Result<Filehandle, FileManagerError> {
        self.send_filehandle_request(None, Some(id)).await
    }

    pub async fn get_filehandle_for_path(
        &self,
        path: String,
    ) -> Result<Filehandle, FileManagerError> {
        self.send_filehandle_request(Some(path), None).await
    }

    pub async fn get_filehandle_attrs(
        &self,
        filehandle_id: NfsFh4,
        attrs_request: Vec<FileAttr>,
    ) -> Result<(Vec<FileAttr>, Vec<FileAttrValue>), FileManagerError> {
        let (tx, rx) = oneshot::channel();
        let req = GetFilehandleAttrsRequest {
            filehandle_id,
            attrs_request,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::GetFilehandleAttrs(req)).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(attrs) => {
                if let Some(attrs) = attrs {
                    return Ok(attrs);
                }
                Err(FileManagerError {
                    nfs_error: NfsStat4::Nfs4errBadhandle,
                })
            }
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    pub async fn create_file(
        &self,
        path: VfsPath,
        client_id: u64,
        owner: Vec<u8>,
        access: u32,
        deny: u32,
        verifier: Option<[u8; 8]>,
    ) -> Result<Filehandle, FileManagerError> {
        let (tx, rx) = oneshot::channel();
        let req = CreateFileRequest {
            path,
            client_id,
            owner,
            share_access: access,
            share_deny: deny,
            verifier,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::CreateFile(req)).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(result) => result.map_err(|nfs_error| FileManagerError { nfs_error }),
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    /// Open state on an existing file (OPEN without create, CLAIM_PREVIOUS
    /// reclaim): the share reservation, checked against the other
    /// open-owners' (`NFS4ERR_SHARE_DENIED`), or merged into this
    /// open-owner's existing one.
    pub async fn create_open_state(
        &self,
        path: VfsPath,
        client_id: u64,
        owner: Vec<u8>,
        access: u32,
        deny: u32,
    ) -> Result<LockingState, FileManagerError> {
        let (tx, rx) = oneshot::channel();
        let req = CreateOpenStateRequest {
            path,
            client_id,
            owner,
            share_access: access,
            share_deny: deny,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::CreateOpenState(req)).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(result) => result.map_err(|nfs_error| FileManagerError { nfs_error }),
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    /// OPEN_DOWNGRADE: narrow the open stateid `stateid` on `filehandle_id`
    /// to `access`/`deny` (a subset of what it holds, else NFS4ERR_INVAL;
    /// unknown stateid → NFS4ERR_BAD_STATEID).
    pub async fn downgrade_open(
        &self,
        filehandle_id: NfsFh4,
        stateid: [u8; 12],
        access: u32,
        deny: u32,
    ) -> Result<LockingState, FileManagerError> {
        let (tx, rx) = oneshot::channel();
        let req = DowngradeOpenRequest {
            filehandle_id,
            stateid,
            share_access: access,
            share_deny: deny,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::DowngradeOpen(req)).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(result) => result.map_err(|nfs_error| FileManagerError { nfs_error }),
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    pub async fn remove_file(&self, path: VfsPath) -> Result<DirChangeInfo, FileManagerError> {
        let (tx, rx) = oneshot::channel();
        if let Err(e) = self.sender.send(FileManagerMessage::RemoveFile(RemoveFileRequest {
            path,
            respond_to: tx,
        })).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(Ok(cinfo)) => Ok(cinfo),
            Ok(Err(nfs_error)) => Err(FileManagerError { nfs_error }),
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    pub async fn touch_file(&self, id: NfsFh4) {
        if let Err(e) = self.sender.send(FileManagerMessage::TouchFile(TouchFileRequest { id })).await {
            error!("filemanager actor gone: {}", e);
        }
    }

    pub async fn update_filehandle(&self, filehandle: Filehandle) {
        if let Err(e) = self.sender.send(FileManagerMessage::UpdateFilehandle(filehandle)).await {
            error!("filemanager actor gone: {}", e);
        }
    }

    pub async fn get_write_cache_handle(
        &self,
        filehandle: Filehandle,
    ) -> Result<WriteCacheHandle, FileManagerError> {
        let (tx, rx) = oneshot::channel();
        if let Err(e) = self.sender.send(FileManagerMessage::GetWriteCacheHandle(
            WriteCacheHandleRequest {
                filemanager: self.clone(),
                filehandle,
                respond_to: tx,
            },
        )).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(handle) => Ok(handle),
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    pub async fn grant_delegation(
        &self,
        filehandle_id: NfsFh4,
        client_id: u64,
        is_write: bool,
    ) -> Option<Stateid4> {
        let (tx, rx) = oneshot::channel();
        let req = GrantDelegationRequest {
            filehandle_id,
            client_id,
            is_write,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::GrantDelegation(req)).await {
            error!("filemanager actor gone: {}", e);
            return None;
        }
        rx.await.unwrap_or(None)
    }

    pub async fn return_delegation(
        &self,
        filehandle_id: NfsFh4,
        deleg_stateid: Stateid4,
    ) -> NfsStat4 {
        let (tx, rx) = oneshot::channel();
        let req = ReturnDelegationRequest {
            filehandle_id,
            deleg_stateid,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::ReturnDelegation(req)).await {
            error!("filemanager actor gone: {}", e);
            return NfsStat4::Nfs4errServerfault;
        }
        rx.await.unwrap_or(NfsStat4::Nfs4errServerfault)
    }

    pub async fn get_delegation(
        &self,
        filehandle_id: NfsFh4,
    ) -> Option<super::DelegationState> {
        let (tx, rx) = oneshot::channel();
        let req = GetDelegationRequest {
            filehandle_id,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::GetDelegation(req)).await {
            error!("filemanager actor gone: {}", e);
            return None;
        }
        rx.await.unwrap_or(None)
    }

    pub async fn open_named_attr_dir(
        &self,
        fileid: u64,
        createdir: bool,
    ) -> Result<Filehandle, FileManagerError> {
        let (tx, rx) = oneshot::channel();
        let req = OpenNamedAttrDirRequest {
            fileid,
            createdir,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::OpenNamedAttrDir(req)).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(Some(fh)) => Ok(fh),
            Ok(None) => Err(FileManagerError { nfs_error: NfsStat4::Nfs4errNoent }),
            Err(_) => Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault }),
        }
    }

    /// Update the filehandle database after a rename/move operation.
    pub async fn rename_path(&self, old_path: String, new_path: String, new_vfs_path: VfsPath) {
        let (tx, rx) = oneshot::channel();
        let req = RenamePathRequest {
            old_path,
            new_path,
            new_vfs_path,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::RenamePath(req)).await {
            error!("filemanager actor gone: {}", e);
            return;
        }
        let _ = rx.await;
    }

    pub async fn drop_write_cache_handle(&self, filehandle_id: NfsFh4) {
        if let Err(e) = self.sender.send(FileManagerMessage::DropWriteCacheHandle(
            DropCacheHandleRequest { filehandle_id },
        )).await {
            error!("filemanager actor gone: {}", e);
        }
    }

    /// Release the open stateid from lockdb on CLOSE.
    pub async fn close_file(&self, stateid: [u8; 12]) {
        let (tx, rx) = oneshot::channel();
        if let Err(e) = self.sender.send(FileManagerMessage::CloseFile(CloseFileRequest {
            stateid,
            respond_to: tx,
        })).await {
            error!("filemanager actor gone: {}", e);
            return;
        }
        let _ = rx.await;
    }

    pub async fn lock_file(
        &self,
        filehandle_id: NfsFh4,
        client_id: u64,
        owner: Vec<u8>,
        lock_type: NfsLockType4,
        offset: u64,
        length: u64,
    ) -> LockResult {
        let (tx, rx) = oneshot::channel();
        let req = LockFileRequest {
            filehandle_id,
            client_id,
            owner,
            lock_type,
            offset,
            length,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::LockFile(req)).await {
            error!("filemanager actor gone: {}", e);
            return LockResult::Error(NfsStat4::Nfs4errServerfault);
        }
        rx.await.unwrap_or(LockResult::Error(NfsStat4::Nfs4errServerfault))
    }

    pub async fn unlock_file(
        &self,
        lock_stateid: [u8; 12],
        offset: u64,
        length: u64,
    ) -> UnlockResult {
        let (tx, rx) = oneshot::channel();
        let req = UnlockFileRequest {
            lock_stateid,
            offset,
            length,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::UnlockFile(req)).await {
            error!("filemanager actor gone: {}", e);
            return UnlockResult::Error(NfsStat4::Nfs4errServerfault);
        }
        rx.await.unwrap_or(UnlockResult::Error(NfsStat4::Nfs4errServerfault))
    }

    pub async fn test_lock(
        &self,
        filehandle_id: NfsFh4,
        client_id: u64,
        owner: Vec<u8>,
        lock_type: NfsLockType4,
        offset: u64,
        length: u64,
    ) -> TestLockResult {
        let (tx, rx) = oneshot::channel();
        let req = TestLockRequest {
            filehandle_id,
            client_id,
            owner,
            lock_type,
            offset,
            length,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::TestLock(req)).await {
            error!("filemanager actor gone: {}", e);
            return TestLockResult::Denied {
                offset: 0, length: 0,
                lock_type: NfsLockType4::ReadLt,
                owner_clientid: 0, owner: vec![],
            };
        }
        rx.await.unwrap_or(TestLockResult::Denied {
            offset: 0, length: 0,
            lock_type: NfsLockType4::ReadLt,
            owner_clientid: 0, owner: vec![],
        })
    }

    pub async fn release_lock_owner(&self, client_id: u64, owner: Vec<u8>) -> NfsStat4 {
        let (tx, rx) = oneshot::channel();
        let req = ReleaseLockOwnerRequest {
            client_id,
            owner,
            respond_to: tx,
        };
        if let Err(e) = self.sender.send(FileManagerMessage::ReleaseLockOwner(req)).await {
            error!("filemanager actor gone: {}", e);
            return NfsStat4::Nfs4errServerfault;
        }
        rx.await.unwrap_or(NfsStat4::Nfs4errServerfault)
    }

    pub fn filehandle_attrs(
        &mut self,
        attr_request: &Vec<FileAttr>,
        filehandle: &Filehandle,
    ) -> Option<(Attrlist4<FileAttr>, Attrlist4<FileAttrValue>)> {
        // space_* and files_* are the filesystem's own (#135): one fstatvfs
        // of the export root, only when one of them is asked for.
        let space = if attr_request.iter().any(|a| {
            matches!(
                a,
                FileAttr::SpaceAvail
                    | FileAttr::SpaceFree
                    | FileAttr::SpaceTotal
                    | FileAttr::FilesAvail
                    | FileAttr::FilesFree
                    | FileAttr::FilesTotal
            )
        }) {
            match self.beneath.statvfs() {
                Ok(space) => Some(space),
                Err(e) => {
                    error!("statvfs of the export root: {}", e);
                    None
                }
            }
        } else {
            None
        };
        let space = space.unwrap_or_default();
        // Refresh from real filesystem to pick up current size/mtime/etc.
        let real_path = self.real_path(filehandle.file.as_str());
        let input_fileid = filehandle.attr_fileid;
        let fh = if let Some(meta) = self.meta(filehandle.file.as_str()) {
            let refreshed = Filehandle::new_real(
                filehandle.file.clone(),
                filehandle.id,
                filehandle.attr_fsid.major,
                filehandle.attr_fsid.minor,
                filehandle.version,
                &meta,
            );
            if refreshed.attr_fileid != input_fileid {
                warn!(
                    "FILEID CHANGED in handle filehandle_attrs: path={:?} input_fileid={} refreshed_fileid={} real_path={:?}",
                    filehandle.file.as_str(), input_fileid, refreshed.attr_fileid, real_path
                );
            }
            refreshed
        } else {
            warn!(
                "RealMeta::from_path FAILED in handle filehandle_attrs: path={:?} real_path={:?} using cached fileid={}",
                filehandle.file.as_str(), real_path, input_fileid
            );
            filehandle.clone()
        };
        let filehandle = &fh;

        let mut answer_attrs = Attrlist4::<FileAttr>::new(None);
        let mut attrs = Attrlist4::<FileAttrValue>::new(None);

        for fileattr in attr_request {
            match fileattr {
                FileAttr::SupportedAttrs => {
                    attrs.push(FileAttrValue::SupportedAttrs(self.attr_supported_attrs()));
                    answer_attrs.push(FileAttr::SupportedAttrs);
                }
                FileAttr::Type => {
                    attrs.push(FileAttrValue::Type(filehandle.attr_type));
                    answer_attrs.push(FileAttr::Type);
                }
                FileAttr::LeaseTime => {
                    attrs.push(FileAttrValue::LeaseTime(self.attr_lease_time()));
                    answer_attrs.push(FileAttr::LeaseTime);
                }
                FileAttr::Change => {
                    attrs.push(FileAttrValue::Change(filehandle.attr_change));
                    answer_attrs.push(FileAttr::Change);
                }
                FileAttr::Size => {
                    attrs.push(FileAttrValue::Size(filehandle.attr_size));
                    answer_attrs.push(FileAttr::Size);
                }
                FileAttr::LinkSupport => {
                    attrs.push(FileAttrValue::LinkSupport(self.hard_link_support));
                    answer_attrs.push(FileAttr::LinkSupport);
                }
                FileAttr::SymlinkSupport => {
                    attrs.push(FileAttrValue::SymlinkSupport(self.symlink_support));
                    answer_attrs.push(FileAttr::SymlinkSupport);
                }
                FileAttr::NamedAttr => {
                    attrs.push(FileAttrValue::NamedAttr(true));
                    answer_attrs.push(FileAttr::NamedAttr);
                }
                FileAttr::Acl => {
                    let aces = mode_to_acl(filehandle.attr_mode, &filehandle.attr_owner, &filehandle.attr_owner_group);
                    attrs.push(FileAttrValue::Acl(aces));
                    answer_attrs.push(FileAttr::Acl);
                }
                FileAttr::AclSupport => {
                    attrs.push(FileAttrValue::AclSupport(ACL4_SUPPORT_ALLOW_ACL));
                    answer_attrs.push(FileAttr::AclSupport);
                }
                FileAttr::Fsid => {
                    attrs.push(FileAttrValue::Fsid(filehandle.attr_fsid));
                    answer_attrs.push(FileAttr::Fsid);
                }
                FileAttr::UniqueHandles => {
                    attrs.push(FileAttrValue::UniqueHandles(self.unique_handles));
                    answer_attrs.push(FileAttr::UniqueHandles);
                }
                FileAttr::FhExpireType => {
                    attrs.push(FileAttrValue::FhExpireType(FH4_PERSISTENT));
                    answer_attrs.push(FileAttr::FhExpireType);
                }
                FileAttr::RdattrError => {
                    attrs.push(FileAttrValue::RdattrError(NfsStat4::Nfs4errInval));
                    answer_attrs.push(FileAttr::RdattrError);
                }
                FileAttr::Fileid => {
                    debug!(
                        "GETATTR Fileid: path={:?} fileid={} id[0]={:#04x}",
                        filehandle.path, filehandle.attr_fileid, filehandle.id[0]
                    );
                    attrs.push(FileAttrValue::Fileid(filehandle.attr_fileid));
                    answer_attrs.push(FileAttr::Fileid);
                }
                FileAttr::Mode => {
                    attrs.push(FileAttrValue::Mode(filehandle.attr_mode));
                    answer_attrs.push(FileAttr::Mode);
                }
                FileAttr::Numlinks => {
                    attrs.push(FileAttrValue::Numlinks(filehandle.attr_nlink));
                    answer_attrs.push(FileAttr::Numlinks);
                }
                FileAttr::Owner => {
                    attrs.push(FileAttrValue::Owner(filehandle.attr_owner.clone()));
                    answer_attrs.push(FileAttr::Owner);
                }
                FileAttr::OwnerGroup => {
                    attrs.push(FileAttrValue::OwnerGroup(
                        filehandle.attr_owner_group.clone(),
                    ));
                    answer_attrs.push(FileAttr::OwnerGroup);
                }
                FileAttr::SpaceAvail => {
                    let val = space.bytes_avail;
                    attrs.push(FileAttrValue::SpaceAvail(val));
                    answer_attrs.push(FileAttr::SpaceAvail);
                }
                FileAttr::SpaceFree => {
                    let val = space.bytes_free;
                    attrs.push(FileAttrValue::SpaceFree(val));
                    answer_attrs.push(FileAttr::SpaceFree);
                }
                FileAttr::SpaceTotal => {
                    let val = space.bytes_total;
                    attrs.push(FileAttrValue::SpaceTotal(val));
                    answer_attrs.push(FileAttr::SpaceTotal);
                }
                FileAttr::SpaceUsed => {
                    attrs.push(FileAttrValue::SpaceUsed(filehandle.attr_space_used));
                    answer_attrs.push(FileAttr::SpaceUsed);
                }
                FileAttr::TimeAccess => {
                    attrs.push(FileAttrValue::TimeAccess(filehandle.attr_time_access));
                    answer_attrs.push(FileAttr::TimeAccess);
                }
                FileAttr::TimeMetadata => {
                    attrs.push(FileAttrValue::TimeMetadata(filehandle.attr_time_metadata));
                    answer_attrs.push(FileAttr::TimeMetadata);
                }
                FileAttr::TimeModify => {
                    attrs.push(FileAttrValue::TimeModify(filehandle.attr_time_modify));
                    answer_attrs.push(FileAttr::TimeModify);
                }
                FileAttr::Maxfilesize => {
                    attrs.push(FileAttrValue::Maxfilesize(i64::MAX as u64));
                    answer_attrs.push(FileAttr::Maxfilesize);
                }
                FileAttr::Maxread => {
                    attrs.push(FileAttrValue::Maxread(1048576));
                    answer_attrs.push(FileAttr::Maxread);
                }
                FileAttr::Maxwrite => {
                    attrs.push(FileAttrValue::Maxwrite(1048576));
                    answer_attrs.push(FileAttr::Maxwrite);
                }
                FileAttr::Maxlink => {
                    attrs.push(FileAttrValue::Maxlink(32000));
                    answer_attrs.push(FileAttr::Maxlink);
                }
                FileAttr::Maxname => {
                    attrs.push(FileAttrValue::Maxname(255));
                    answer_attrs.push(FileAttr::Maxname);
                }
                FileAttr::Homogeneous => {
                    attrs.push(FileAttrValue::Homogeneous(true));
                    answer_attrs.push(FileAttr::Homogeneous);
                }
                FileAttr::NoTrunc => {
                    attrs.push(FileAttrValue::NoTrunc(true));
                    answer_attrs.push(FileAttr::NoTrunc);
                }
                FileAttr::Cansettime => {
                    attrs.push(FileAttrValue::Cansettime(true));
                    answer_attrs.push(FileAttr::Cansettime);
                }
                FileAttr::ChownRestricted => {
                    attrs.push(FileAttrValue::ChownRestricted(true));
                    answer_attrs.push(FileAttr::ChownRestricted);
                }
                FileAttr::CaseInsensitive => {
                    attrs.push(FileAttrValue::CaseInsensitive(false));
                    answer_attrs.push(FileAttr::CaseInsensitive);
                }
                FileAttr::CasePreserving => {
                    attrs.push(FileAttrValue::CasePreserving(true));
                    answer_attrs.push(FileAttr::CasePreserving);
                }
                FileAttr::FilesAvail => {
                    let val = space.files_avail;
                    attrs.push(FileAttrValue::FilesAvail(val));
                    answer_attrs.push(FileAttr::FilesAvail);
                }
                FileAttr::FilesFree => {
                    let val = space.files_free;
                    attrs.push(FileAttrValue::FilesFree(val));
                    answer_attrs.push(FileAttr::FilesFree);
                }
                FileAttr::FilesTotal => {
                    let val = space.files_total;
                    attrs.push(FileAttrValue::FilesTotal(val));
                    answer_attrs.push(FileAttr::FilesTotal);
                }
                FileAttr::FsLocations => {
                    // Local export — fs_root is "/" (root of export), no referral locations
                    attrs.push(FileAttrValue::FsLocations(FsLocations4 {
                        fs_root: vec!["/".to_string()],
                        locations: vec![],
                    }));
                    answer_attrs.push(FileAttr::FsLocations);
                }
                FileAttr::TimeDelta => {
                    // Server time granularity: 1 nanosecond
                    attrs.push(FileAttrValue::TimeDelta(Nfstime4 { seconds: 0, nseconds: 1 }));
                    answer_attrs.push(FileAttr::TimeDelta);
                }
                FileAttr::TimeCreate => {
                    // Use modify time as creation time (VFS doesn't expose birth time)
                    attrs.push(FileAttrValue::TimeCreate(filehandle.attr_time_modify));
                    answer_attrs.push(FileAttr::TimeCreate);
                }
                FileAttr::MountedOnFileid => {
                    attrs.push(FileAttrValue::MountedOnFileid(filehandle.attr_fileid));
                    answer_attrs.push(FileAttr::MountedOnFileid);
                }
                _ => {}
            }
        }
        Some((answer_attrs, attrs))
    }

    /// Resolve an NFSv4 owner string to a numeric uid.
    /// Handles: "1000", "user", "user@domain", "1000@domain"
    pub(crate) fn resolve_nfs4_uid(owner: &str) -> Option<u32> {
        if let Ok(uid) = owner.parse::<u32>() {
            return Some(uid);
        }
        let name = owner.split('@').next().unwrap_or(owner);
        if let Ok(uid) = name.parse::<u32>() {
            return Some(uid);
        }
        // NSS lookup via getpwnam_r
        let c_name = std::ffi::CString::new(name).ok()?;
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let ret = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            )
        };
        if ret == 0 && !result.is_null() {
            return Some(pwd.pw_uid);
        }
        None
    }

    /// Resolve an NFSv4 owner_group string to a numeric gid.
    /// Handles: "1000", "group", "group@domain", "1000@domain"
    pub(crate) fn resolve_nfs4_gid(group: &str) -> Option<u32> {
        if let Ok(gid) = group.parse::<u32>() {
            return Some(gid);
        }
        let name = group.split('@').next().unwrap_or(group);
        if let Ok(gid) = name.parse::<u32>() {
            return Some(gid);
        }
        // NSS lookup via getgrnam_r
        let c_name = std::ffi::CString::new(name).ok()?;
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 4096];
        let mut result: *mut libc::group = std::ptr::null_mut();
        let ret = unsafe {
            libc::getgrnam_r(
                c_name.as_ptr(),
                &mut grp,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            )
        };
        if ret == 0 && !result.is_null() {
            return Some(grp.gr_gid);
        }
        None
    }

    /// SETATTR size on a vfs-only file (MemoryFS in unit tests): read it,
    /// cut or zero-extend it to `size`, write it back.
    fn vfs_set_len(file: &VfsPath, size: u64) -> std::io::Result<()> {
        use std::io::{Read, Write};
        let io = |e: vfs::VfsError| std::io::Error::other(e.to_string());
        let mut data = Vec::new();
        file.open_file().map_err(io)?.read_to_end(&mut data)?;
        data.resize(size as usize, 0);
        let mut out = file.create_file().map_err(io)?;
        out.write_all(&data)?;
        out.flush()
    }

    pub fn set_attr(
        &self,
        filehandle: &Filehandle,
        attr_vals: &Attrlist4<FileAttrValue>,
    ) -> Attrlist4<FileAttr> {
        let mut attrsset = Attrlist4::<FileAttr>::new(None);
        // Every change is made on the object resolved beneath the export
        // root at the moment of the change (#123). Owner, group and times
        // are set on a symlink itself, never on what it points to; size and
        // mode are refused on one (ELOOP; the dispatcher already answers
        // NFS4ERR_INVAL, #122).
        let path = filehandle.file.as_str();
        let root = &self.beneath;
        for attr in attr_vals.iter() {
            match attr {
                FileAttrValue::Size(size) => {
                    debug!("Set size to: {:?}", size);
                    let result = match root.set_len(path, *size) {
                        // MemoryFS (unit tests) has no file under the root.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound
                            && self.vfs_only
                            && filehandle.file.exists().unwrap_or(false) =>
                        {
                            Self::vfs_set_len(&filehandle.file, *size)
                        }
                        other => other,
                    };
                    match result {
                        Ok(()) => attrsset.push(FileAttr::Size),
                        Err(e) => error!("SETATTR size failed: {}", e),
                    }
                }
                FileAttrValue::Owner(uid_str) => {
                    if let Some(uid) = Self::resolve_nfs4_uid(uid_str) {
                        match root.lchown(path, uid, u32::MAX) {
                            Ok(()) => attrsset.push(FileAttr::Owner),
                            Err(e) => error!("SETATTR chown(uid={}) failed: {}", uid, e),
                        }
                    } else {
                        error!("SETATTR Owner: cannot resolve {:?} to uid", uid_str);
                    }
                }
                FileAttrValue::OwnerGroup(gid_str) => {
                    if let Some(gid) = Self::resolve_nfs4_gid(gid_str) {
                        match root.lchown(path, u32::MAX, gid) {
                            Ok(()) => attrsset.push(FileAttr::OwnerGroup),
                            Err(e) => error!("SETATTR chown(gid={}) failed: {}", gid, e),
                        }
                    } else {
                        error!("SETATTR OwnerGroup: cannot resolve {:?} to gid", gid_str);
                    }
                }
                FileAttrValue::Mode(mode) => match root.chmod(path, *mode) {
                    Ok(()) => attrsset.push(FileAttr::Mode),
                    Err(e) => error!("SETATTR chmod({:#o}) failed: {}", mode, e),
                },
                FileAttrValue::TimeModifySet => {
                    // SET_TO_SERVER_TIME4: atime and mtime to the server's time.
                    match root.set_times(path, None, None) {
                        Ok(()) => attrsset.push(FileAttr::TimeModifySet),
                        Err(e) => error!("SETATTR utimensat failed: {}", e),
                    }
                }
                _ => {
                    debug!("Not supported set attr requested for: {:?}", attr);
                }
            }
        }
        attrsset
    }

    pub fn attr_lease_time(&self) -> NfsLease4 {
        self.lease_time
    }

    pub fn attr_rdattr_error(&self) -> NfsStat4 {
        // rdattr_error:
        // The server uses this to specify the behavior of the client when
        // reading attributes.  See Section 4 for additional description.
        NfsStat4::Nfs4errInval
    }

    pub fn attr_supported_attrs(&self) -> Attrlist4<FileAttr> {
        // supported_attrs:
        // The bit vector that would retrieve all REQUIRED and RECOMMENDED
        // attributes that are supported for this object.  The scope of this
        //attribute applies to all objects with a matching fsid.
        Attrlist4::<FileAttr>::new(Some(vec![
            FileAttr::SupportedAttrs,
            FileAttr::Type,
            FileAttr::FhExpireType,
            FileAttr::Change,
            FileAttr::Size,
            FileAttr::LinkSupport,
            FileAttr::SymlinkSupport,
            FileAttr::NamedAttr,
            FileAttr::Fsid,
            FileAttr::UniqueHandles,
            FileAttr::LeaseTime,
            FileAttr::RdattrError,
            FileAttr::Acl,
            FileAttr::AclSupport,
            FileAttr::Archive,
            FileAttr::Cansettime,
            FileAttr::CaseInsensitive,
            FileAttr::CasePreserving,
            FileAttr::ChownRestricted,
            FileAttr::Filehandle,
            FileAttr::Fileid,
            FileAttr::FilesAvail,
            FileAttr::FilesFree,
            FileAttr::FilesTotal,
            FileAttr::FsLocations,
            FileAttr::Homogeneous,
            FileAttr::Maxfilesize,
            FileAttr::Maxlink,
            FileAttr::Maxname,
            FileAttr::Maxread,
            FileAttr::Maxwrite,
            FileAttr::Mode,
            FileAttr::NoTrunc,
            FileAttr::Numlinks,
            FileAttr::Owner,
            FileAttr::OwnerGroup,
            FileAttr::SpaceAvail,
            FileAttr::SpaceFree,
            FileAttr::SpaceTotal,
            FileAttr::SpaceUsed,
            FileAttr::TimeAccess,
            FileAttr::TimeCreate,
            FileAttr::TimeDelta,
            FileAttr::TimeMetadata,
            FileAttr::TimeModify,
            FileAttr::MountedOnFileid,
        ]))
    }

}

pub enum WriteCacheMessage {
    Write(WriteBytesRequest),
    Commit(CommitRequest),
}

pub struct WriteBytesRequest {
    // seek offset
    pub offset: u64,
    // bytes to insert
    pub data: Vec<u8>,
    /// The write's outcome, sent once the bytes are in the file (page
    /// cache) or the write failed (#136, finding 8e411131c3ba).
    pub respond_to: Option<oneshot::Sender<Result<u64, WriteCacheError>>>,
}

pub struct CommitRequest {
    /// COMMIT reports (and so clears) a failure kept from an earlier flush;
    /// CLOSE's flush does not, so the client's COMMIT still sees it.
    pub report_pending: bool,
    pub respond_to: Option<oneshot::Sender<Result<u64, WriteCacheError>>>,
}

/// Why an unstable WRITE or a COMMIT through the write cache failed.
#[derive(Debug)]
pub enum WriteCacheError {
    /// The write or fsync failed with this error.
    Io(std::io::Error),
    /// The file the cache was opened for is gone from beneath the export
    /// root (the handle is stale).
    Gone,
    /// The cache actor is not running.
    ActorGone,
}

impl WriteCacheError {
    /// The status of a failed unstable WRITE: the errno's own (NOSPC,
    /// DQUOT, FBIG, …), STALE for a file that is gone.
    pub fn write_status(&self) -> NfsStat4 {
        match self {
            WriteCacheError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => NfsStat4::Nfs4errStale,
            WriteCacheError::Io(e) => crate::server::createattrs::io_status(e),
            WriteCacheError::Gone => NfsStat4::Nfs4errStale,
            WriteCacheError::ActorGone => NfsStat4::Nfs4errServerfault,
        }
    }

    /// The status of a failed COMMIT, from the statuses RFC 8881 allows
    /// COMMIT (no NOSPC there): ISDIR, STALE, else IO.
    pub fn commit_status(&self) -> NfsStat4 {
        match self {
            WriteCacheError::Io(e) => match e.raw_os_error() {
                Some(libc::EISDIR) => NfsStat4::Nfs4errIsdir,
                Some(libc::ENOENT) => NfsStat4::Nfs4errStale,
                _ => NfsStat4::Nfs4errIo,
            },
            WriteCacheError::Gone => NfsStat4::Nfs4errStale,
            WriteCacheError::ActorGone => NfsStat4::Nfs4errServerfault,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WriteCacheHandle {
    sender: mpsc::Sender<WriteCacheMessage>,
}

impl WriteCacheHandle {
    pub fn new(
        filehandle: Filehandle,
        filemanager: FileManagerHandle,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        let write_cache = WriteCache::new(receiver, filehandle, filemanager);
        tokio::spawn(run_file_write_cache(write_cache));

        Self { sender }
    }

    /// Write `data` at `offset`; Ok carries the write epoch the bytes were
    /// written under (`FileManagerHandle::write_verifier`).
    pub async fn write_bytes(&self, offset: u64, data: Vec<u8>) -> Result<u64, WriteCacheError> {
        let (tx, rx) = oneshot::channel();
        let msg = WriteCacheMessage::Write(WriteBytesRequest { offset, data, respond_to: Some(tx) });
        if let Err(e) = self.sender.send(msg).await {
            error!("write cache actor gone: {}", e);
            return Err(WriteCacheError::ActorGone);
        }
        rx.await.unwrap_or(Err(WriteCacheError::ActorGone))
    }

    /// COMMIT: fsync the file. Ok carries the write epoch after it; an
    /// fsync failure, or one kept from an earlier flush, is the Err.
    pub async fn commit(&self) -> Result<u64, WriteCacheError> {
        self.sync(true).await
    }

    /// CLOSE's flush: like `commit`, but a failure stays pending in the
    /// cache for the next COMMIT, which reports it.
    pub async fn flush(&self) -> Result<u64, WriteCacheError> {
        self.sync(false).await
    }

    async fn sync(&self, report_pending: bool) -> Result<u64, WriteCacheError> {
        let (tx, rx) = oneshot::channel();
        let msg = WriteCacheMessage::Commit(CommitRequest { report_pending, respond_to: Some(tx) });
        if let Err(e) = self.sender.send(msg).await {
            error!("write cache actor gone: {}", e);
            return Err(WriteCacheError::ActorGone);
        }
        rx.await.unwrap_or(Err(WriteCacheError::ActorGone))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::{MemoryFS, VfsPath};

    fn make_fm() -> FileManagerHandle {
        let vfs_root = VfsPath::new(MemoryFS::new());
        FileManagerHandle::new(vfs_root, Some(1), PathBuf::from("/tmp"))
    }

    // ── FileManager actor tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_get_root_filehandle() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();
        assert_eq!(root.file.as_str(), "");
    }

    #[tokio::test]
    async fn test_get_root_filehandle_is_dir() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();
        assert!(root.file.is_dir().unwrap());
    }

    #[tokio::test]
    async fn test_get_filehandle_for_nonexistent_path() {
        let fm = make_fm();
        let result = fm.get_filehandle_for_path("nonexistent".to_string()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().nfs_error, NfsStat4::Nfs4errNoent);
    }

    #[tokio::test]
    async fn test_get_filehandle_for_invalid_id() {
        let fm = make_fm();
        let bad_id: NfsFh4 = [0xDE; 26];
        let result = fm.get_filehandle_for_id(bad_id).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().nfs_error, NfsStat4::Nfs4errStale);
    }

    #[tokio::test]
    async fn test_create_and_get_file() {
        let fm = make_fm();
        // Create a file via the handle — uses the FM's internal VFS
        let _fh = fm
            .create_file(
                VfsPath::new(MemoryFS::new()).join("testfile").unwrap(),
                1,
                b"owner1".to_vec(),
                1,
                0,
                None,
            )
            .await;
        let root = fm.get_root_filehandle().await.unwrap();
        assert!(root.file.is_dir().unwrap());
    }

    #[tokio::test]
    async fn test_get_filehandle_attrs_type() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();
        let (attr_names, attr_vals) = fm
            .get_filehandle_attrs(root.id, vec![FileAttr::Type])
            .await
            .unwrap();
        assert_eq!(attr_names.len(), 1);
        assert_eq!(attr_names[0], FileAttr::Type);
        assert_eq!(attr_vals.len(), 1);
    }

    #[tokio::test]
    async fn test_get_filehandle_attrs_multiple() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();
        let attrs = vec![
            FileAttr::Type,
            FileAttr::Mode,
            FileAttr::Size,
            FileAttr::Owner,
            FileAttr::OwnerGroup,
        ];
        let (attr_names, attr_vals) = fm
            .get_filehandle_attrs(root.id, attrs)
            .await
            .unwrap();
        assert_eq!(attr_names.len(), 5);
        assert_eq!(attr_vals.len(), 5);
    }

    #[tokio::test]
    async fn test_get_filehandle_attrs_bad_handle() {
        let fm = make_fm();
        let bad_id: NfsFh4 = [0xAA; 26];
        let result = fm
            .get_filehandle_attrs(bad_id, vec![FileAttr::Type])
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().nfs_error, NfsStat4::Nfs4errBadhandle);
    }

    #[tokio::test]
    async fn test_root_filehandle_stable_id() {
        let fm = make_fm();
        let root1 = fm.get_root_filehandle().await.unwrap();
        let root2 = fm.get_root_filehandle().await.unwrap();
        assert_eq!(root1.id, root2.id);
    }

    // ── Lock tests via FileManagerHandle ─────────────────────────────

    #[tokio::test]
    async fn test_lock_and_unlock() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Lock a range
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        let stateid = match result {
            LockResult::Ok(sid) => sid,
            other => panic!("Expected LockResult::Ok, got {:?}", other),
        };

        // Unlock
        let unlock_result = fm.unlock_file(stateid.other, 0, 100).await;
        match unlock_result {
            UnlockResult::Ok(sid) => assert_eq!(sid.seqid, stateid.seqid + 1),
            other => panic!("Expected UnlockResult::Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_unlock_bad_stateid() {
        let fm = make_fm();
        let bad_stateid = [0xFF; 12];
        let result = fm.unlock_file(bad_stateid, 0, 100).await;
        match result {
            UnlockResult::Error(status) => assert_eq!(status, NfsStat4::Nfs4errBadStateid),
            other => panic!("Expected UnlockResult::Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_lock_conflict_write_vs_write() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // First write lock
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));

        // Second write lock from different owner — should conflict
        let result = fm
            .lock_file(root.id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 50, 100)
            .await;
        assert!(matches!(result, LockResult::Denied { .. }));
    }

    #[tokio::test]
    async fn test_lock_no_conflict_read_vs_read() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // First read lock
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::ReadLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));

        // Second read lock from different owner — no conflict
        let result = fm
            .lock_file(root.id, 2, b"owner2".to_vec(), NfsLockType4::ReadLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));
    }

    #[tokio::test]
    async fn test_lock_conflict_read_vs_write() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Read lock
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::ReadLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));

        // Write lock from different owner — should conflict
        let result = fm
            .lock_file(root.id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Denied { .. }));
    }

    #[tokio::test]
    async fn test_lock_no_conflict_non_overlapping() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Write lock on [0, 100)
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));

        // Write lock on [200, 300) — no overlap
        let result = fm
            .lock_file(root.id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 200, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));
    }

    #[tokio::test]
    async fn test_lock_same_owner_no_conflict() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Write lock from owner1
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));

        // Another write lock from same owner/client — should NOT conflict
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 50, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));
    }

    #[tokio::test]
    async fn test_lock_zero_length_to_eof() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Lock from 0 to EOF (length=0 means entire file)
        let result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 0)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));

        // Any other lock should conflict
        let result = fm
            .lock_file(root.id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 999999, 1)
            .await;
        assert!(matches!(result, LockResult::Denied { .. }));
    }

    #[tokio::test]
    async fn test_test_lock_no_conflict() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        let result = fm
            .test_lock(root.id, 1, b"owner1".to_vec(), NfsLockType4::ReadLt, 0, 100)
            .await;
        assert!(matches!(result, TestLockResult::Ok));
    }

    #[tokio::test]
    async fn test_test_lock_detects_conflict() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Acquire write lock
        let lock_result = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        assert!(matches!(lock_result, LockResult::Ok(_)));

        // Test lock from different owner — should see conflict
        let result = fm
            .test_lock(root.id, 2, b"owner2".to_vec(), NfsLockType4::ReadLt, 50, 50)
            .await;
        assert!(matches!(result, TestLockResult::Denied { .. }));
    }

    #[tokio::test]
    async fn test_release_lock_owner() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Acquire two locks
        let r1 = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        assert!(matches!(r1, LockResult::Ok(_)));
        let r2 = fm
            .lock_file(root.id, 1, b"owner1".to_vec(), NfsLockType4::ReadLt, 200, 100)
            .await;
        assert!(matches!(r2, LockResult::Ok(_)));

        // Release all locks for owner1
        let status = fm.release_lock_owner(1, b"owner1".to_vec()).await;
        assert_eq!(status, NfsStat4::Nfs4Ok);

        // Now a different owner should be able to lock the same range
        let result = fm
            .lock_file(root.id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        assert!(matches!(result, LockResult::Ok(_)));
    }

    // ── Write cache / file lifecycle tests ───────────────────────────

    #[tokio::test]
    async fn test_get_write_cache_handle() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();
        let wch = fm.get_write_cache_handle(root).await;
        assert!(wch.is_ok());
    }

    #[tokio::test]
    async fn test_remove_nonexistent_file() {
        let fm = make_fm();
        let vfs_root = VfsPath::new(MemoryFS::new());
        // Removing a nonexistent file returns an error (correct behavior)
        let path = vfs_root.join("no_such_file").unwrap();
        let result = fm.remove_file(path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_touch_nonexistent_file() {
        let fm = make_fm();
        let bad_id: NfsFh4 = [0xBB; 26];
        // touch on a bad id should not panic
        fm.touch_file(bad_id).await;
        // If we get here, it didn't panic
    }

    #[test]
    fn test_mode_to_acl_755() {
        let aces = mode_to_acl(0o755, "1000", "1000");
        assert_eq!(aces.len(), 3);
        // Owner (rwx=7): should have read+write+execute
        assert!(aces[0].access_mask & ACE4_READ_DATA != 0);
        assert!(aces[0].access_mask & ACE4_WRITE_DATA != 0);
        assert!(aces[0].access_mask & ACE4_EXECUTE != 0);
        assert_eq!(aces[0].who, "1000");
        // Group (r-x=5): read+execute but not write
        assert!(aces[1].access_mask & ACE4_READ_DATA != 0);
        assert!(aces[1].access_mask & ACE4_WRITE_DATA == 0);
        assert!(aces[1].access_mask & ACE4_EXECUTE != 0);
        assert_eq!(aces[1].flag, ACE4_IDENTIFIER_GROUP);
        // Everyone (r-x=5): read+execute
        assert!(aces[2].access_mask & ACE4_READ_DATA != 0);
        assert!(aces[2].access_mask & ACE4_EXECUTE != 0);
        assert_eq!(aces[2].who, "EVERYONE@");
    }

    #[test]
    fn test_mode_to_acl_644() {
        let aces = mode_to_acl(0o644, "0", "0");
        // Owner (rw-=6): read+write, no execute
        assert!(aces[0].access_mask & ACE4_READ_DATA != 0);
        assert!(aces[0].access_mask & ACE4_WRITE_DATA != 0);
        assert!(aces[0].access_mask & ACE4_EXECUTE == 0);
        // Group (r--=4): read only
        assert!(aces[1].access_mask & ACE4_READ_DATA != 0);
        assert!(aces[1].access_mask & ACE4_WRITE_DATA == 0);
        // Everyone (r--=4): read only
        assert!(aces[2].access_mask & ACE4_READ_DATA != 0);
        assert!(aces[2].access_mask & ACE4_WRITE_DATA == 0);
    }

    #[test]
    fn test_mode_to_acl_000() {
        let aces = mode_to_acl(0o000, "0", "0");
        // No rwx bits — only base attributes (read_attributes, read_acl, synchronize)
        assert!(aces[0].access_mask & ACE4_READ_DATA == 0);
        assert!(aces[0].access_mask & ACE4_WRITE_DATA == 0);
        assert!(aces[0].access_mask & ACE4_EXECUTE == 0);
        // But should still have base attributes
        assert!(aces[0].access_mask & ACE4_READ_ATTRIBUTES != 0);
    }

    #[tokio::test]
    async fn test_close_file_releases_stateid() {
        let fm = make_fm();
        // Create a file via OPEN (creates an open lock in lockdb)
        let root = fm.get_root_filehandle().await.unwrap();
        let new_fh = fm
            .create_file(
                root.file.join("closeable.txt").unwrap(),
                1,
                b"owner1".to_vec(),
                1,
                0,
                None,
            )
            .await;
        assert!(new_fh.is_ok());
        let fh = new_fh.unwrap();
        assert_eq!(fh.locks.len(), 1);
        let open_stateid = fh.locks[0].stateid;

        // Close the file — should remove the stateid from lockdb
        fm.close_file(open_stateid).await;

        // Verify: re-fetch the filehandle and check locks are gone
        let refreshed = fm.get_filehandle_for_id(fh.id).await.unwrap();
        assert!(refreshed.locks.is_empty(), "open stateid should be removed after close");
    }

    /// Regression test for #35: get_filehandle_for_path must attach existing
    /// locks from lockdb to the returned filehandle.
    #[tokio::test]
    async fn test_get_filehandle_for_path_attaches_locks() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Create a file via create_file (this adds a lock to lockdb)
        let fh = fm
            .create_file(
                root.file.join("lock_attach_test.txt").unwrap(),
                1,
                b"owner1".to_vec(),
                1,
                0,
                None,
            )
            .await
            .unwrap();
        assert_eq!(fh.locks.len(), 1, "create_file should return fh with 1 lock");
        let open_stateid = fh.locks[0].stateid;

        // Now retrieve the same file via get_filehandle_for_path
        let fh2 = fm
            .get_filehandle_for_path("lock_attach_test.txt".to_string())
            .await
            .unwrap();

        // The retrieved filehandle MUST have the lock attached
        assert!(
            !fh2.locks.is_empty(),
            "get_filehandle_for_path must attach existing locks (regression #35)"
        );
        assert_eq!(
            fh2.locks[0].stateid, open_stateid,
            "attached lock stateid must match the one from create_file"
        );
    }

    /// Regression test for #35: get_filehandle_for_id must also attach locks.
    #[tokio::test]
    async fn test_get_filehandle_for_id_attaches_locks() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        let fh = fm
            .create_file(
                root.file.join("id_lock_test.txt").unwrap(),
                1,
                b"owner1".to_vec(),
                1,
                0,
                None,
            )
            .await
            .unwrap();
        assert_eq!(fh.locks.len(), 1);
        let fh_id = fh.id;

        // Retrieve by ID
        let fh2 = fm.get_filehandle_for_id(fh_id).await.unwrap();
        assert!(
            !fh2.locks.is_empty(),
            "get_filehandle_for_id must attach existing locks (regression #35)"
        );
    }

    /// Regression test for #34: CLOSE of one file must not break locks on
    /// a different file for the same client.
    #[tokio::test]
    async fn test_close_does_not_break_other_files_locks() {
        let fm = make_fm();
        let root = fm.get_root_filehandle().await.unwrap();

        // Create two files
        let fh1 = fm
            .create_file(
                root.file.join("file_a.txt").unwrap(),
                1,
                b"owner1".to_vec(),
                1,
                0,
                None,
            )
            .await
            .unwrap();
        let fh2 = fm
            .create_file(
                root.file.join("file_b.txt").unwrap(),
                1,
                b"owner1".to_vec(),
                1,
                0,
                None,
            )
            .await
            .unwrap();

        assert_eq!(fh1.locks.len(), 1);
        assert_eq!(fh2.locks.len(), 1);
        let stateid1 = fh1.locks[0].stateid;
        let stateid2 = fh2.locks[0].stateid;

        // Close file_a
        fm.close_file(stateid1).await;

        // file_b's lock must still be intact
        let fh2_refreshed = fm.get_filehandle_for_id(fh2.id).await.unwrap();
        assert!(
            !fh2_refreshed.locks.is_empty(),
            "CLOSE of file_a must not remove file_b's locks (regression #34)"
        );
        assert_eq!(fh2_refreshed.locks[0].stateid, stateid2);
    }
}
