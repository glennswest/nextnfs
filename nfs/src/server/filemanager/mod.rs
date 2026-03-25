use std::collections::HashMap;
use std::path::PathBuf;

use nextnfs_proto::nfs4_proto::{
    Attrlist4, FileAttr, FileAttrValue, NfsFh4, NfsStat4, ACL4_SUPPORT_ALLOW_ACL,
    FH4_PERSISTENT,
};

mod filehandle;
pub use filehandle::Filehandle;
pub use filehandle::RealMeta;
pub use handle::FileManagerHandle;
pub use handle::{LockResult, TestLockResult, UnlockResult};
mod caching;
mod handle;
mod locking;

use filehandle::FilehandleDb;
use handle::{FileManagerMessage, WriteCacheHandle};
use locking::{LockType, LockingState, LockingStateDb};
use tokio::sync::mpsc;
use tracing::{debug, error};
use vfs::VfsPath;

#[derive(Debug)]
pub struct FileManager {
    pub root: VfsPath,
    pub export_root: PathBuf,
    pub lease_time: u32,
    pub hard_link_support: bool,
    pub symlink_support: bool,
    pub unique_handles: bool,
    pub fsid: u64,
    pub fhdb: FilehandleDb,
    pub next_fh_id: u128,
    pub lockdb: LockingStateDb,
    pub next_stateid_id: u64,
    pub boot_time: u64,
    pub receiver: mpsc::Receiver<FileManagerMessage>,
    pub cachedb: HashMap<NfsFh4, WriteCacheHandle>,
}

impl FileManager {
    pub fn new(
        receiver: mpsc::Receiver<FileManagerMessage>,
        root: VfsPath,
        fsid: Option<u64>,
        export_root: PathBuf,
    ) -> Self {
        let fsid = fsid.unwrap_or(152);
        let boot_time = std::time::UNIX_EPOCH.elapsed().unwrap().as_secs();
        let mut fmanager = FileManager {
            receiver,
            root: root.clone(),
            export_root,
            lease_time: 90,
            hard_link_support: true,
            symlink_support: true,
            unique_handles: true,
            boot_time,
            fsid,
            next_fh_id: 100,
            next_stateid_id: 100,
            fhdb: FilehandleDb::default(),
            lockdb: LockingStateDb::default(),
            cachedb: HashMap::new(),
        };
        fmanager.root_fh();
        fmanager
    }

