use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::info;
use vfs::{AltrootFS, PhysicalFS, VfsPath};

use super::filemanager::FileManagerHandle;

/// Per-export statistics.
#[derive(Debug)]
pub struct ExportStats {
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    pub ops: AtomicU64,
}

impl ExportStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            ops: AtomicU64::new(0),
        })
    }

    pub fn snapshot(&self) -> ExportStatsSnapshot {
        ExportStatsSnapshot {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            ops: self.ops.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExportStatsSnapshot {
    pub reads: u64,
    pub writes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub ops: u64,
}

/// State for a single export.
#[derive(Debug, Clone)]
pub struct ExportInfo {
    pub export_id: u8,
    pub name: String,
    pub path: PathBuf,
    pub read_only: bool,
    pub stats: Arc<ExportStats>,
}

/// Full export state including the FileManagerHandle.
struct ExportState {
    pub info: ExportInfo,
    pub _vfs_root: VfsPath,
    pub file_manager: FileManagerHandle,
}

// Messages to the ExportManager actor
enum ExportManagerMessage {
    AddExport(AddExportRequest),
    RemoveExport(RemoveExportRequest),
    ListExports(ListExportsRequest),
    GetExportById(GetExportByIdRequest),
    GetExportByName(GetExportByNameRequest),
}

struct AddExportRequest {
    name: String,
    path: PathBuf,
    read_only: bool,
    respond_to: oneshot::Sender<Result<ExportInfo, String>>,
}

struct RemoveExportRequest {
    name: String,
    respond_to: oneshot::Sender<Result<(), String>>,
}

struct ListExportsRequest {
    respond_to: oneshot::Sender<Vec<ExportInfo>>,
}

struct GetExportByIdRequest {
    export_id: u8,
    respond_to: oneshot::Sender<Option<(ExportInfo, FileManagerHandle)>>,
}

struct GetExportByNameRequest {
    name: String,
    respond_to: oneshot::Sender<Option<(ExportInfo, FileManagerHandle)>>,
}

struct ExportManager {
    exports: HashMap<String, ExportState>,
    export_by_id: HashMap<u8, String>,
    next_export_id: u8,
    receiver: mpsc::Receiver<ExportManagerMessage>,
}

impl ExportManager {
    fn new(receiver: mpsc::Receiver<ExportManagerMessage>) -> Self {
        Self {
            exports: HashMap::new(),
            export_by_id: HashMap::new(),
            next_export_id: 1, // 0x00 reserved, 0xFF = pseudo-root
            receiver,
        }
    }

    fn handle_message(&mut self, msg: ExportManagerMessage) {
        match msg {
            ExportManagerMessage::AddExport(req) => {
                let result = self.add_export(req.name, req.path, req.read_only);
                let _ = req.respond_to.send(result);
            }
            ExportManagerMessage::RemoveExport(req) => {
                let result = self.remove_export(&req.name);
                let _ = req.respond_to.send(result);
            }
            ExportManagerMessage::ListExports(req) => {
                let list: Vec<ExportInfo> =
                    self.exports.values().map(|s| s.info.clone()).collect();
                let _ = req.respond_to.send(list);
            }
            ExportManagerMessage::GetExportById(req) => {
                let result = self.export_by_id.get(&req.export_id).and_then(|name| {
                    self.exports
                        .get(name)
                        .map(|s| (s.info.clone(), s.file_manager.clone()))
                });
                let _ = req.respond_to.send(result);
            }
            ExportManagerMessage::GetExportByName(req) => {
                let result = self.exports.get(&req.name).map(|s| {
                    (s.info.clone(), s.file_manager.clone())
                });
                let _ = req.respond_to.send(result);
            }
        }
    }

    fn add_export(
        &mut self,
        name: String,
        path: PathBuf,
        read_only: bool,
    ) -> Result<ExportInfo, String> {
        if self.exports.contains_key(&name) {
            return Err(format!("export '{}' already exists", name));
        }
        if self.next_export_id >= 0xFE {
            return Err("maximum number of exports reached".to_string());
        }

        let canonical = path.canonicalize().map_err(|e| {
            format!("export path {} does not exist: {}", path.display(), e)
        })?;
        if !canonical.is_dir() {
            return Err(format!("{} is not a directory", canonical.display()));
        }

        let export_id = self.next_export_id;
        self.next_export_id += 1;

        let vfs_root: VfsPath =
            AltrootFS::new(VfsPath::new(PhysicalFS::new(&canonical))).into();
        let file_manager =
            FileManagerHandle::new(vfs_root.clone(), Some(export_id as u64), canonical.clone());

        let stats = ExportStats::new();
        let info = ExportInfo {
            export_id,
            name: name.clone(),
            path: canonical.clone(),
            read_only,
            stats,
        };

        let state = ExportState {
            info: info.clone(),
            _vfs_root: vfs_root,
            file_manager,
        };

        self.exports.insert(name.clone(), state);
        self.export_by_id.insert(export_id, name.clone());
        info!(%name, path = %canonical.display(), export_id, "export added");
        Ok(info)
    }

    fn remove_export(&mut self, name: &str) -> Result<(), String> {
        match self.exports.remove(name) {
            Some(state) => {
                self.export_by_id.remove(&state.info.export_id);
                info!(%name, "export removed");
                Ok(())
            }
            None => Err(format!("export '{}' not found", name)),
        }
    }
}

async fn run_export_manager(mut actor: ExportManager) {
    while let Some(msg) = actor.receiver.recv().await {
        actor.handle_message(msg);
    }
}

/// Async handle to the ExportManager actor.
#[derive(Debug, Clone)]
pub struct ExportManagerHandle {
    sender: mpsc::Sender<ExportManagerMessage>,
}

impl Default for ExportManagerHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ExportManagerHandle {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(64);
        let actor = ExportManager::new(receiver);
        tokio::spawn(run_export_manager(actor));
        Self { sender }
    }

    pub async fn add_export(
        &self,
        name: String,
        path: PathBuf,
        read_only: bool,
    ) -> Result<ExportInfo, String> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(ExportManagerMessage::AddExport(AddExportRequest {
                name,
                path,
                read_only,
                respond_to: tx,
            }))
            .await
            .map_err(|_| "export manager gone".to_string())?;
        rx.await.map_err(|_| "export manager gone".to_string())?
    }

    pub async fn remove_export(&self, name: &str) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(ExportManagerMessage::RemoveExport(RemoveExportRequest {
                name: name.to_string(),
                respond_to: tx,
            }))
            .await
            .map_err(|_| "export manager gone".to_string())?;
        rx.await.map_err(|_| "export manager gone".to_string())?
    }

    pub async fn list_exports(&self) -> Vec<ExportInfo> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .send(ExportManagerMessage::ListExports(ListExportsRequest {
                respond_to: tx,
            }))
            .await;
        rx.await.unwrap_or_default()
    }

    pub async fn get_export_by_id(
        &self,
        export_id: u8,
    ) -> Option<(ExportInfo, FileManagerHandle)> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .send(ExportManagerMessage::GetExportById(GetExportByIdRequest {
                export_id,
                respond_to: tx,
            }))
            .await;
        rx.await.ok().flatten()
    }

    pub async fn get_export_by_name(
        &self,
        name: &str,
    ) -> Option<(ExportInfo, FileManagerHandle)> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .send(ExportManagerMessage::GetExportByName(
                GetExportByNameRequest {
                    name: name.to_string(),
                    respond_to: tx,
                },
            ))
            .await;
        rx.await.ok().flatten()
    }
}
