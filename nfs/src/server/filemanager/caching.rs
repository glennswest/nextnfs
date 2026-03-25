use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;

use tokio::sync::mpsc;
use tracing::{debug, error};

use super::{handle::WriteCacheMessage, FileManagerHandle, Filehandle};

#[derive(Debug)]
pub struct WriteCache {
    pub file: Option<File>,
    pub dirty: bool,
    pub filehandle: Filehandle,
    pub receiver: mpsc::Receiver<WriteCacheMessage>,
    pub filemanager: FileManagerHandle,
    pub _real_path: std::path::PathBuf,
}

impl WriteCache {
    pub fn new(
        receiver: mpsc::Receiver<WriteCacheMessage>,
        filehandle: Filehandle,
        filemanager: FileManagerHandle,
        real_path: std::path::PathBuf,
    ) -> Self {
        // Open the real file for writing — keep the fd open for the duration
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .open(&real_path);

        match &file {
            Ok(_) => debug!("WriteCache opened {:?}", real_path),
            Err(e) => error!("WriteCache failed to open {:?}: {:?}", real_path, e),
        }

        WriteCache {
            file: file.ok(),
            dirty: false,
            filehandle,
            receiver,
            filemanager,
            _real_path: real_path,
        }
    }

    pub async fn handle_message(&mut self, msg: WriteCacheMessage) {
        match msg {
            WriteCacheMessage::Write(req) => {
                if let Some(ref mut file) = self.file {
                    if let Err(e) = file.seek(SeekFrom::Start(req.offset)) {
                        error!("write seek failed: {:?}", e);
                        return;
                    }
                    if let Err(e) = file.write_all(&req.data) {
                        error!("write failed: {:?}", e);
                        return;
                    }
                    self.dirty = true;
                } else {
                    // Fallback: use VFS write path
                    if let Ok(mut vfs_file) = self.filehandle.file.append_file() {
                        let _ = vfs_file.seek(SeekFrom::Start(req.offset));
                        let _ = vfs_file.write_all(&req.data);
                        self.dirty = true;
                    }
                }
            }
            WriteCacheMessage::Commit => {
                if self.dirty {
                    if let Some(ref mut file) = self.file {
                        // fsync to ensure durability
                        if let Err(e) = file.sync_all() {
                            error!("fsync failed: {:?}", e);
                        }
                    }
                    self.dirty = false;
                    self.filemanager
                        .touch_file(self.filehandle.id)
                        .await;
                }
                self.filemanager
                    .drop_write_cache_handle(self.filehandle.id)
                    .await;
            }
        }
    }
}

pub async fn run_file_write_cache(mut actor: WriteCache) {
    while let Some(msg) = actor.receiver.recv().await {
        actor.handle_message(msg).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::handle::{WriteCacheMessage, WriteBytesRequest};
    use std::path::PathBuf;
    use tokio::sync::mpsc;
    use vfs::{MemoryFS, VfsPath};

    fn make_test_handle() -> FileManagerHandle {
        let vfs_root = VfsPath::new(MemoryFS::new());
        FileManagerHandle::new(vfs_root, Some(1), PathBuf::from("/tmp"))
    }

    fn make_test_filehandle() -> super::super::Filehandle {
        let vfs_root = VfsPath::new(MemoryFS::new());
        super::super::Filehandle::new(vfs_root, [0u8; 26], 1, 1, 0)
    }

    #[tokio::test]
    async fn test_write_cache_new_invalid_path() {
        let (_, rx) = mpsc::channel(256);
        let fh = make_test_filehandle();
        let fm = make_test_handle();
        let wc = WriteCache::new(rx, fh, fm, PathBuf::from("/nonexistent/path/file.dat"));
        assert!(wc.file.is_none());
        assert!(!wc.dirty);
    }

    #[tokio::test]
    async fn test_write_cache_new_valid_path() {
        let (_, rx) = mpsc::channel(256);
        let fh = make_test_filehandle();
        let fm = make_test_handle();
        let path = PathBuf::from("/tmp/nextnfs_test_cache_valid");
        let wc = WriteCache::new(rx, fh, fm, path.clone());
        assert!(wc.file.is_some());
        assert!(!wc.dirty);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_write_cache_write_sets_dirty() {
        let (_, rx) = mpsc::channel(256);
        let fh = make_test_filehandle();
        let fm = make_test_handle();
        let path = PathBuf::from("/tmp/nextnfs_test_cache_dirty");
        let mut wc = WriteCache::new(rx, fh, fm, path.clone());
        assert!(!wc.dirty);

        let write_msg = WriteCacheMessage::Write(WriteBytesRequest {
            offset: 0,
            data: b"hello".to_vec(),
        });
        wc.handle_message(write_msg).await;
        assert!(wc.dirty);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_write_cache_vfs_fallback() {
        let (_, rx) = mpsc::channel(256);
        let vfs = VfsPath::new(MemoryFS::new());
        let _ = vfs.join("testfile").unwrap().create_file();
        let fh = super::super::Filehandle::new(
            vfs.join("testfile").unwrap(), [0u8; 26], 1, 1, 0,
        );
        let fm = make_test_handle();
        let mut wc = WriteCache::new(rx, fh, fm, PathBuf::from("/nonexistent/fallback"));
        assert!(wc.file.is_none());

        let write_msg = WriteCacheMessage::Write(WriteBytesRequest {
            offset: 0,
            data: b"vfs_data".to_vec(),
        });
        wc.handle_message(write_msg).await;
        assert!(wc.dirty);
    }
}