    fn handle_message(&mut self, msg: FileManagerMessage) {
        match msg {
            FileManagerMessage::GetRootFilehandle(req) => {
                let fh_wo_locks = self.root_fh();
                let fh = self.attach_locks(fh_wo_locks);
                let _ = req.respond_to.send(fh);
            }
            FileManagerMessage::GetFilehandle(req) => {
                if let Some(fh_id) = req.filehandle {
                    let fh = self.get_filehandle_by_id(&fh_id);
                    match fh {
                        Some(fh_wo_locks) => {
                            let fh = self.attach_locks(fh_wo_locks);
                            let _ = req.respond_to.send(Some(fh));
                        }
                        None => {
                            debug!("Filehandle not found");
                            let _ = req.respond_to.send(None);
                        }
                    }
                } else if let Some(req_path) = req.path {
                    let path = match self.root.join(req_path) {
                        Ok(p) => p,
                        Err(_) => {
                            let _ = req.respond_to.send(None);
                            return;
                        }
                    };
                    if path.exists().unwrap_or(false) {
                        let fh_wo_locks = self.get_filehandle(&path);
                        let fh = self.attach_locks(fh_wo_locks);
                        let _ = req.respond_to.send(Some(fh));
                    } else {
                        debug!("File not found {:?}", path);
                        let _ = req.respond_to.send(None);
                    }
                } else {
                    let fh_wo_locks = self.root_fh();
                    let fh = self.attach_locks(fh_wo_locks);
                    let _ = req.respond_to.send(Some(fh));
                }
            }
            FileManagerMessage::GetFilehandleAttrs(req) => {
                let _ = req.respond_to
                    .send(self.filehandle_attrs(&req.attrs_request, &req.filehandle_id));
            }
            FileManagerMessage::CreateFile(req) => {
                let fh = self.create_file(&req.path);
                if let Some(mut fh) = fh {
                    let stateid = self.get_new_lockingstate_id();
                    let lock = LockingState::new_shared_reservation(
                        fh.id,
                        stateid,
                        req.client_id,
                        req.owner,
                        req.share_access,
                        req.share_deny,
                    );
                    self.lockdb.insert(lock.clone());
                    fh.locks = vec![lock];
                    let _ = req.respond_to.send(Some(fh));
                } else {
                    let _ = req.respond_to.send(None);
                }
            }
            FileManagerMessage::LockFile(req) => {
                let result = self.handle_lock(
                    &req.filehandle_id,
                    req.client_id,
                    req.owner,
                    req.lock_type,
                    req.offset,
                    req.length,
                );
                let _ = req.respond_to.send(result);
            }
            FileManagerMessage::UnlockFile(req) => {
                let result = self.handle_unlock(&req.lock_stateid, req.offset, req.length);
                let _ = req.respond_to.send(result);
            }
            FileManagerMessage::TestLock(req) => {
                let result = self.handle_test_lock(
                    &req.filehandle_id,
                    req.client_id,
                    &req.owner,
                    &req.lock_type,
                    req.offset,
                    req.length,
                );
                let _ = req.respond_to.send(result);
            }
            FileManagerMessage::ReleaseLockOwner(req) => {
                let result = self.handle_release_lock_owner(req.client_id, &req.owner);
                let _ = req.respond_to.send(result);
            }
            FileManagerMessage::CloseFile() => {},
            FileManagerMessage::RemoveFile(req) => {
                let filehandle = self.get_filehandle_by_path(&req.path.as_str().to_string());
                let mut parent_path = req.path.parent().as_str().to_string();
                match filehandle {
                    Some(filehandle) => {
                        if req.path.is_dir().unwrap_or(false) {
                            let _ = req.path.remove_dir();
                        } else {
                            let _ = req.path.remove_file();
                        }
                        self.fhdb.remove_by_id(&filehandle.id);
                    }
                    None => {
                        if req.path.is_dir().unwrap_or(false) {
                            let _ = req.path.remove_dir();
                        } else {
                            let _ = req.path.remove_file();
                        }
                    }
                }

                if parent_path.is_empty() {
                    parent_path = "/".to_string();
                }

                if let Some(parent_filehandle) = self.get_filehandle_by_path(&parent_path) {
                    self.touch_filehandle(parent_filehandle);
                }
                let _ = req.respond_to.send(());
            }
            FileManagerMessage::TouchFile(req) => {
                let filehandle = self.get_filehandle_by_id(&req.id);
                if let Some(filehandle) = filehandle {
                    self.touch_filehandle(filehandle);
                }
            }
            FileManagerMessage::GetWriteCacheHandle(req) => {
                let handle = self.get_cache_handle(req.filehandle, req.filemanager);
                let _ = req.respond_to.send(handle);
            }
            FileManagerMessage::DropWriteCacheHandle(req) => {
                self.drop_cache_handle(&req.filehandle_id);
            }
            FileManagerMessage::UpdateFilehandle(req) => {
                self.update_filehandle(req);
            }
        }
    }

    fn real_path(&self, vfs_path: &VfsPath) -> PathBuf {
        let rel = vfs_path.as_str().to_string();
        if rel.is_empty() || rel == "/" {
            self.export_root.clone()
        } else {
            self.export_root.join(rel.trim_start_matches('/'))
        }
    }

    fn touch_filehandle(&mut self, filehandle: Filehandle) {
        let real_path = self.real_path(&filehandle.file);
        let fh = if let Some(meta) = RealMeta::from_path(&real_path) {
            Filehandle::new_real(
                filehandle.file.clone(),
                filehandle.id,
                self.fsid,
                self.fsid,
                filehandle.version,
                &meta,
            )
        } else {
            Filehandle::new(
                filehandle.file.clone(),
                filehandle.id,
                self.fsid,
                self.fsid,
                filehandle.version,
            )
        };
        self.fhdb.remove_by_id(&filehandle.id);
        debug!("Touching filehandle: {:?}", fh);
        self.fhdb.insert(fh);
    }

    fn update_filehandle(&mut self, filehandle: Filehandle) {
        debug!("Updating filehandle: {:?}", &filehandle);
        self.fhdb.remove_by_id(&filehandle.id);
        self.fhdb.insert(filehandle);
    }

