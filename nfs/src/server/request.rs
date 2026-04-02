use std::{collections::HashMap, sync::Arc, time::SystemTime};

use nextnfs_proto::nfs4_proto::{NfsFh4, NfsStat4};
use tracing::error;

use tokio::sync::Mutex;

use super::{
    clientmanager::ClientManagerHandle,
    export_manager::{AccessControl, ExportManagerHandle, ExportStats, QuotaManager, RateLimiter},
    filemanager::{FileManagerHandle, Filehandle},
    nfs40::op_pseudo,
    nfs41::SessionManager,
};

#[derive(Debug)]
pub struct NfsRequest<'a> {
    client_addr: String,
    filehandle: Option<Filehandle>,
    // saved filehandle for SAVEFH/RESTOREFH
    saved_filehandle: Option<Filehandle>,
    // shared state for client manager between connections
    cmanager: ClientManagerHandle,
    // export manager — routes to per-export FileManagerHandles
    export_manager: ExportManagerHandle,
    // cached file manager for the current export (set by set_export)
    cached_fmanager: Option<FileManagerHandle>,
    // cached export stats for zero-cost counter updates
    export_stats: Option<Arc<ExportStats>>,
    // cached per-export rate limiter for QoS enforcement
    rate_limiter: Option<Arc<Mutex<RateLimiter>>>,
    // cached per-export quota manager
    quota_manager: Option<Arc<QuotaManager>>,
    // cached per-export access control
    access_control: Option<Arc<AccessControl>>,
    // current export id (extracted from filehandle)
    current_export_id: Option<u8>,
    // time the server was booted
    pub boot_time: u64,
    // time the request was received
    pub request_time: u64,
    // locally cached filehandles for this client
    pub filehandle_cache: Option<&'a mut HashMap<NfsFh4, (SystemTime, Filehandle)>>,
    cache_ttl: u64,
    // NFSv4.1 session manager (None for v4.0-only)
    session_manager: Option<SessionManager>,
    // Session context set by SEQUENCE handler
    pub session_id: Option<[u8; 16]>,
    pub sequence_slotid: Option<u32>,
}

impl<'a> NfsRequest<'a> {
    pub fn new(
        client_addr: String,
        cmanager: ClientManagerHandle,
        export_manager: ExportManagerHandle,
        default_fmanager: Option<FileManagerHandle>,
        boot_time: u64,
        filehandle_cache: Option<&'a mut HashMap<NfsFh4, (SystemTime, Filehandle)>>,
        session_manager: Option<SessionManager>,
    ) -> Self {
        let request_time = std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_secs();

        NfsRequest {
            client_addr,
            filehandle: None,
            saved_filehandle: None,
            cmanager,
            export_manager,
            cached_fmanager: default_fmanager,
            export_stats: None,
            rate_limiter: None,
            quota_manager: None,
            access_control: None,
            current_export_id: None,
            boot_time,
            request_time,
            filehandle_cache,
            cache_ttl: 10,
            session_manager,
            session_id: None,
            sequence_slotid: None,
        }
    }

    /// Get the session manager (for v4.1+ operations).
    pub fn session_manager(&self) -> Option<&SessionManager> {
        self.session_manager.as_ref()
    }

    pub fn client_addr(&self) -> &String {
        &self.client_addr
    }

    pub fn current_filehandle_id(&self) -> Option<NfsFh4> {
        self.filehandle.as_ref().map(|fh| fh.id)
    }

    pub fn current_filehandle(&self) -> Option<&Filehandle> {
        self.filehandle.as_ref()
    }

    pub fn client_manager(&self) -> ClientManagerHandle {
        self.cmanager.clone()
    }

    pub fn export_manager(&self) -> ExportManagerHandle {
        self.export_manager.clone()
    }

    /// Get the current export's FileManagerHandle (synchronous).
    /// Panics if no export has been selected yet.
    pub fn file_manager(&self) -> FileManagerHandle {
        self.cached_fmanager
            .clone()
            .expect("file_manager() called before export was selected")
    }

