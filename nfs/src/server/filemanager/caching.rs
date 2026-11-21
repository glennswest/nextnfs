use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;

use tokio::sync::mpsc;
use tracing::{debug, error};

use super::{
    handle::{CommitRequest, WriteBytesRequest, WriteCacheError, WriteCacheMessage},
    FileManagerHandle, Filehandle,
};

/// The unstable-WRITE cache of one file: an fd opened once beneath the
/// export root. Unstable bytes go to the file (the kernel's page cache)
/// with pwrite; COMMIT is an fsync of that fd.
///
/// Every outcome goes back to the op (#136, finding 8e411131c3ba): a failed
/// pwrite fails the WRITE; a failed fsync fails the COMMIT, keeps the cache
/// (and its dirty state) and changes the write verifier, because after a
/// failed fsync the kernel may have dropped the dirty pages, and a later
/// fsync would then succeed without them.
#[derive(Debug)]
pub struct WriteCache {
    pub file: Option<File>,
    /// Why `file` could not be opened (None while it is open).
    pub open_error: Option<io::Error>,
    pub dirty: bool,
    /// An fsync failure seen by a flush that did not report it (CLOSE);
    /// the next COMMIT reports it.
    pub pending_error: Option<io::Error>,
    pub filehandle: Filehandle,
    pub receiver: mpsc::Receiver<WriteCacheMessage>,
    pub filemanager: FileManagerHandle,
}

