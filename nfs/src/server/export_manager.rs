use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::info;
use vfs::{AltrootFS, PhysicalFS, VfsPath};

use super::filemanager::FileManagerHandle;

/// Per-export QoS configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QosConfig {
    /// Maximum operations per second (0 = unlimited)
    pub max_ops_per_sec: u64,
    /// Maximum bytes per second for reads+writes (0 = unlimited)
    pub max_bytes_per_sec: u64,
}

impl Default for QosConfig {
    fn default() -> Self {
        Self {
            max_ops_per_sec: 0,
            max_bytes_per_sec: 0,
        }
    }
}

/// Token bucket rate limiter for QoS enforcement.
#[derive(Debug)]
pub struct RateLimiter {
    /// Tokens for operations
    ops_tokens: f64,
    /// Tokens for bytes
    bytes_tokens: f64,
    /// Last refill time
    last_refill: Instant,
    /// Configuration
    config: QosConfig,
}

impl RateLimiter {
    pub fn new(config: QosConfig) -> Self {
        Self {
            ops_tokens: config.max_ops_per_sec as f64,
            bytes_tokens: config.max_bytes_per_sec as f64,
            last_refill: Instant::now(),
            config,
        }
    }

    /// Refill tokens based on elapsed time since last refill.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        if self.config.max_ops_per_sec > 0 {
            self.ops_tokens += elapsed * self.config.max_ops_per_sec as f64;
            if self.ops_tokens > self.config.max_ops_per_sec as f64 {
                self.ops_tokens = self.config.max_ops_per_sec as f64;
            }
        }
        if self.config.max_bytes_per_sec > 0 {
            self.bytes_tokens += elapsed * self.config.max_bytes_per_sec as f64;
            if self.bytes_tokens > self.config.max_bytes_per_sec as f64 {
                self.bytes_tokens = self.config.max_bytes_per_sec as f64;
            }
        }
    }

    /// Try to consume one operation token. Returns true if allowed.
    pub fn try_consume_op(&mut self) -> bool {
        if self.config.max_ops_per_sec == 0 {
            return true; // unlimited
        }
        self.refill();
        if self.ops_tokens >= 1.0 {
            self.ops_tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Try to consume byte tokens. Returns true if allowed.
    pub fn try_consume_bytes(&mut self, bytes: u64) -> bool {
        if self.config.max_bytes_per_sec == 0 {
            return true; // unlimited
        }
        self.refill();
        let needed = bytes as f64;
        if self.bytes_tokens >= needed {
            self.bytes_tokens -= needed;
            true
        } else {
            false
        }
    }

    /// Update the QoS configuration.
    pub fn update_config(&mut self, config: QosConfig) {
        self.config = config;
    }

    /// Get the current QoS configuration.
    pub fn config(&self) -> &QosConfig {
        &self.config
    }
}

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
    /// Per-export rate limiter for QoS enforcement (shared via Arc<Mutex>)
    pub rate_limiter: Arc<Mutex<RateLimiter>>,
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
    SetQos(SetQosRequest),
    GetQos(GetQosRequest),
}

struct SetQosRequest {
    name: String,
    config: QosConfig,
    respond_to: oneshot::Sender<Result<(), String>>,
}

