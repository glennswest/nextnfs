//! NFSv4.1 session layer (RFC 5661).
//!
//! Provides session management for NFSv4.1 COMPOUND requests. Key operations:
//!
//! - EXCHANGE_ID — register client, get client ID + server owner
//! - CREATE_SESSION — create session with fore/back channel attrs
//! - SEQUENCE — must be first op in every v4.1 COMPOUND; slot/sequence validation
//! - BIND_CONN_TO_SESSION — associate additional connections (trunking)
//! - DESTROY_SESSION / DESTROY_CLIENTID — cleanup
//! - RECLAIM_COMPLETE — signal end of grace/reclaim period
//!
//! ## Session Trunking (RFC 5661 §2.10.5)
//!
//! Multiple TCP connections can be bound to a single session via
//! BIND_CONN_TO_SESSION. This enables bandwidth aggregation (session
//! trunking) and failover (client-ID trunking). The server tracks bound
//! connections per session for auditing and cleanup.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Unique session identifier (16 bytes per RFC 5661 §2.10.3.1).
pub type SessionId = [u8; 16];

/// Per-session state.
#[derive(Debug, Clone)]
pub struct Session {
    /// Session identifier.
    pub id: SessionId,
    /// Client ID that owns this session.
    pub client_id: u64,
    /// Sequence slots for exactly-once semantics (slot_id → highest_seq).
    pub slots: Vec<SlotState>,
    /// Fore-channel attributes negotiated at CREATE_SESSION.
    pub fore_channel_attrs: ChannelAttrs,
    /// Bound connections for session trunking (RFC 5661 §2.10.5).
    /// Tracks client addresses bound via BIND_CONN_TO_SESSION.
    pub bound_connections: HashSet<String>,
}

/// State of a single sequence slot.
#[derive(Debug, Clone, Default)]
pub struct SlotState {
    /// Highest sequence ID seen on this slot.
    pub sequence_id: u32,
    /// Cached reply for replay detection.
    pub cached_reply: Option<Vec<u8>>,
}

/// Channel attributes negotiated during CREATE_SESSION.
#[derive(Debug, Clone)]
pub struct ChannelAttrs {
    pub max_request_size: u32,
    pub max_response_size: u32,
    pub max_ops: u32,
    pub max_requests: u32,
}

impl Default for ChannelAttrs {
    fn default() -> Self {
        Self {
            max_request_size: 1048576,  // 1MB
            max_response_size: 1048576, // 1MB
            max_ops: 64,
            max_requests: 64,
        }
    }
}