    fn create_file(&mut self, request_file: &VfsPath) -> Option<Filehandle> {
        match request_file.create_file() {
            Ok(_) => debug!("File created successfully"),
            Err(e) => {
                error!("Error creating file {:?}", e);
                return None;
            }
        };

        let fh = self.get_filehandle(request_file);
        let mut path = request_file.parent().as_str().to_string();
        if path.is_empty() {
            path = "/".to_string();
        }
        if let Some(parent_filehandle) = self.get_filehandle_by_path(&path) {
            self.touch_filehandle(parent_filehandle);
        }
        Some(fh)
    }

    fn get_new_lockingstate_id(&mut self) -> [u8; 12] {
        let mut id = vec![0_u8, 0_u8, 0_u8, 0_u8];
        id.extend(self.next_stateid_id.to_be_bytes().to_vec());
        self.next_stateid_id += 1;
        id.try_into().expect("stateid is always 12 bytes")
    }

    fn get_filehandle_id(&mut self, file: &VfsPath) -> NfsFh4 {
        let mut path = file.as_str().to_string();
        if path.is_empty() {
            path = "/".to_string();
        }
        let exists = self.get_filehandle_by_path(&path);
        if let Some(exists) = exists {
            return exists.id;
        }

        // NfsFh4 is [u8; 26] — pack dev:ino into 26 bytes
        let real_path = self.real_path(file);
        if let Some(meta) = RealMeta::from_path(&real_path) {
            let mut id = [0u8; 26];
            id[0] = 0x01; // version: inode-based persistent handle
            id[1] = 0x00; // reserved
            id[2..10].copy_from_slice(&meta.dev.to_be_bytes());
            id[10..18].copy_from_slice(&meta.ino.to_be_bytes());
            // bytes 18..26 are zero padding
            debug!("created inode-based filehandle: dev={} ino={}", meta.dev, meta.ino);
            return id;
        }

        // Fallback: volatile handle using boot_time + sequence
        let mut id = [0u8; 26];
        id[0] = 0x80; // version: volatile
        id[1] = 0x00;
        id[2..10].copy_from_slice(&self.boot_time.to_be_bytes());
        let seq_bytes = self.next_fh_id.to_be_bytes();
        id[10..26].copy_from_slice(&seq_bytes);
        debug!("created volatile filehandle: seq={}", self.next_fh_id);
        self.next_fh_id += 1;
        id
    }

    fn get_filehandle_by_id(&mut self, id: &NfsFh4) -> Option<Filehandle> {
        let fh = self.fhdb.get_by_id(id);
        if let Some(fh) = fh {
            if fh.file.exists().unwrap_or(false) {
                return Some(fh.clone());
            } else {
                self.fhdb.remove_by_id(id);
            }
        }
        None
    }

    pub fn get_filehandle_by_path(&self, path: &String) -> Option<Filehandle> {
        self.fhdb.get_by_path(path).cloned()
    }

    pub fn get_filehandle(&mut self, file: &VfsPath) -> Filehandle {
        let id = self.get_filehandle_id(file);
        match self.get_filehandle_by_id(&id) {
            Some(fh) => fh.clone(),
            None => {
                let real_path = self.real_path(file);
                let fh = if let Some(meta) = RealMeta::from_path(&real_path) {
                    Filehandle::new_real(file.clone(), id, self.fsid, self.fsid, 0, &meta)
                } else {
                    Filehandle::new(file.clone(), id, self.fsid, self.fsid, 0)
                };
                self.fhdb.insert(fh.clone());
                fh
            }
        }
    }

    pub fn root_fh(&mut self) -> Filehandle {
        self.get_filehandle(&self.root.clone())
    }

    pub fn attach_locks(&self, mut filehandle: Filehandle) -> Filehandle {
        let locks = self.lockdb.get_by_filehandle_id(&filehandle.id);
        filehandle.locks = locks.into_iter().cloned().collect();
        filehandle
    }

    pub fn get_cache_handle(
        &mut self,
        mut filehandle: Filehandle,
        filemanager: FileManagerHandle,
    ) -> WriteCacheHandle {
        if let Some(cached) = self.cachedb.get(&filehandle.id) {
            cached.clone()
        } else {
            let real_path = self.real_path(&filehandle.file);
            let handle = WriteCacheHandle::new(filehandle.clone(), filemanager, real_path);
            filehandle.write_cache = Some(handle.clone());
            self.cachedb.insert(filehandle.id, handle.clone());
            self.update_filehandle(filehandle);
            handle
        }
    }

