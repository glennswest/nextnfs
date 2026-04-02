use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error};
use vfs::VfsPath;

use std::path::PathBuf;

use nextnfs_proto::nfs4_proto::{
    Attrlist4, FileAttr, FileAttrValue, NfsLease4, NfsLockType4, Nfsace4, NfsStat4, Nfstime4,
    Stateid4, ACE4_ACCESS_ALLOWED_ACE_TYPE, ACE4_APPEND_DATA, ACE4_DELETE, ACE4_DELETE_CHILD,
    ACE4_EXECUTE, ACE4_IDENTIFIER_GROUP, ACE4_READ_ACL, ACE4_READ_ATTRIBUTES, ACE4_READ_DATA,
    ACE4_SYNCHRONIZE, ACE4_WRITE_ACL, ACE4_WRITE_ATTRIBUTES, ACE4_WRITE_DATA,
    ACE4_WRITE_OWNER, ACL4_SUPPORT_ALLOW_ACL, FH4_PERSISTENT, FsLocations4,
};

use super::{
    caching::run_file_write_cache, caching::WriteCache, filehandle::Filehandle,
    locking::LockingState, run_file_manager, FileManager,
};
use crate::server::filemanager::NfsFh4;

pub enum FileManagerMessage {
    GetRootFilehandle(GetRootFilehandleRequest),
    GetFilehandle(GetFilehandleRequest),
    GetFilehandleAttrs(GetFilehandleAttrsRequest),
    CreateFile(CreateFileRequest),
    CreateOpenState(CreateOpenStateRequest),
    RemoveFile(RemoveFileRequest),
    TouchFile(TouchFileRequest),
    UpdateFilehandle(Filehandle),
    LockFile(LockFileRequest),
    UnlockFile(UnlockFileRequest),
    TestLock(TestLockRequest),
    ReleaseLockOwner(ReleaseLockOwnerRequest),
    OpenNamedAttrDir(OpenNamedAttrDirRequest),
    CloseFile(),
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
    pub respond_to: oneshot::Sender<Option<Filehandle>>,
}

/// Request to create open state on an existing file (CLAIM_PREVIOUS reclaim).
pub struct CreateOpenStateRequest {
    pub path: VfsPath,
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub share_access: u32,
    pub share_deny: u32,
    pub respond_to: oneshot::Sender<Option<LockingState>>,
}

pub struct RemoveFileRequest {
    pub path: VfsPath,
    pub respond_to: oneshot::Sender<()>,
}

pub struct TouchFileRequest {
    pub id: NfsFh4,
}

pub struct WriteCacheHandleRequest {
    pub filemanager: FileManagerHandle,
    pub filehandle: Filehandle,
    pub respond_to: oneshot::Sender<WriteCacheHandle>,
}

pub struct DropCacheHandleRequest {
    pub filehandle_id: NfsFh4,
}