/// Session manager — tracks all active sessions.
///
/// Thread-safe via `Arc<RwLock<>>`.  In a full implementation this would
/// be instantiated by an NFS41Server and consulted on every SEQUENCE op.
#[derive(Debug, Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<SessionId, Session>>>,
    next_client_id: Arc<RwLock<u64>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            next_client_id: Arc::new(RwLock::new(1)),
        }
    }

    /// Allocate a new client ID (EXCHANGE_ID).
    pub async fn allocate_client_id(&self) -> u64 {
        let mut id = self.next_client_id.write().await;
        let client_id = *id;
        *id += 1;
        client_id
    }

    /// Create a new session (CREATE_SESSION).
    pub async fn create_session(
        &self,
        client_id: u64,
        max_slots: u32,
    ) -> Session {
        let mut rng_bytes = [0u8; 16];
        // Simple session ID from client_id + counter
        rng_bytes[..8].copy_from_slice(&client_id.to_be_bytes());
        let counter = {
            let sessions = self.sessions.read().await;
            sessions.len() as u64
        };
        rng_bytes[8..16].copy_from_slice(&counter.to_be_bytes());

        let session = Session {
            id: rng_bytes,
            client_id,
            slots: (0..max_slots)
                .map(|_| SlotState::default())
                .collect(),
            fore_channel_attrs: ChannelAttrs::default(),
            bound_connections: HashSet::new(),
        };

        let mut sessions = self.sessions.write().await;
        sessions.insert(session.id, session.clone());
        session
    }

    /// Look up a session by ID (SEQUENCE).
    pub async fn get_session(&self, id: &SessionId) -> Option<Session> {
        let sessions = self.sessions.read().await;
        sessions.get(id).cloned()
    }

    /// Destroy a session (DESTROY_SESSION).
    pub async fn destroy_session(&self, id: &SessionId) -> bool {
        let mut sessions = self.sessions.write().await;
        sessions.remove(id).is_some()
    }

    /// Bind a connection to a session (BIND_CONN_TO_SESSION for trunking).
    pub async fn bind_connection(&self, id: &SessionId, addr: String) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            session.bound_connections.insert(addr);
            true
        } else {
            false
        }
    }

    /// Get the number of bound connections for a session.
    pub async fn connection_count(&self, id: &SessionId) -> usize {
        let sessions = self.sessions.read().await;
        sessions
            .get(id)
            .map(|s| s.bound_connections.len())
            .unwrap_or(0)
    }

    /// Destroy all sessions for a client (DESTROY_CLIENTID).
    pub async fn destroy_client(&self, client_id: u64) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, s| s.client_id != client_id);
        before - sessions.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_lifecycle() {
        let mgr = SessionManager::new();

        // EXCHANGE_ID
        let cid = mgr.allocate_client_id().await;
        assert_eq!(cid, 1);

        // CREATE_SESSION
        let session = mgr.create_session(cid, 4).await;
        assert_eq!(session.client_id, cid);
        assert_eq!(session.slots.len(), 4);

        // SEQUENCE (lookup)
        let found = mgr.get_session(&session.id).await;
        assert!(found.is_some());

        // DESTROY_SESSION
        assert!(mgr.destroy_session(&session.id).await);
        assert!(mgr.get_session(&session.id).await.is_none());
    }

    #[tokio::test]
    async fn test_destroy_client() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        mgr.create_session(cid, 2).await;
        mgr.create_session(cid, 2).await;

        let removed = mgr.destroy_client(cid).await;
        assert_eq!(removed, 2);
    }

    #[tokio::test]
    async fn test_allocate_client_id_increments() {
        let mgr = SessionManager::new();
        let id1 = mgr.allocate_client_id().await;
        let id2 = mgr.allocate_client_id().await;
        let id3 = mgr.allocate_client_id().await;
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[tokio::test]
    async fn test_create_session_unique_ids() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let s1 = mgr.create_session(cid, 4).await;
        let s2 = mgr.create_session(cid, 4).await;
        assert_ne!(s1.id, s2.id);
    }

    #[tokio::test]
    async fn test_get_session_nonexistent() {
        let mgr = SessionManager::new();
        assert!(mgr.get_session(&[0u8; 16]).await.is_none());
    }

    #[tokio::test]
    async fn test_destroy_session_nonexistent() {
        let mgr = SessionManager::new();
        assert!(!mgr.destroy_session(&[0u8; 16]).await);
    }

    #[tokio::test]
    async fn test_destroy_client_no_sessions() {
        let mgr = SessionManager::new();
        assert_eq!(mgr.destroy_client(999).await, 0);
    }

    #[test]
    fn test_channel_attrs_default() {
        let attrs = ChannelAttrs::default();
        assert_eq!(attrs.max_request_size, 1048576);
        assert_eq!(attrs.max_response_size, 1048576);
        assert_eq!(attrs.max_ops, 64);
        assert_eq!(attrs.max_requests, 64);
    }

    #[test]
    fn test_slot_state_default() {
        let slot = SlotState::default();
        assert_eq!(slot.sequence_id, 0);
        assert!(slot.cached_reply.is_none());
    }

    #[tokio::test]
    async fn test_session_manager_default() {
        let mgr = SessionManager::default();
        let cid = mgr.allocate_client_id().await;
        assert_eq!(cid, 1);
    }

    #[tokio::test]
    async fn test_create_session_slot_count() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let session = mgr.create_session(cid, 8).await;
        assert_eq!(session.slots.len(), 8);
        for slot in &session.slots {
            assert_eq!(slot.sequence_id, 0);
            assert!(slot.cached_reply.is_none());
        }
    }

    #[tokio::test]
    async fn test_bind_connection_trunking() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let session = mgr.create_session(cid, 4).await;

        // Bind multiple connections to the same session
        assert!(mgr.bind_connection(&session.id, "192.168.1.10:12345".to_string()).await);
        assert!(mgr.bind_connection(&session.id, "192.168.1.10:54321".to_string()).await);
        assert!(mgr.bind_connection(&session.id, "192.168.1.20:12345".to_string()).await);

        assert_eq!(mgr.connection_count(&session.id).await, 3);

        // Binding same address again is idempotent (HashSet)
        assert!(mgr.bind_connection(&session.id, "192.168.1.10:12345".to_string()).await);
        assert_eq!(mgr.connection_count(&session.id).await, 3);
    }

    #[tokio::test]
    async fn test_bind_connection_nonexistent_session() {
        let mgr = SessionManager::new();
        assert!(!mgr.bind_connection(&[0xFF; 16], "10.0.0.1:80".to_string()).await);
        assert_eq!(mgr.connection_count(&[0xFF; 16]).await, 0);
    }

    #[tokio::test]
    async fn test_destroy_client_preserves_other_sessions() {
        let mgr = SessionManager::new();
        let cid1 = mgr.allocate_client_id().await;
        let cid2 = mgr.allocate_client_id().await;
        let _s1 = mgr.create_session(cid1, 2).await;
        let s2 = mgr.create_session(cid2, 2).await;

        mgr.destroy_client(cid1).await;
        assert!(mgr.get_session(&s2.id).await.is_some());
    }
}