    pub fn drop_cache_handle(&mut self, filehandle_id: &NfsFh4) {
        if self.cachedb.contains_key(filehandle_id) {
            self.cachedb.remove(filehandle_id);
        }
        let filehandle = self.get_filehandle_by_id(filehandle_id);
        if let Some(mut filehandle) = filehandle {
            filehandle.write_cache = None;
            self.update_filehandle(filehandle);
        }
    }

    pub fn filehandle_attrs(
        &mut self,
        attr_request: &Vec<FileAttr>,
        filehandle_id: &NfsFh4,
    ) -> Option<(Vec<FileAttr>, Vec<FileAttrValue>)> {
        let mut answer_attrs = Vec::new();
        let mut attrs = Vec::new();

        let fh = self.get_filehandle_by_id(filehandle_id);
        let fh = match fh {
            Some(old_fh) => {
                let real_path = self.real_path(&old_fh.file);
                if let Some(meta) = RealMeta::from_path(&real_path) {
                    let refreshed = Filehandle::new_real(
                        old_fh.file.clone(),
                        old_fh.id,
                        self.fsid,
                        self.fsid,
                        old_fh.version,
                        &meta,
                    );
                    self.fhdb.remove_by_id(&old_fh.id);
                    self.fhdb.insert(refreshed.clone());
                    refreshed
                } else {
                    old_fh
                }
            }
            None => return None,
        };

        for fileattr in attr_request {
            match fileattr {
                FileAttr::SupportedAttrs => {
                    attrs.push(FileAttrValue::SupportedAttrs(self.attr_supported_attrs()));
                    answer_attrs.push(FileAttr::SupportedAttrs);
                }
                FileAttr::Type => {
                    attrs.push(FileAttrValue::Type(fh.attr_type));
                    answer_attrs.push(FileAttr::Type);
                }
                FileAttr::LeaseTime => {
                    attrs.push(FileAttrValue::LeaseTime(self.lease_time));
                    answer_attrs.push(FileAttr::LeaseTime);
                }
                FileAttr::Change => {
                    attrs.push(FileAttrValue::Change(fh.attr_change));
                    answer_attrs.push(FileAttr::Change);
                }
                FileAttr::Size => {
                    attrs.push(FileAttrValue::Size(fh.attr_size));
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
                    attrs.push(FileAttrValue::NamedAttr(false));
                    answer_attrs.push(FileAttr::NamedAttr);
                }
                FileAttr::AclSupport => {
                    attrs.push(FileAttrValue::AclSupport(ACL4_SUPPORT_ALLOW_ACL));
                    answer_attrs.push(FileAttr::AclSupport);
                }
                FileAttr::Fsid => {
                    attrs.push(FileAttrValue::Fsid(fh.attr_fsid));
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
                    attrs.push(FileAttrValue::Fileid(fh.attr_fileid));
                    answer_attrs.push(FileAttr::Fileid);
                }
                FileAttr::Mode => {
                    attrs.push(FileAttrValue::Mode(fh.attr_mode));
                    answer_attrs.push(FileAttr::Mode);
                }
                FileAttr::Numlinks => {
                    attrs.push(FileAttrValue::Numlinks(fh.attr_nlink));
                    answer_attrs.push(FileAttr::Numlinks);
                }
                FileAttr::Owner => {
                    attrs.push(FileAttrValue::Owner(fh.attr_owner.clone()));
                    answer_attrs.push(FileAttr::Owner);
                }
                FileAttr::OwnerGroup => {
                    attrs.push(FileAttrValue::OwnerGroup(fh.attr_owner_group.clone()));
                    answer_attrs.push(FileAttr::OwnerGroup);
                }
                FileAttr::SpaceUsed => {
                    attrs.push(FileAttrValue::SpaceUsed(fh.attr_space_used));
                    answer_attrs.push(FileAttr::SpaceUsed);
                }
                FileAttr::TimeAccess => {
                    attrs.push(FileAttrValue::TimeAccess(fh.attr_time_access));
                    answer_attrs.push(FileAttr::TimeAccess);
                }
                FileAttr::TimeMetadata => {
                    attrs.push(FileAttrValue::TimeMetadata(fh.attr_time_metadata));
                    answer_attrs.push(FileAttr::TimeMetadata);
                }
                FileAttr::TimeModify => {
                    attrs.push(FileAttrValue::TimeModify(fh.attr_time_modify));
                    answer_attrs.push(FileAttr::TimeModify);
                }
                _ => {}
            }
        }
        Some((answer_attrs, attrs))
    }