pub struct OpenNamedAttrDirRequest {
    pub fileid: u64,
    pub createdir: bool,
    pub respond_to: oneshot::Sender<Option<Filehandle>>,
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

/// Export-level quota and space info passed to filehandle_attrs for GETATTR.
#[derive(Debug, Clone, Default)]
pub struct QuotaInfo {
    pub quota_avail_hard: u64,
    pub quota_avail_soft: u64,
    pub quota_used: u64,
    pub space_avail: u64,
    pub space_free: u64,
    pub space_total: u64,
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
    lease_time: u32,
    hard_link_support: bool,
    symlink_support: bool,
    unique_handles: bool,
}

impl FileManagerHandle {
    pub fn new(root: VfsPath, fsid: Option<u64>, export_root: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        let fmanager = FileManager::new(receiver, root, fsid, export_root);
        tokio::spawn(run_file_manager(fmanager));

        Self {
            sender,
            lease_time: 90,
            hard_link_support: true,
            symlink_support: true,
            unique_handles: true,
        }
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
            Ok(fh) => {
                if let Some(fh) = fh {
                    return Ok(fh);
                }
                Err(FileManagerError {
                    nfs_error: NfsStat4::Nfs4errNoent,
                })
            }
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    /// Create open state on an existing file (for CLAIM_PREVIOUS reclaim).
    /// Returns the LockingState with stateid for the reclaimed open.
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
            Ok(Some(lock)) => Ok(lock),
            Ok(None) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errStale,
            }),
            Err(_) => Err(FileManagerError {
                nfs_error: NfsStat4::Nfs4errServerfault,
            }),
        }
    }

    pub async fn remove_file(&self, path: VfsPath) -> Result<(), FileManagerError> {
        let (tx, rx) = oneshot::channel();
        if let Err(e) = self.sender.send(FileManagerMessage::RemoveFile(RemoveFileRequest {
            path,
            respond_to: tx,
        })).await {
            error!("filemanager actor gone: {}", e);
            return Err(FileManagerError { nfs_error: NfsStat4::Nfs4errServerfault });
        }
        match rx.await {
            Ok(_) => Ok(()),
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

    pub async fn drop_write_cache_handle(&self, filehandle_id: NfsFh4) {
        if let Err(e) = self.sender.send(FileManagerMessage::DropWriteCacheHandle(
            DropCacheHandleRequest { filehandle_id },
        )).await {
            error!("filemanager actor gone: {}", e);
        }
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
        quota_info: Option<&QuotaInfo>,
    ) -> Option<(Attrlist4<FileAttr>, Attrlist4<FileAttrValue>)> {
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
                FileAttr::QuotaAvailHard => {
                    let val = quota_info.map_or(0, |q| q.quota_avail_hard);
                    attrs.push(FileAttrValue::QuotaAvailHard(val));
                    answer_attrs.push(FileAttr::QuotaAvailHard);
                }
                FileAttr::QuotaAvailSoft => {
                    let val = quota_info.map_or(0, |q| q.quota_avail_soft);
                    attrs.push(FileAttrValue::QuotaAvailSoft(val));
                    answer_attrs.push(FileAttr::QuotaAvailSoft);
                }
                FileAttr::QuotaUsed => {
                    let val = quota_info.map_or(0, |q| q.quota_used);
                    attrs.push(FileAttrValue::QuotaUsed(val));
                    answer_attrs.push(FileAttr::QuotaUsed);
                }
                FileAttr::SpaceAvail => {
                    let val = quota_info.map_or(0, |q| q.space_avail);
                    attrs.push(FileAttrValue::SpaceAvail(val));
                    answer_attrs.push(FileAttr::SpaceAvail);
                }
                FileAttr::SpaceFree => {
                    let val = quota_info.map_or(0, |q| q.space_free);
                    attrs.push(FileAttrValue::SpaceFree(val));
                    answer_attrs.push(FileAttr::SpaceFree);
                }
                FileAttr::SpaceTotal => {
                    let val = quota_info.map_or(0, |q| q.space_total);
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
                    let val = quota_info.map_or(u64::MAX, |q| {
                        if q.space_total == 0 { u64::MAX } else { q.space_avail / 4096 }
                    });
                    attrs.push(FileAttrValue::FilesAvail(val));
                    answer_attrs.push(FileAttr::FilesAvail);
                }
                FileAttr::FilesFree => {
                    let val = quota_info.map_or(u64::MAX, |q| {
                        if q.space_total == 0 { u64::MAX } else { q.space_free / 4096 }
                    });
                    attrs.push(FileAttrValue::FilesFree(val));
                    answer_attrs.push(FileAttr::FilesFree);
                }
                FileAttr::FilesTotal => {
                    let val = quota_info.map_or(u64::MAX, |q| {
                        if q.space_total == 0 { u64::MAX } else { q.space_total / 4096 }
                    });
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

    pub fn set_attr(
        &self,
        filehandle: &Filehandle,
        attr_vals: &Attrlist4<FileAttrValue>,
    ) -> Attrlist4<FileAttr> {
        let mut attrsset = Attrlist4::<FileAttr>::new(None);
        for attr in attr_vals.iter() {
            match attr {
                FileAttrValue::Size(args) => {
                    debug!("Set size to: {:?}", args);
                    let mut buf = vec![0_u8; *args as usize];
                    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                        let mut file = filehandle.file.open_file()?;
                        let _ = file.rewind();
                        file.read_exact(&mut buf)?;

                        let mut file = filehandle.file.append_file()?;
                        let _ = file.rewind();
                        file.write_all(&buf)?;
                        file.flush()?;
                        Ok(())
                    })();
                    match result {
                        Ok(_) => attrsset.push(FileAttr::Size),
                        Err(e) => error!("SETATTR size failed: {}", e),
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
            FileAttr::QuotaAvailHard,
            FileAttr::QuotaAvailSoft,
            FileAttr::QuotaUsed,
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
    Commit,
}

pub struct WriteBytesRequest {
    // seek offset
    pub offset: u64,
    // bytes to insert
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct WriteCacheHandle {
    sender: mpsc::Sender<WriteCacheMessage>,
}

impl WriteCacheHandle {
    pub fn new(
        filehandle: Filehandle,
        filemanager: FileManagerHandle,
        real_path: std::path::PathBuf,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        let write_cache = WriteCache::new(receiver, filehandle, filemanager, real_path);
        tokio::spawn(run_file_write_cache(write_cache));

        Self { sender }
    }

    pub async fn write_bytes(&self, offset: u64, data: Vec<u8>) {
        if let Err(e) = self.sender.send(WriteCacheMessage::Write(WriteBytesRequest { offset, data })).await {
            error!("write cache actor gone: {}", e);
        }
    }

    pub async fn commit(&self) {
        if let Err(e) = self.sender.send(WriteCacheMessage::Commit).await {
            error!("write cache actor gone: {}", e);
        }
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
        // Removing a nonexistent file shouldn't panic
        let path = vfs_root.join("no_such_file").unwrap();
        let result = fm.remove_file(path).await;
        // The actor handles this gracefully (just calls remove_file on VFS)
        assert!(result.is_ok());
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
}