    /// Switch to a different export by id. Updates the cached file manager.
    /// Called by PUTROOTFH, PUTFH, LOOKUP when routing to an export.
    pub async fn set_export(&mut self, export_id: u8) {
        self.current_export_id = Some(export_id);
        if export_id == op_pseudo::PSEUDO_ROOT_EXPORT_ID {
            // Pseudo-root doesn't have a real file manager
            self.cached_fmanager = None;
            self.export_stats = None;
            self.rate_limiter = None;
            self.quota_manager = None;
            self.access_control = None;
            return;
        }
        if let Some((info, fm)) = self.export_manager.get_export_by_id(export_id).await {
            self.cached_fmanager = Some(fm);
            self.export_stats = Some(info.stats);
            self.rate_limiter = Some(info.rate_limiter);
            self.quota_manager = Some(info.quota_manager);
            self.access_control = Some(info.access_control);
        }
    }

    /// Check if the current filehandle is the pseudo-root.
    pub fn is_pseudo_root(&self) -> bool {
        self.current_export_id == Some(op_pseudo::PSEUDO_ROOT_EXPORT_ID)
    }

    pub fn current_export_id(&self) -> Option<u8> {
        self.current_export_id
    }

    /// Get cached export stats for zero-cost counter updates.
    pub fn export_stats(&self) -> Option<&Arc<ExportStats>> {
        self.export_stats.as_ref()
    }

    /// Get the cached per-export rate limiter for QoS enforcement.
    pub fn rate_limiter(&self) -> Option<&Arc<Mutex<RateLimiter>>> {
        self.rate_limiter.as_ref()
    }

    /// Get the cached per-export quota manager.
    pub fn quota_manager(&self) -> Option<&Arc<QuotaManager>> {
        self.quota_manager.as_ref()
    }

    /// Get the cached per-export access control.
    pub fn access_control(&self) -> Option<&Arc<AccessControl>> {
        self.access_control.as_ref()
    }

    /// Check if the current client is allowed to access the current export.
    /// Returns true if allowed (no ACL or client matches).
    pub fn check_client_access(&self) -> bool {
        match self.access_control.as_ref() {
            Some(ac) => ac.check_client(&self.client_addr),
            None => true,
        }
    }

    pub fn set_filehandle(&mut self, filehandle: Filehandle) {
        self.filehandle = Some(filehandle);
    }

    /// Set filehandle and extract/track export_id (no export switch — caller must
    /// call set_export() separately if needed).
    pub fn set_filehandle_with_export(&mut self, filehandle: Filehandle) {
        let export_id = op_pseudo::export_id_from_fh(&filehandle.id);
        self.current_export_id = Some(export_id);
        self.filehandle = Some(filehandle);
    }

    pub fn cache_filehandle(&mut self, filehandle: Filehandle) {
        if let Some(cache) = self.filehandle_cache.as_mut() {
            let now: SystemTime = SystemTime::now();
            cache.insert(filehandle.id, (now, filehandle));
        }
    }

    pub fn drop_filehandle_from_cache(&mut self, filehandle_id: NfsFh4) {
        if let Some(cache) = self.filehandle_cache.as_mut() {
            cache.remove(&filehandle_id);
        }
    }

    pub fn get_filehandle_from_cache(&mut self, filehandle_id: NfsFh4) -> Option<Filehandle> {
        let cache = self.filehandle_cache.as_ref();
        match cache {
            None => None,
            Some(cache) => {
                match cache.get(&filehandle_id) {
                    Some(fh) => {
                        let now: SystemTime = SystemTime::now();
                        let (time, filehandle) = fh;
                        if now.duration_since(*time).unwrap_or_default().as_secs()
                            > self.cache_ttl
                        {
                            self.drop_filehandle_from_cache(filehandle.id);
                            None
                        } else {
                            Some(filehandle.clone())
                        }
                    }
                    None => None,
                }
            }
        }
    }

    pub async fn set_filehandle_id(
        &mut self,
        filehandle_id: NfsFh4,
    ) -> Result<Filehandle, NfsStat4> {
        // Extract export_id and switch if needed
        let export_id = op_pseudo::export_id_from_fh(&filehandle_id);
        if self.current_export_id != Some(export_id) {
            self.set_export(export_id).await;
        }

        if export_id == op_pseudo::PSEUDO_ROOT_EXPORT_ID {
            return Err(NfsStat4::Nfs4errStale);
        }

        let fm = self.file_manager();
        let res = fm.get_filehandle_for_id(filehandle_id).await;
        match res {
            Ok(ref fh) => {
                self.filehandle = Some(fh.clone());
                Ok(fh.clone())
            }
            Err(e) => {
                error!("couldn't set filehandle: {:?}", e);
                Err(NfsStat4::Nfs4errStale)
            }
        }
    }

