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
    pub real_path: std::path::PathBuf,
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
            real_path,
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
                        .touch_file(self.filehandle.id.clone())
                        .await;
                }
                self.filemanager
                    .drop_write_cache_handle(self.filehandle.id.clone())
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
