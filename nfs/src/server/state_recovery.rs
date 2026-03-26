//! Near-zero grace period: persist client and lock state to disk for crash recovery.
//!
//! On clean shutdown or periodically, the server serializes a snapshot of all
//! confirmed clients and active locks to a JSON file. On restart, if the state
//! file is found and valid, the server can skip the grace period entirely.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use nextnfs_proto::nfs4_proto::NfsLockType4;

/// Serializable snapshot of a client entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSnapshot {
    pub principal: Option<String>,
    pub verifier: [u8; 8],
    pub id: String,
    pub clientid: u64,
    pub callback_program: u32,
    pub callback_rnetid: String,
    pub callback_raddr: String,
    pub callback_ident: u32,
    pub confirmed: bool,
}

/// Serializable snapshot of a lock state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockSnapshot {
    pub stateid: [u8; 12],
    pub seqid: u32,
    pub client_id: u64,
    pub owner: Vec<u8>,
    pub lock_type: String, // "Open" or "ByteRange"
    pub filehandle_id: [u8; 26],
    pub start: Option<u64>,
    pub length: Option<u64>,
    pub share_access: Option<u32>,
    pub share_deny: Option<u32>,
    pub nfs_lock_type: Option<String>, // "ReadLt", "WriteLt", etc.
}

/// Full server state snapshot for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Version tag for forward compatibility
    pub version: u32,
    /// Boot time of the server that wrote this state
    pub boot_time: u64,
    /// Timestamp when this snapshot was written (unix epoch seconds)
    pub written_at: u64,
    /// Confirmed client entries
    pub clients: Vec<ClientSnapshot>,
    /// Active lock states (per-export, keyed by export_id)
    pub locks: Vec<LockSnapshot>,
}

impl StateSnapshot {
    pub fn new(boot_time: u64) -> Self {
        let written_at = std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_secs();
        Self {
            version: 1,
            boot_time,
            written_at,
            clients: Vec::new(),
            locks: Vec::new(),
        }
    }
}

/// State file manager — handles reading/writing state snapshots.
#[derive(Debug, Clone)]
pub struct StateRecovery {
    state_path: PathBuf,
}

impl StateRecovery {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_path: state_dir.join("nextnfs-state.json"),
        }
    }

    /// Save a state snapshot to disk.
    pub fn save(&self, snapshot: &StateSnapshot) -> Result<(), String> {
        let json = serde_json::to_string_pretty(snapshot)
            .map_err(|e| format!("failed to serialize state: {}", e))?;

        // Write to temp file, then rename for atomicity
        let tmp_path = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json)
            .map_err(|e| format!("failed to write state file: {}", e))?;
        std::fs::rename(&tmp_path, &self.state_path)
            .map_err(|e| format!("failed to rename state file: {}", e))?;

        debug!(path = %self.state_path.display(), clients = snapshot.clients.len(),
               locks = snapshot.locks.len(), "state snapshot saved");
        Ok(())
    }

    /// Load a state snapshot from disk.
    pub fn load(&self) -> Result<StateSnapshot, String> {
        if !self.state_path.exists() {
            return Err("no state file found".to_string());
        }

        let json = std::fs::read_to_string(&self.state_path)
            .map_err(|e| format!("failed to read state file: {}", e))?;
        let snapshot: StateSnapshot = serde_json::from_str(&json)
            .map_err(|e| format!("failed to parse state file: {}", e))?;

        // Validate version
        if snapshot.version != 1 {
            return Err(format!("unsupported state version: {}", snapshot.version));
        }

        // Check staleness — state older than 5 minutes is considered stale
        let now = std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_secs();
        let age = now.saturating_sub(snapshot.written_at);
        if age > 300 {
            warn!(
                age_secs = age,
                "state file is stale (>5 minutes old), ignoring"
            );
            return Err(format!("state file is stale: {} seconds old", age));
        }

        info!(
            path = %self.state_path.display(),
            clients = snapshot.clients.len(),
            locks = snapshot.locks.len(),
            age_secs = age,
            "state snapshot loaded"
        );
        Ok(snapshot)
    }

    /// Delete the state file (after successful recovery or on clean shutdown).
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.state_path);
        debug!(path = %self.state_path.display(), "state file cleared");
    }

    /// Check if a state file exists.
    pub fn exists(&self) -> bool {
        self.state_path.exists()
    }

    /// Get the path to the state file.
    pub fn path(&self) -> &Path {
        &self.state_path
    }
}

