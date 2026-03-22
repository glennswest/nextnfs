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
mod caching;
mod handle;
mod locking;

use filehandle::FilehandleDb;
use handle::{FileManagerMessage, WriteCacheHandle};
use locking::{LockingState, LockingStateDb};
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
                req.respond_to.send(fh).unwrap();
            }
            FileManagerMessage::GetFilehandle(req) => {
                if req.filehandle.is_some() {
                    let fh = self.get_filehandle_by_id(&req.filehandle.unwrap());
                    match fh {
                        Some(fh_wo_locks) => {
                            let fh = self.attach_locks(fh_wo_locks);
                            req.respond_to.send(Some(fh)).unwrap();
                        }
                        None => {
                            debug!("Filehandle not found");
                            req.respond_to.send(None).unwrap();
                        }
                    }
                } else if req.path.is_some() {
                    let path = self.root.join(req.path.unwrap()).unwrap();
                    if path.exists().unwrap() {
                        let fh_wo_locks = self.get_filehandle(&path);
                        let fh = self.attach_locks(fh_wo_locks);
                        req.respond_to.send(Some(fh)).unwrap();
                    } else {
                        debug!("File not found {:?}", path);
                        req.respond_to.send(None).unwrap();
                    }
                } else {
                    let fh_wo_locks = self.root_fh();
                    let fh = self.attach_locks(fh_wo_locks);
                    req.respond_to.send(Some(fh)).unwrap();
                }
            }
            FileManagerMessage::GetFilehandleAttrs(req) => {
                req.respond_to
                    .send(self.filehandle_attrs(&req.attrs_request, &req.filehandle_id))
                    .unwrap();
            }
            FileManagerMessage::CreateFile(req) => {
                let fh = self.create_file(&req.path);
                if let Some(mut fh) = fh {
                    let stateid = self.get_new_lockingstate_id();
                    let lock = LockingState::new_shared_reservation(
                        fh.id.clone(),
                        stateid,
                        req.client_id,
                        req.owner,
                        req.share_access,
                        req.share_deny,
                    );
                    self.lockdb.insert(lock.clone());
                    fh.locks = vec![lock];
                    req.respond_to.send(Some(fh)).unwrap();
                } else {
                    req.respond_to.send(None).unwrap();
                }
            }
            FileManagerMessage::LockFile() => todo!(),
            FileManagerMessage::CloseFile() => todo!(),
            FileManagerMessage::RemoveFile(req) => {
                let filehandle = self.get_filehandle_by_path(&req.path.as_str().to_string());
                let mut parent_path = req.path.parent().as_str().to_string();
                match filehandle {
                    Some(filehandle) => {
                        if req.path.is_dir().unwrap() {
                            let _ = req.path.read_dir();
                        } else {
                            let _ = req.path.remove_file();
                        }
                        self.fhdb.remove_by_id(&filehandle.id);
                    }
                    None => {
                        if req.path.is_dir().unwrap() {
                            let _ = req.path.read_dir();
                        } else {
                            let _ = req.path.remove_file();
                        }
                    }
                }

                if parent_path.is_empty() {
                    parent_path = "/".to_string();
                }

                let parent_filehandle = self.get_filehandle_by_path(&parent_path).unwrap();
                self.touch_filehandle(parent_filehandle);
                req.respond_to.send(()).unwrap()
            }
            FileManagerMessage::TouchFile(req) => {
                let filehandle = self.get_filehandle_by_id(&req.id);
                if let Some(filehandle) = filehandle {
                    self.touch_filehandle(filehandle);
                }
            }
            FileManagerMessage::GetWriteCacheHandle(req) => {
                let handle = self.get_cache_handle(req.filehandle, req.filemanager);
                req.respond_to.send(handle).unwrap();
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
                filehandle.id.clone(),
                self.fsid,
                self.fsid,
                filehandle.version,
                &meta,
            )
        } else {
            Filehandle::new(
                filehandle.file.clone(),
                filehandle.id.clone(),
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
        let parent_filehandle = self.get_filehandle_by_path(&path).unwrap();
        self.touch_filehandle(parent_filehandle);
        Some(fh)
    }

    fn get_new_lockingstate_id(&mut self) -> [u8; 12] {
        let mut id = vec![0_u8, 0_u8, 0_u8, 0_u8];
        id.extend(self.next_stateid_id.to_be_bytes().to_vec());
        self.next_stateid_id += 1;
        id.try_into().unwrap()
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
            if fh.file.exists().unwrap() {
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
        if self.cachedb.contains_key(&filehandle.id) {
            self.cachedb.get(&filehandle.id).unwrap().clone()
        } else {
            let handle = WriteCacheHandle::new(filehandle.clone(), filemanager);
            filehandle.write_cache = Some(handle.clone());
            self.cachedb.insert(filehandle.id.clone(), handle.clone());
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
                        old_fh.id.clone(),
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