    pub fn unset_filehandle(&mut self) {
        self.filehandle = None;
    }

    /// Save current filehandle (SAVEFH).
    pub fn save_filehandle(&mut self) {
        self.saved_filehandle = self.filehandle.clone();
    }

    /// Restore saved filehandle (RESTOREFH).
    pub fn restore_filehandle(&mut self) -> bool {
        if let Some(ref saved) = self.saved_filehandle {
            self.filehandle = Some(saved.clone());
            true
        } else {
            false
        }
    }

    /// Get the saved filehandle.
    pub fn saved_filehandle(&self) -> Option<&Filehandle> {
        self.saved_filehandle.as_ref()
    }

    /// Set the saved filehandle directly (used by COPY tests).
    pub fn set_saved_filehandle(&mut self, filehandle: Filehandle) {
        self.saved_filehandle = Some(filehandle);
    }

    pub async fn close(&self) {}

    /// Set quota manager directly (for testing).
    #[cfg(test)]
    pub fn set_quota_manager(&mut self, qm: Arc<QuotaManager>) {
        self.quota_manager = Some(qm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_request_no_filehandle_initially() {
        let request = create_nfs40_server(None).await;
        assert!(request.current_filehandle().is_none());
        assert!(request.current_filehandle_id().is_none());
    }

    #[tokio::test]
    async fn test_request_no_saved_filehandle_initially() {
        let request = create_nfs40_server(None).await;
        assert!(request.saved_filehandle().is_none());
    }

    #[tokio::test]
    async fn test_request_save_restore_filehandle() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let fh_id = request.current_filehandle_id().unwrap();

        // Save
        request.save_filehandle();
        assert!(request.saved_filehandle().is_some());

        // Clear current
        request.unset_filehandle();
        assert!(request.current_filehandle().is_none());

        // Restore
        assert!(request.restore_filehandle());
        assert_eq!(request.current_filehandle_id().unwrap(), fh_id);
    }

    #[tokio::test]
    async fn test_request_restore_without_save() {
        let mut request = create_nfs40_server(None).await;
        assert!(!request.restore_filehandle());
    }

    #[tokio::test]
    async fn test_request_unset_filehandle() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        assert!(request.current_filehandle().is_some());
        request.unset_filehandle();
        assert!(request.current_filehandle().is_none());
    }

    #[tokio::test]
    async fn test_request_client_addr() {
        let request = create_nfs40_server(None).await;
        assert_eq!(request.client_addr(), "127.0.0.1:12345");
    }

    #[tokio::test]
    async fn test_request_no_export_id_initially() {
        let request = create_nfs40_server(None).await;
        assert!(request.current_export_id().is_none());
    }

    #[tokio::test]
    async fn test_request_is_not_pseudo_root_initially() {
        let request = create_nfs40_server(None).await;
        assert!(!request.is_pseudo_root());
    }

    #[tokio::test]
    async fn test_set_filehandle_id_bad_id() {
        let mut request = create_nfs40_server(None).await;
        let bad_id: NfsFh4 = [0xCC; 26];
        let result = request.set_filehandle_id(bad_id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_request_boot_time_set() {
        let request = create_nfs40_server(None).await;
        assert!(request.boot_time > 0);
    }

    #[tokio::test]
    async fn test_request_time_set() {
        let request = create_nfs40_server(None).await;
        assert!(request.request_time > 0);
    }

    #[tokio::test]
    async fn test_request_close_no_panic() {
        let request = create_nfs40_server(None).await;
        request.close().await;
    }

    #[tokio::test]
    async fn test_set_filehandle_with_export() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let fh = request.current_filehandle().unwrap().clone();
        request.unset_filehandle();
        request.set_filehandle_with_export(fh.clone());
        assert!(request.current_filehandle().is_some());
        assert!(request.current_export_id().is_some());
    }
}