/// Convert NfsLockType4 to string for serialization.
pub fn lock_type_to_string(lt: &NfsLockType4) -> String {
    match lt {
        NfsLockType4::ReadLt => "ReadLt".to_string(),
        NfsLockType4::WriteLt => "WriteLt".to_string(),
        NfsLockType4::ReadwLt => "ReadwLt".to_string(),
        NfsLockType4::WritewLt => "WritewLt".to_string(),
    }
}

/// Convert string back to NfsLockType4.
pub fn string_to_lock_type(s: &str) -> Option<NfsLockType4> {
    match s {
        "ReadLt" => Some(NfsLockType4::ReadLt),
        "WriteLt" => Some(NfsLockType4::WriteLt),
        "ReadwLt" => Some(NfsLockType4::ReadwLt),
        "WritewLt" => Some(NfsLockType4::WritewLt),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_snapshot() -> StateSnapshot {
        let mut snap = StateSnapshot::new(1234567890);
        snap.clients.push(ClientSnapshot {
            principal: Some("Linux".to_string()),
            verifier: [1; 8],
            id: "client1".to_string(),
            clientid: 42,
            callback_program: 0x40000001,
            callback_rnetid: "tcp".to_string(),
            callback_raddr: "192.168.1.10".to_string(),
            callback_ident: 1,
            confirmed: true,
        });
        snap.locks.push(LockSnapshot {
            stateid: [2; 12],
            seqid: 1,
            client_id: 42,
            owner: b"owner1".to_vec(),
            lock_type: "Open".to_string(),
            filehandle_id: [3; 26],
            start: None,
            length: None,
            share_access: Some(1),
            share_deny: Some(0),
            nfs_lock_type: None,
        });
        snap
    }

    #[test]
    fn test_save_and_load() {
        let dir = TempDir::new().unwrap();
        let sr = StateRecovery::new(dir.path());

        let snap = make_snapshot();
        sr.save(&snap).unwrap();
        assert!(sr.exists());

        let loaded = sr.load().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.boot_time, 1234567890);
        assert_eq!(loaded.clients.len(), 1);
        assert_eq!(loaded.clients[0].clientid, 42);
        assert_eq!(loaded.locks.len(), 1);
        assert_eq!(loaded.locks[0].client_id, 42);
    }

    #[test]
    fn test_load_no_file() {
        let dir = TempDir::new().unwrap();
        let sr = StateRecovery::new(dir.path());
        assert!(!sr.exists());
        let result = sr.load();
        assert!(result.is_err());
    }

    #[test]
    fn test_clear() {
        let dir = TempDir::new().unwrap();
        let sr = StateRecovery::new(dir.path());
        sr.save(&make_snapshot()).unwrap();
        assert!(sr.exists());
        sr.clear();
        assert!(!sr.exists());
    }

    #[test]
    fn test_lock_type_roundtrip() {
        for lt in [
            NfsLockType4::ReadLt,
            NfsLockType4::WriteLt,
            NfsLockType4::ReadwLt,
            NfsLockType4::WritewLt,
        ] {
            let s = lock_type_to_string(&lt);
            let decoded = string_to_lock_type(&s).unwrap();
            assert_eq!(decoded, lt);
        }
    }

    #[test]
    fn test_string_to_lock_type_invalid() {
        assert!(string_to_lock_type("InvalidType").is_none());
    }

    #[test]
    fn test_snapshot_serialization() {
        let snap = make_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: StateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.clients.len(), 1);
        assert_eq!(decoded.locks.len(), 1);
        assert_eq!(decoded.version, 1);
    }

    #[test]
    fn test_atomic_write() {
        let dir = TempDir::new().unwrap();
        let sr = StateRecovery::new(dir.path());
        let snap = make_snapshot();
        // Save twice — should not leave temp files
        sr.save(&snap).unwrap();
        sr.save(&snap).unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        // Should only have the final state file
        assert_eq!(entries.len(), 1);
    }
}
