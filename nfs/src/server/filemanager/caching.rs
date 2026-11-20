use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;

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
}

impl WriteCache {
    pub fn new(
        receiver: mpsc::Receiver<WriteCacheMessage>,
        filehandle: Filehandle,
        filemanager: FileManagerHandle,
    ) -> Self {
        // Open the file for writing, resolved beneath the export root (no
        // symlink on the way or as the file, #122/#123); the fd stays open
        // for the cache's life, so later writes cannot be redirected.
        let path = filehandle.file.as_str().to_string();
        let file = filemanager.beneath().open_file(
            &path,
            libc::O_WRONLY | libc::O_CREAT,
            0o644,
        );

        match &file {
            Ok(_) => debug!("WriteCache opened {:?}", path),
            Err(e) => error!("WriteCache failed to open {:?}: {:?}", path, e),
        }

        WriteCache {
            file: file.ok(),
            dirty: false,
            filehandle,
            receiver,
            filemanager,
        }
    }

    pub async fn handle_message(&mut self, msg: WriteCacheMessage) {
        match msg {
            WriteCacheMessage::Write(req) => {
                if let Some(ref file) = self.file {
                    // Use pwrite (write_all_at) for atomic positional writes —
                    // avoids seek+write race under concurrent writers
                    if let Err(e) = file.write_all_at(&req.data, req.offset) {
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
            WriteCacheMessage::Commit(respond_to) => {
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
                if let Some(tx) = respond_to {
                    let _ = tx.send(());
                }
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

    fn make_test_handle(root: &std::path::Path) -> FileManagerHandle {
        let vfs_root = VfsPath::new(MemoryFS::new());
        FileManagerHandle::new(vfs_root, Some(1), PathBuf::from(root))
    }

    /// A filehandle for `path` ("/a/b"); its parents exist in MemoryFS only.
    fn make_test_filehandle(path: &str) -> super::super::Filehandle {
        let vfs_root = VfsPath::new(MemoryFS::new());
        let file = vfs_root.join(path.trim_start_matches('/')).unwrap();
        let _ = file.parent().create_dir_all();
        super::super::Filehandle::new(file, [0u8; 26], 1, 1, 0)
    }

    #[tokio::test]
    async fn test_write_cache_new_invalid_path() {
        let (_, rx) = mpsc::channel(256);
        let dir = tempfile::tempdir().unwrap();
        let fh = make_test_filehandle("/nonexistent/path/file.dat");
        let fm = make_test_handle(dir.path());
        let wc = WriteCache::new(rx, fh, fm);
        assert!(wc.file.is_none());
        assert!(!wc.dirty);
    }

    #[tokio::test]
    async fn test_write_cache_new_valid_path() {
        let (_, rx) = mpsc::channel(256);
        let dir = tempfile::tempdir().unwrap();
        let fh = make_test_filehandle("/cache_valid");
        let fm = make_test_handle(dir.path());
        let wc = WriteCache::new(rx, fh, fm);
        assert!(wc.file.is_some());
        assert!(!wc.dirty);
        assert!(dir.path().join("cache_valid").exists());
    }

    #[tokio::test]
    async fn test_write_cache_write_sets_dirty() {
        let (_, rx) = mpsc::channel(256);
        let dir = tempfile::tempdir().unwrap();
        let fh = make_test_filehandle("/cache_dirty");
        let fm = make_test_handle(dir.path());
        let mut wc = WriteCache::new(rx, fh, fm);
        assert!(!wc.dirty);

        let write_msg = WriteCacheMessage::Write(WriteBytesRequest {
            offset: 0,
            data: b"hello".to_vec(),
        });
        wc.handle_message(write_msg).await;
        assert!(wc.dirty);
        assert_eq!(std::fs::read(dir.path().join("cache_dirty")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn test_write_cache_vfs_fallback() {
        let (_, rx) = mpsc::channel(256);
        let dir = tempfile::tempdir().unwrap();
        let fh = make_test_filehandle("/memonly/testfile");
        let _ = fh.file.create_file();
        let fm = make_test_handle(dir.path());
        let mut wc = WriteCache::new(rx, fh, fm);
        assert!(wc.file.is_none());

        let write_msg = WriteCacheMessage::Write(WriteBytesRequest {
            offset: 0,
            data: b"vfs_data".to_vec(),
        });
        wc.handle_message(write_msg).await;
        assert!(wc.dirty);
    }
}