struct GetQosRequest {
    name: String,
    respond_to: oneshot::Sender<Option<QosConfig>>,
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
            ExportManagerMessage::SetQos(req) => {
                let result = if let Some(state) = self.exports.get(&req.name) {
                    // Update the rate limiter config (will take effect on next lock acquisition)
                    let rl = state.info.rate_limiter.clone();
                    // We can't await inside this sync handler, so spawn a task
                    tokio::spawn(async move {
                        let mut limiter = rl.lock().await;
                        limiter.update_config(req.config);
                    });
                    Ok(())
                } else {
                    Err(format!("export '{}' not found", req.name))
                };
                let _ = req.respond_to.send(result);
            }
            ExportManagerMessage::GetQos(req) => {
                if let Some(state) = self.exports.get(&req.name) {
                    let rl = state.info.rate_limiter.clone();
                    tokio::spawn(async move {
                        let limiter = rl.lock().await;
                        let _ = req.respond_to.send(Some(limiter.config().clone()));
                    });
                } else {
                    let _ = req.respond_to.send(None);
                }
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
        let rate_limiter = Arc::new(Mutex::new(RateLimiter::new(QosConfig::default())));
        let info = ExportInfo {
            export_id,
            name: name.clone(),
            path: canonical.clone(),
            read_only,
            stats,
            rate_limiter,
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

    /// Set QoS rate limiting configuration for an export.
    pub async fn set_qos(&self, name: &str, config: QosConfig) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(ExportManagerMessage::SetQos(SetQosRequest {
                name: name.to_string(),
                config,
                respond_to: tx,
            }))
            .await
            .map_err(|_| "export manager gone".to_string())?;
        rx.await.map_err(|_| "export manager gone".to_string())?
    }

    /// Get the current QoS config for an export.
    pub async fn get_qos(&self, name: &str) -> Option<QosConfig> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .send(ExportManagerMessage::GetQos(GetQosRequest {
                name: name.to_string(),
                respond_to: tx,
            }))
            .await;
        rx.await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_exports_empty() {
        let em = ExportManagerHandle::new();
        let exports = em.list_exports().await;
        assert!(exports.is_empty());
    }

    #[tokio::test]
    async fn test_add_export_real_dir() {
        let em = ExportManagerHandle::new();
        // /tmp exists on all platforms
        let result = em
            .add_export("tmpdir".to_string(), PathBuf::from("/tmp"), false)
            .await;
        assert!(result.is_ok());
        let info = result.unwrap();
        assert_eq!(info.name, "tmpdir");
        assert_eq!(info.export_id, 1);
        assert!(!info.read_only);
    }

    #[tokio::test]
    async fn test_add_export_nonexistent_path() {
        let em = ExportManagerHandle::new();
        let result = em
            .add_export(
                "bad".to_string(),
                PathBuf::from("/nonexistent_path_abc123"),
                false,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_add_duplicate_export() {
        let em = ExportManagerHandle::new();
        let r1 = em
            .add_export("dup".to_string(), PathBuf::from("/tmp"), false)
            .await;
        assert!(r1.is_ok());
        let r2 = em
            .add_export("dup".to_string(), PathBuf::from("/tmp"), false)
            .await;
        assert!(r2.is_err());
        assert!(r2.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn test_remove_export() {
        let em = ExportManagerHandle::new();
        em.add_export("removeme".to_string(), PathBuf::from("/tmp"), false)
            .await
            .unwrap();
        let result = em.remove_export("removeme").await;
        assert!(result.is_ok());
        // Should be gone
        let exports = em.list_exports().await;
        assert!(exports.is_empty());
    }

    #[tokio::test]
    async fn test_remove_nonexistent_export() {
        let em = ExportManagerHandle::new();
        let result = em.remove_export("nosuch").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_get_export_by_id() {
        let em = ExportManagerHandle::new();
        let info = em
            .add_export("byid".to_string(), PathBuf::from("/tmp"), true)
            .await
            .unwrap();
        let result = em.get_export_by_id(info.export_id).await;
        assert!(result.is_some());
        let (found_info, _fm) = result.unwrap();
        assert_eq!(found_info.name, "byid");
        assert!(found_info.read_only);
    }

    #[tokio::test]
    async fn test_get_export_by_id_not_found() {
        let em = ExportManagerHandle::new();
        let result = em.get_export_by_id(99).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_export_by_name() {
        let em = ExportManagerHandle::new();
        em.add_export("named".to_string(), PathBuf::from("/tmp"), false)
            .await
            .unwrap();
        let result = em.get_export_by_name("named").await;
        assert!(result.is_some());
        let (info, _fm) = result.unwrap();
        assert_eq!(info.name, "named");
    }

    #[tokio::test]
    async fn test_get_export_by_name_not_found() {
        let em = ExportManagerHandle::new();
        let result = em.get_export_by_name("nosuch").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_multiple_exports_sequential_ids() {
        let em = ExportManagerHandle::new();
        let r1 = em
            .add_export("e1".to_string(), PathBuf::from("/tmp"), false)
            .await
            .unwrap();
        let r2 = em
            .add_export("e2".to_string(), PathBuf::from("/tmp"), false)
            .await
            .unwrap();
        assert_eq!(r1.export_id, 1);
        assert_eq!(r2.export_id, 2);
        let exports = em.list_exports().await;
        assert_eq!(exports.len(), 2);
    }

    #[tokio::test]
    async fn test_export_stats_initial_zero() {
        let em = ExportManagerHandle::new();
        let info = em
            .add_export("stats".to_string(), PathBuf::from("/tmp"), false)
            .await
            .unwrap();
        let snap = info.stats.snapshot();
        assert_eq!(snap.reads, 0);
        assert_eq!(snap.writes, 0);
        assert_eq!(snap.bytes_read, 0);
        assert_eq!(snap.bytes_written, 0);
        assert_eq!(snap.ops, 0);
    }

    // ── Rate limiter unit tests ─────────────────────────────────────

    #[test]
    fn test_rate_limiter_unlimited() {
        let mut rl = RateLimiter::new(QosConfig::default());
        // Unlimited — always allowed
        for _ in 0..1000 {
            assert!(rl.try_consume_op());
            assert!(rl.try_consume_bytes(1_000_000));
        }
    }

    #[test]
    fn test_rate_limiter_ops_limit() {
        let mut rl = RateLimiter::new(QosConfig {
            max_ops_per_sec: 10,
            max_bytes_per_sec: 0,
        });
        // Should allow 10 ops initially (bucket starts full)
        for _ in 0..10 {
            assert!(rl.try_consume_op());
        }
        // 11th should be denied (no time to refill)
        assert!(!rl.try_consume_op());
    }

    #[test]
    fn test_rate_limiter_bytes_limit() {
        let mut rl = RateLimiter::new(QosConfig {
            max_ops_per_sec: 0,
            max_bytes_per_sec: 1000,
        });
        // Should allow 1000 bytes initially
        assert!(rl.try_consume_bytes(500));
        assert!(rl.try_consume_bytes(500));
        // 1001th byte should be denied
        assert!(!rl.try_consume_bytes(1));
    }

    #[test]
    fn test_rate_limiter_config_update() {
        let mut rl = RateLimiter::new(QosConfig::default());
        assert_eq!(rl.config().max_ops_per_sec, 0);
        rl.update_config(QosConfig {
            max_ops_per_sec: 100,
            max_bytes_per_sec: 50000,
        });
        assert_eq!(rl.config().max_ops_per_sec, 100);
        assert_eq!(rl.config().max_bytes_per_sec, 50000);
    }

    #[test]
    fn test_qos_config_default() {
        let config = QosConfig::default();
        assert_eq!(config.max_ops_per_sec, 0);
        assert_eq!(config.max_bytes_per_sec, 0);
    }

    #[tokio::test]
    async fn test_set_qos_nonexistent_export() {
        let em = ExportManagerHandle::new();
        let result = em
            .set_qos(
                "nosuch",
                QosConfig {
                    max_ops_per_sec: 100,
                    max_bytes_per_sec: 0,
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_qos_nonexistent_export() {
        let em = ExportManagerHandle::new();
        let result = em.get_qos("nosuch").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_set_and_get_qos() {
        let em = ExportManagerHandle::new();
        em.add_export("qos_test".to_string(), PathBuf::from("/tmp"), false)
            .await
            .unwrap();
        let result = em
            .set_qos(
                "qos_test",
                QosConfig {
                    max_ops_per_sec: 500,
                    max_bytes_per_sec: 10_000_000,
                },
            )
            .await;
        assert!(result.is_ok());
        // Give the spawn a moment to update
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let qos = em.get_qos("qos_test").await;
        assert!(qos.is_some());
        let qos = qos.unwrap();
        assert_eq!(qos.max_ops_per_sec, 500);
        assert_eq!(qos.max_bytes_per_sec, 10_000_000);
    }
}