    fn handle_lock(
        &mut self,
        filehandle_id: &NfsFh4,
        client_id: u64,
        owner: Vec<u8>,
        lock_type: nextnfs_proto::nfs4_proto::NfsLockType4,
        offset: u64,
        length: u64,
    ) -> LockResult {
        // Check for conflicts against all existing byte-range locks on this file
        let existing_locks: Vec<LockingState> = self
            .lockdb
            .get_by_filehandle_id(filehandle_id)
            .into_iter()
            .cloned()
            .collect();

        for lock in &existing_locks {
            if lock.conflicts_with(offset, length, &lock_type, &owner, client_id) {
                return LockResult::Denied {
                    offset: lock.start.unwrap_or(0),
                    length: lock.length.unwrap_or(0),
                    lock_type: lock.nfs_lock_type.clone().unwrap_or(
                        nextnfs_proto::nfs4_proto::NfsLockType4::ReadLt,
                    ),
                    owner_clientid: lock.client_id,
                    owner: lock.owner.clone(),
                };
            }
        }

        // No conflict — grant the lock
        let stateid = self.get_new_lockingstate_id();
        let lock = LockingState::new_byte_range_lock(
            *filehandle_id,
            stateid,
            client_id,
            owner,
            lock_type,
            offset,
            length,
        );
        let seqid = lock.seqid;
        self.lockdb.insert(lock);

        LockResult::Ok(nextnfs_proto::nfs4_proto::Stateid4 {
            seqid,
            other: stateid,
        })
    }

    fn handle_unlock(
        &mut self,
        lock_stateid: &[u8; 12],
        _offset: u64,
        _length: u64,
    ) -> UnlockResult {
        // Find the lock by stateid
        let lock = self.lockdb.get_by_stateid(lock_stateid);
        match lock {
            Some(lock) => {
                let new_seqid = lock.seqid + 1;
                let stateid_copy = lock.stateid;
                self.lockdb.remove_by_stateid(lock_stateid);
                UnlockResult::Ok(nextnfs_proto::nfs4_proto::Stateid4 {
                    seqid: new_seqid,
                    other: stateid_copy,
                })
            }
            None => UnlockResult::Error(NfsStat4::Nfs4errBadStateid),
        }
    }

    fn handle_test_lock(
        &self,
        filehandle_id: &NfsFh4,
        client_id: u64,
        owner: &[u8],
        lock_type: &nextnfs_proto::nfs4_proto::NfsLockType4,
        offset: u64,
        length: u64,
    ) -> TestLockResult {
        let existing_locks: Vec<&LockingState> = self
            .lockdb
            .get_by_filehandle_id(filehandle_id)
            .into_iter()
            .collect();

        for lock in &existing_locks {
            if lock.conflicts_with(offset, length, lock_type, owner, client_id) {
                return TestLockResult::Denied {
                    offset: lock.start.unwrap_or(0),
                    length: lock.length.unwrap_or(0),
                    lock_type: lock.nfs_lock_type.clone().unwrap_or(
                        nextnfs_proto::nfs4_proto::NfsLockType4::ReadLt,
                    ),
                    owner_clientid: lock.client_id,
                    owner: lock.owner.clone(),
                };
            }
        }

        TestLockResult::Ok
    }

    fn handle_release_lock_owner(&mut self, client_id: u64, owner: &[u8]) -> NfsStat4 {
        // Remove all byte-range locks for this owner
        let locks: Vec<LockingState> = self
            .lockdb
            .get_by_client_id(&client_id)
            .into_iter()
            .filter(|l| matches!(l.lock_type, LockType::ByteRange) && l.owner == owner)
            .cloned()
            .collect();

        for lock in locks {
            self.lockdb.remove_by_stateid(&lock.stateid);
        }

        NfsStat4::Nfs4Ok
    }

    pub fn attr_supported_attrs(&self) -> Attrlist4<FileAttr> {
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
            FileAttr::Filehandle,
            FileAttr::Fileid,
            FileAttr::Mode,
            FileAttr::Numlinks,
            FileAttr::Owner,
            FileAttr::OwnerGroup,
            FileAttr::SpaceUsed,
            FileAttr::TimeAccess,
            FileAttr::TimeMetadata,
            FileAttr::TimeModify,
        ]))
    }
}