/// A second copy of `e` (io::Error is not Clone).
fn copy_error(e: &io::Error) -> io::Error {
    match e.raw_os_error() {
        Some(errno) => io::Error::from_raw_os_error(errno),
        None => io::Error::new(e.kind(), e.to_string()),
    }
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
        // A real export's file is never created here: a handle whose file
        // is gone is stale, and writing through it must not bring the file
        // back (#136). Only a vfs-only tree (unit tests) has its disk file
        // made on demand.
        let path = filehandle.file.as_str().to_string();
        let flags = if filemanager.vfs_only() {
            libc::O_WRONLY | libc::O_CREAT
        } else {
            libc::O_WRONLY
        };
        let file = filemanager.beneath().open_file(&path, flags, 0o644);

        let (file, open_error) = match file {
            Ok(f) => {
                debug!("WriteCache opened {:?}", path);
                (Some(f), None)
            }
            Err(e) => {
                error!("WriteCache failed to open {:?}: {:?}", path, e);
                (None, Some(e))
            }
        };

        WriteCache {
            file,
            open_error,
            dirty: false,
            pending_error: None,
            filehandle,
            receiver,
            filemanager,
        }
    }

    /// The error for a write or commit when the file is not open.
    fn not_open(&self) -> WriteCacheError {
        match &self.open_error {
            Some(e) if e.kind() == io::ErrorKind::NotFound => WriteCacheError::Gone,
            Some(e) => WriteCacheError::Io(copy_error(e)),
            None => WriteCacheError::Gone,
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<u64, WriteCacheError> {
        if let Some(ref file) = self.file {
            // pwrite (write_all_at): positional, no seek+write race.
            file.write_all_at(data, offset).map_err(WriteCacheError::Io)?;
        } else if self.filemanager.vfs_only() {
            // A vfs-only tree (MemoryFS, unit tests): through the vfs.
            let mut vfs_file = self
                .filehandle
                .file
                .append_file()
                .map_err(|e| WriteCacheError::Io(io::Error::other(e.to_string())))?;
            vfs_file
                .seek(SeekFrom::Start(offset))
                .and_then(|_| vfs_file.write_all(data))
                .and_then(|_| vfs_file.flush())
                .map_err(WriteCacheError::Io)?;
        } else {
            return Err(self.not_open());
        }
        self.dirty = true;
        Ok(self.filemanager.write_epoch())
    }

    /// fsync. Always, not only when this cache saw a write: a WRITE may
    /// have gone through an earlier cache of the same file (a filehandle
    /// still holding it), and the fsync covers the whole inode.
    async fn commit(&mut self, report_pending: bool) -> Result<u64, WriteCacheError> {
        let synced = match &self.file {
            Some(file) => file.sync_all().map_err(WriteCacheError::Io),
            None if self.filemanager.vfs_only() => Ok(()),
            None => Err(self.not_open()),
        };
        match synced {
            Err(WriteCacheError::Io(e)) => {
                error!("fsync of {:?} failed: {:?}", self.filehandle.file.as_str(), e);
                // Unstable data may be lost: no later COMMIT may vouch for
                // WRITEs answered with the old verifier. The cache and its
                // dirty state stay.
                self.filemanager.bump_write_epoch();
                if !report_pending {
                    self.pending_error = Some(copy_error(&e));
                }
                Err(WriteCacheError::Io(e))
            }
            Err(other) => Err(other),
            Ok(()) => {
                if self.dirty {
                    self.dirty = false;
                    self.filemanager.touch_file(self.filehandle.id).await;
                }
                if report_pending {
                    if let Some(e) = self.pending_error.take() {
                        return Err(WriteCacheError::Io(e));
                    }
                }
                if self.pending_error.is_none() {
                    self.filemanager
                        .drop_write_cache_handle(self.filehandle.id)
                        .await;
                }
                Ok(self.filemanager.write_epoch())
            }
        }
    }

    pub async fn handle_message(&mut self, msg: WriteCacheMessage) {
        match msg {
            WriteCacheMessage::Write(WriteBytesRequest { offset, data, respond_to }) => {
                let result = self.write(offset, &data);
                if let Err(e) = &result {
                    error!("write at {} failed: {:?}", offset, e);
                }
                if let Some(tx) = respond_to {
                    let _ = tx.send(result);
                }
            }
            WriteCacheMessage::Commit(CommitRequest { report_pending, respond_to }) => {
                let result = self.commit(report_pending).await;
                if let Some(tx) = respond_to {
                    let _ = tx.send(result);
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
            respond_to: None,
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
            respond_to: None,
        });
        wc.handle_message(write_msg).await;
        assert!(wc.dirty);
    }

    /// The write end of a pipe as a File: pwrite on it fails (ESPIPE) and
    /// so does fsync (EINVAL) — a file whose writes and flushes fail,
    /// without filling a disk.
    fn failing_file() -> (File, File) {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    async fn send_write(wc: &mut WriteCache, offset: u64, data: &[u8]) -> Result<u64, WriteCacheError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        wc.handle_message(WriteCacheMessage::Write(WriteBytesRequest {
            offset,
            data: data.to_vec(),
            respond_to: Some(tx),
        }))
        .await;
        rx.await.unwrap()
    }

    async fn send_commit(wc: &mut WriteCache, report_pending: bool) -> Result<u64, WriteCacheError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        wc.handle_message(WriteCacheMessage::Commit(CommitRequest {
            report_pending,
            respond_to: Some(tx),
        }))
        .await;
        rx.await.unwrap()
    }

    /// #136 (finding 8e411131c3ba): a failed pwrite only logged. Now it
    /// is the write's result, and the cache is not marked dirty by it.
    #[tokio::test]
    async fn test_failed_write_is_reported() {
        let (_, rx) = mpsc::channel(256);
        let dir = tempfile::tempdir().unwrap();
        let fm = make_test_handle(dir.path());
        let mut wc = WriteCache::new(rx, make_test_filehandle("/w"), fm);
        let (_read, write) = failing_file();
        wc.file = Some(write);
        match send_write(&mut wc, 0, b"x").await {
            Err(WriteCacheError::Io(e)) => assert_eq!(e.raw_os_error(), Some(libc::ESPIPE)),
            other => panic!("expected an I/O error, got {:?}", other),
        }
        assert!(!wc.dirty);
    }

    /// #136: a failed fsync was logged, the cache cleared dirty and COMMIT
    /// said OK with the same verifier. Now the COMMIT fails, the cache stays
    /// dirty, and the write epoch moves (the verifier changes), so WRITEs
    /// answered before cannot be vouched for by a later COMMIT. Once the
    /// file syncs again, COMMIT succeeds under the new epoch.
    #[tokio::test]
    async fn test_failed_fsync_fails_commit_and_changes_verifier() {
        let (_, rx) = mpsc::channel(256);
        let dir = tempfile::tempdir().unwrap();
        let fm = make_test_handle(dir.path());
        let mut wc = WriteCache::new(rx, make_test_filehandle("/c"), fm.clone());
        let before = send_write(&mut wc, 0, b"data").await.unwrap();
        assert!(wc.dirty);

        let good = wc.file.take().unwrap();
        let (_read, write) = failing_file();
        wc.file = Some(write);
        match send_commit(&mut wc, true).await {
            Err(WriteCacheError::Io(e)) => assert_eq!(e.raw_os_error(), Some(libc::EINVAL)),
            other => panic!("expected the fsync error, got {:?}", other),
        }
        assert!(wc.dirty, "a failed fsync keeps the dirty state");
        assert!(wc.pending_error.is_none(), "COMMIT reported it");
        let after = fm.write_epoch();
        assert_ne!(after, before);

        wc.file = Some(good);
        assert_eq!(send_commit(&mut wc, true).await.unwrap(), after);
        assert!(!wc.dirty);
        assert_ne!(
            FileManagerHandle::write_verifier(1, before),
            FileManagerHandle::write_verifier(1, after)
        );
    }

    /// #136: CLOSE's flush has no status to fail with, and Linux reports an
    /// fsync error once: a COMMIT after it would have fsync'd cleanly and
    /// said OK. The flush's failure is kept, the next COMMIT reports it,
    /// and only the one after that is OK.
    #[tokio::test]
    async fn test_flush_failure_is_reported_by_next_commit() {
        let (_, rx) = mpsc::channel(256);
        let dir = tempfile::tempdir().unwrap();
        let fm = make_test_handle(dir.path());
        let mut wc = WriteCache::new(rx, make_test_filehandle("/p"), fm);
        send_write(&mut wc, 0, b"data").await.unwrap();

        let good = wc.file.take().unwrap();
        let (_read, write) = failing_file();
        wc.file = Some(write);
        assert!(send_commit(&mut wc, false).await.is_err());
        assert!(wc.pending_error.is_some());

        wc.file = Some(good);
        // A second flush keeps it too.
        assert!(send_commit(&mut wc, false).await.is_ok());
        assert!(wc.pending_error.is_some());
        match send_commit(&mut wc, true).await {
            Err(WriteCacheError::Io(e)) => assert_eq!(e.raw_os_error(), Some(libc::EINVAL)),
            other => panic!("expected the kept fsync error, got {:?}", other),
        }
        assert!(wc.pending_error.is_none());
        assert!(send_commit(&mut wc, true).await.is_ok());
    }
}