pub async fn run_file_manager(mut actor: FileManager) {
    while let Some(msg) = actor.receiver.recv().await {
        actor.handle_message(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nextnfs_proto::nfs4_proto::NfsLockType4;
    use tokio::sync::mpsc;
    use vfs::MemoryFS;

    fn make_fm() -> FileManager {
        let (_tx, rx) = mpsc::channel(256);
        let root = VfsPath::new(MemoryFS::new());
        FileManager::new(rx, root, Some(42), PathBuf::from("/tmp"))
    }

    #[test]
    fn test_new_defaults() {
        let fm = make_fm();
        assert_eq!(fm.lease_time, 90);
        assert!(fm.hard_link_support);
        assert!(fm.symlink_support);
        assert!(fm.unique_handles);
        assert_eq!(fm.fsid, 42);
        assert_eq!(fm.next_fh_id, 100);
        assert_eq!(fm.next_stateid_id, 100);
        assert!(fm.cachedb.is_empty());
    }

    #[test]
    fn test_new_default_fsid() {
        let (_tx, rx) = mpsc::channel(256);
        let root = VfsPath::new(MemoryFS::new());
        let fm = FileManager::new(rx, root, None, PathBuf::from("/tmp"));
        assert_eq!(fm.fsid, 152);
    }

    #[test]
    fn test_root_fh_is_root_path() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        assert_eq!(root.path, "/");
    }

    #[test]
    fn test_root_fh_stable_id() {
        let mut fm = make_fm();
        let r1 = fm.root_fh();
        let r2 = fm.root_fh();
        assert_eq!(r1.id, r2.id);
    }

    #[test]
    fn test_create_file_returns_filehandle() {
        let mut fm = make_fm();
        let newfile = fm.root.join("testfile").unwrap();
        let fh = fm.create_file(&newfile);
        assert!(fh.is_some());
        let fh = fh.unwrap();
        assert_eq!(fh.path, "/testfile");
    }

    #[test]
    fn test_create_file_in_subdir() {
        let mut fm = make_fm();
        let _ = fm.root.join("subdir").unwrap().create_dir();
        let newfile = fm.root.join("subdir").unwrap().join("file.txt").unwrap();
        let fh = fm.create_file(&newfile);
        assert!(fh.is_some());
    }

    #[test]
    fn test_get_filehandle_by_path_nonexistent() {
        let fm = make_fm();
        let result = fm.get_filehandle_by_path(&"nonexistent".to_string());
        assert!(result.is_none());
    }

    #[test]
    fn test_get_filehandle_by_path_root() {
        let fm = make_fm();
        let result = fm.get_filehandle_by_path(&"/".to_string());
        assert!(result.is_some());
    }

    #[test]
    fn test_get_filehandle_by_id_nonexistent() {
        let mut fm = make_fm();
        let bad_id = [0xFF; 26];
        let result = fm.get_filehandle_by_id(&bad_id);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_filehandle_by_id_valid() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let result = fm.get_filehandle_by_id(&root.id);
        assert!(result.is_some());
        assert_eq!(result.unwrap().path, "/");
    }

    #[test]
    fn test_get_new_lockingstate_id_increments() {
        let mut fm = make_fm();
        let id1 = fm.get_new_lockingstate_id();
        let id2 = fm.get_new_lockingstate_id();
        assert_ne!(id1, id2);
        assert_eq!(&id1[0..4], &[0, 0, 0, 0]);
        assert_eq!(fm.next_stateid_id, 102);
    }

    #[test]
    fn test_attr_supported_attrs_count() {
        let fm = make_fm();
        let attrs = fm.attr_supported_attrs();
        assert!(attrs.len() >= 20);
    }

    #[test]
    fn test_real_path_root() {
        let fm = make_fm();
        let root_path = fm.real_path(&fm.root);
        assert_eq!(root_path, PathBuf::from("/tmp"));
    }

    #[test]
    fn test_real_path_subpath() {
        let fm = make_fm();
        let subpath = fm.root.join("subdir").unwrap();
        let real = fm.real_path(&subpath);
        assert_eq!(real, PathBuf::from("/tmp/subdir"));
    }

    #[test]
    fn test_handle_lock_success() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let result = fm.handle_lock(
            &root.id, 1, b"owner1".to_vec(),
            NfsLockType4::WriteLt, 0, 100,
        );
        assert!(matches!(result, LockResult::Ok(_)));
    }

    #[test]
    fn test_handle_lock_conflict() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let _ = fm.handle_lock(&root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100);
        let result = fm.handle_lock(&root.id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 50, 50);
        assert!(matches!(result, LockResult::Denied { .. }));
    }

    #[test]
    fn test_handle_unlock_success() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let lock_result = fm.handle_lock(&root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100);
        let stateid = match lock_result {
            LockResult::Ok(s) => s,
            _ => panic!("Expected Ok"),
        };
        let result = fm.handle_unlock(&stateid.other, 0, 100);
        assert!(matches!(result, UnlockResult::Ok(_)));
    }

    #[test]
    fn test_handle_unlock_bad_stateid() {
        let mut fm = make_fm();
        let result = fm.handle_unlock(&[0xFF; 12], 0, 100);
        match result {
            UnlockResult::Error(s) => assert_eq!(s, NfsStat4::Nfs4errBadStateid),
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_handle_test_lock_no_conflict() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let result = fm.handle_test_lock(&root.id, 1, b"owner1", &NfsLockType4::ReadLt, 0, 100);
        assert!(matches!(result, TestLockResult::Ok));
    }

    #[test]
    fn test_handle_test_lock_detects_conflict() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let _ = fm.handle_lock(&root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100);
        let result = fm.handle_test_lock(&root.id, 2, b"owner2", &NfsLockType4::ReadLt, 50, 50);
        assert!(matches!(result, TestLockResult::Denied { .. }));
    }

    #[test]
    fn test_handle_release_lock_owner() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let _ = fm.handle_lock(&root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100);
        let status = fm.handle_release_lock_owner(1, b"owner1");
        assert_eq!(status, NfsStat4::Nfs4Ok);
        // Verify lock was released — another owner can lock same range
        let result = fm.handle_lock(&root.id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 0, 100);
        assert!(matches!(result, LockResult::Ok(_)));
    }

    #[test]
    fn test_attach_locks_empty_for_new_fh() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let attached = fm.attach_locks(root);
        assert!(attached.locks.is_empty());
    }

    #[test]
    fn test_attach_locks_includes_locks() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let _ = fm.handle_lock(&root.id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100);
        let root2 = fm.root_fh();
        let attached = fm.attach_locks(root2);
        assert_eq!(attached.locks.len(), 1);
    }

    #[test]
    fn test_touch_filehandle_reinserts() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        fm.touch_filehandle(root);
        // After touch, the filehandle should still exist in the db
        let refreshed = fm.get_filehandle_by_path(&"/".to_string());
        assert!(refreshed.is_some());
    }

    #[test]
    fn test_update_filehandle_replaces() {
        let mut fm = make_fm();
        let mut root = fm.root_fh();
        root.attr_size = 9999;
        fm.update_filehandle(root);
        let updated = fm.get_filehandle_by_path(&"/".to_string()).unwrap();
        assert_eq!(updated.attr_size, 9999);
    }

    #[test]
    fn test_filehandle_attrs_returns_requested() {
        let mut fm = make_fm();
        let root = fm.root_fh();
        let result = fm.filehandle_attrs(
            &vec![FileAttr::Type, FileAttr::Size, FileAttr::Mode],
            &root.id,
        );
        assert!(result.is_some());
        let (keys, vals) = result.unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(vals.len(), 3);
    }

    #[test]
    fn test_filehandle_attrs_nonexistent_id() {
        let mut fm = make_fm();
        let result = fm.filehandle_attrs(&vec![FileAttr::Type], &[0xFF; 26]);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_filehandle_id_volatile_fallback() {
        let mut fm = make_fm();
        // VFS path exists in memory but no real path → volatile handle
        let subdir = fm.root.join("vdir").unwrap();
        let _ = subdir.create_dir();
        let id = fm.get_filehandle_id(&subdir);
        // Volatile handles start with 0x80
        assert_eq!(id[0], 0x80);
    }

    #[test]
    fn test_drop_cache_handle_no_panic() {
        let mut fm = make_fm();
        // Dropping a cache for a non-cached filehandle should not panic
        fm.drop_cache_handle(&[0xAA; 26]);
    }
}
