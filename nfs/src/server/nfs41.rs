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

use nextnfs_proto::nfs4_proto::Compound4res;

/// Unique session identifier (16 bytes per RFC 5661 §2.10.3.1).
pub type SessionId = [u8; 16];

/// Per-session state.
#[derive(Debug, Clone)]
pub struct Session {
    /// Session identifier.
    pub id: SessionId,
    /// Client ID that owns this session.
    pub client_id: u64,
    /// Sequence slots for exactly-once semantics (RFC 8881 §2.10.6).
    pub slots: Vec<SlotState>,
    /// Fore-channel attributes negotiated at CREATE_SESSION.
    pub fore_channel_attrs: ChannelAttrs,
    /// Bound connections for session trunking (RFC 5661 §2.10.5).
    /// Tracks client addresses bound via BIND_CONN_TO_SESSION.
    pub bound_connections: HashSet<String>,
}

/// State of a single sequence slot (RFC 8881 §2.10.6.1).
#[derive(Debug, Clone, Default)]
pub struct SlotState {
    /// Sequence ID of the last request accepted on this slot (0 before the
    /// first; the client's first request on a slot uses 1).
    pub sequence_id: u32,
    /// A request has been accepted on this slot (so `sequence_id` names a
    /// real request a retry can match).
    pub used: bool,
    /// A request with `sequence_id` is still executing.
    pub in_use: bool,
    /// The reply to the last request, when it was cached (`sa_cachethis`,
    /// or a SEQUENCE-only COMPOUND) and fit `ca_maxresponsesize_cached`.
    pub cached_reply: Option<Arc<Compound4res>>,
}

/// What SEQUENCE does with a request on a slot ([`SessionManager::begin_slot`]).
#[derive(Debug)]
pub enum SlotCheck {
    /// New request (seqid == last + 1): the slot is now in use; execute the
    /// COMPOUND and hand its reply to [`SlotClaim::finish`].
    New(SlotClaim),
    /// Retry of the last request, whose reply was cached: send it again.
    Replay(Arc<Compound4res>),
    /// Retry of the last request, whose reply was not cached.
    ReplayUncached,
    /// The slot is still executing the request with this seqid
    /// (NFS4ERR_DELAY).
    InUse,
    /// Any other seqid (NFS4ERR_SEQ_MISORDERED).
    Misordered,
    /// The session no longer exists (NFS4ERR_BADSESSION).
    BadSession,
    /// The slot id is outside the session's slot table (NFS4ERR_BADSLOT).
    BadSlot,
}

/// A SEQUENCE that is a retry of the slot's last request: the COMPOUND
/// is answered from here instead of being executed again.
#[derive(Debug)]
pub enum SequenceReplay {
    /// The cached reply, sent as it was.
    Cached(Arc<Compound4res>),
    /// No reply was cached: SEQUENCE succeeds and the next op gets
    /// NFS4ERR_RETRY_UNCACHED_REP.
    Uncached,
}

/// A slot claimed by a new request. Hand the COMPOUND reply to
/// [`SlotClaim::finish`]; a claim dropped without it (the request was
/// cancelled) frees the slot with nothing cached, so a retry gets
/// NFS4ERR_RETRY_UNCACHED_REP instead of a slot stuck in use.
#[derive(Debug)]
pub struct SlotClaim {
    manager: SessionManager,
    session_id: SessionId,
    slot_id: u32,
    sequence_id: u32,
    cache_this: bool,
    max_cached: u32,
    done: bool,
}

impl SlotClaim {
    /// Free the slot and keep `reply` for a retry when the client asked for
    /// it (`sa_cachethis`, or `solo_sequence` — a COMPOUND of SEQUENCE alone,
    /// which costs nothing to keep) and its XDR encoding fits the session's
    /// `ca_maxresponsesize_cached`. A reply that does not fit is not kept:
    /// a retry then gets NFS4ERR_RETRY_UNCACHED_REP, never a second
    /// execution.
    pub async fn finish(mut self, reply: &Compound4res, solo_sequence: bool) {
        self.done = true;
        let cached = if self.cache_this || solo_sequence {
            match nextnfs_proto::xdr::to_bytes(reply) {
                Ok(bytes) if bytes.len() <= self.max_cached as usize => {
                    Some(Arc::new(reply.clone()))
                }
                Ok(bytes) => {
                    tracing::debug!(
                        size = bytes.len(),
                        max = self.max_cached,
                        "SEQUENCE: reply too big for the slot's reply cache, not cached"
                    );
                    None
                }
                Err(_) => None,
            }
        } else {
            None
        };
        self.manager
            .end_slot(&self.session_id, self.slot_id, self.sequence_id, cached)
            .await;
    }
}

impl Drop for SlotClaim {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let manager = self.manager.clone();
        let (session_id, slot_id, seq) = (self.session_id, self.slot_id, self.sequence_id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                manager.end_slot(&session_id, slot_id, seq, None).await;
            });
        }
    }
}

/// Channel attributes negotiated during CREATE_SESSION.
#[derive(Debug, Clone)]
pub struct ChannelAttrs {
    pub max_request_size: u32,
    pub max_response_size: u32,
    /// Largest reply kept in a slot's reply cache (`ca_maxresponsesize_cached`).
    pub max_response_size_cached: u32,
    pub max_ops: u32,
    pub max_requests: u32,
}

/// Upper bound on `ca_maxresponsesize_cached` granted at CREATE_SESSION:
/// with at most 64 slots a session caches at most 4 MiB of replies.
pub const MAX_CACHED_REPLY_SIZE: u32 = 65_536;

impl Default for ChannelAttrs {
    fn default() -> Self {
        Self {
            max_request_size: 1048576,  // 1MB
            max_response_size: 1048576, // 1MB
            max_response_size_cached: MAX_CACHED_REPLY_SIZE,
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

    /// Create a new session (CREATE_SESSION) with default channel attributes.
    pub async fn create_session(&self, client_id: u64, max_slots: u32) -> Session {
        self.create_session_with(client_id, max_slots, ChannelAttrs::default())
            .await
    }

    /// Create a new session with the negotiated fore-channel attributes.
    pub async fn create_session_with(
        &self,
        client_id: u64,
        max_slots: u32,
        fore_channel_attrs: ChannelAttrs,
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
            fore_channel_attrs,
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

    /// SEQUENCE's slot check (RFC 8881 §2.10.6.1). A new request
    /// (seqid == last + 1, wrapping) claims the slot; a retry of the last one
    /// gets its cached reply; anything else is an error.
    pub async fn begin_slot(
        &self,
        id: &SessionId,
        slot_id: u32,
        sequence_id: u32,
        cache_this: bool,
    ) -> SlotCheck {
        let mut sessions = self.sessions.write().await;
        let Some(session) = sessions.get_mut(id) else {
            return SlotCheck::BadSession;
        };
        let max_cached = session.fore_channel_attrs.max_response_size_cached;
        let Some(slot) = session.slots.get_mut(slot_id as usize) else {
            return SlotCheck::BadSlot;
        };
        if slot.in_use {
            // The client must wait for a reply before reusing a slot; a
            // retry of the running request is answered once it is done.
            return if sequence_id == slot.sequence_id {
                SlotCheck::InUse
            } else {
                SlotCheck::Misordered
            };
        }
        if sequence_id == slot.sequence_id.wrapping_add(1) {
            slot.sequence_id = sequence_id;
            slot.used = true;
            slot.in_use = true;
            slot.cached_reply = None;
            return SlotCheck::New(SlotClaim {
                manager: self.clone(),
                session_id: *id,
                slot_id,
                sequence_id,
                cache_this,
                max_cached,
                done: false,
            });
        }
        if slot.used && sequence_id == slot.sequence_id {
            return match &slot.cached_reply {
                Some(reply) => SlotCheck::Replay(reply.clone()),
                None => SlotCheck::ReplayUncached,
            };
        }
        SlotCheck::Misordered
    }

    /// Free a slot claimed for `sequence_id`, keeping `reply` for retries.
    async fn end_slot(
        &self,
        id: &SessionId,
        slot_id: u32,
        sequence_id: u32,
        reply: Option<Arc<Compound4res>>,
    ) {
        let mut sessions = self.sessions.write().await;
        let Some(slot) = sessions
            .get_mut(id)
            .and_then(|s| s.slots.get_mut(slot_id as usize))
        else {
            return;
        };
        if slot.in_use && slot.sequence_id == sequence_id {
            slot.in_use = false;
            slot.cached_reply = reply;
        }
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

    fn reply(tag: &str) -> Compound4res {
        Compound4res {
            status: nextnfs_proto::nfs4_proto::NfsStat4::Nfs4Ok,
            tag: tag.into(),
            resarray: vec![],
        }
    }

    #[tokio::test]
    async fn test_slot_new_replay_misordered() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let s = mgr.create_session(cid, 2).await;

        // seqid 0 on a fresh slot is neither new nor a retry.
        assert!(matches!(mgr.begin_slot(&s.id, 0, 0, true).await, SlotCheck::Misordered));
        assert!(matches!(mgr.begin_slot(&s.id, 0, 2, true).await, SlotCheck::Misordered));

        let SlotCheck::New(claim) = mgr.begin_slot(&s.id, 0, 1, true).await else {
            panic!("seqid 1 should be new");
        };
        // While running: a retry waits, the next seqid is misordered.
        assert!(matches!(mgr.begin_slot(&s.id, 0, 1, true).await, SlotCheck::InUse));
        assert!(matches!(mgr.begin_slot(&s.id, 0, 2, true).await, SlotCheck::Misordered));
        claim.finish(&reply("one"), false).await;

        match mgr.begin_slot(&s.id, 0, 1, true).await {
            SlotCheck::Replay(r) => assert_eq!(r.tag, "one"),
            other => panic!("expected replay, got {other:?}"),
        }
        // Slot 1 is independent of slot 0.
        assert!(matches!(mgr.begin_slot(&s.id, 1, 1, false).await, SlotCheck::New(_)));
        assert!(matches!(mgr.begin_slot(&s.id, 2, 1, false).await, SlotCheck::BadSlot));
        assert!(matches!(mgr.begin_slot(&[9; 16], 0, 1, false).await, SlotCheck::BadSession));
    }

    #[tokio::test]
    async fn test_slot_uncached_and_too_big() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let attrs = ChannelAttrs { max_response_size_cached: 16, ..ChannelAttrs::default() };
        let s = mgr.create_session_with(cid, 1, attrs).await;

        // Not asked to cache → retry is uncached.
        let SlotCheck::New(c) = mgr.begin_slot(&s.id, 0, 1, false).await else { panic!() };
        c.finish(&reply("x"), false).await;
        assert!(matches!(mgr.begin_slot(&s.id, 0, 1, false).await, SlotCheck::ReplayUncached));

        // Asked to cache but over ca_maxresponsesize_cached → uncached.
        let SlotCheck::New(c) = mgr.begin_slot(&s.id, 0, 2, true).await else { panic!() };
        c.finish(&reply("a-tag-longer-than-sixteen-bytes"), false).await;
        assert!(matches!(mgr.begin_slot(&s.id, 0, 2, true).await, SlotCheck::ReplayUncached));
    }

    #[tokio::test]
    async fn test_slot_dropped_claim_frees_slot() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let s = mgr.create_session(cid, 1).await;
        let SlotCheck::New(c) = mgr.begin_slot(&s.id, 0, 1, true).await else { panic!() };
        drop(c);
        // The release runs on a spawned task.
        for _ in 0..100 {
            if !matches!(mgr.begin_slot(&s.id, 0, 1, true).await, SlotCheck::InUse) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(matches!(mgr.begin_slot(&s.id, 0, 1, true).await, SlotCheck::ReplayUncached));
        assert!(matches!(mgr.begin_slot(&s.id, 0, 2, true).await, SlotCheck::New(_)));
    }

    #[tokio::test]
    async fn test_slot_seqid_wraps() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let s = mgr.create_session(cid, 1).await;
        {
            let mut sessions = mgr.sessions.write().await;
            let slot = &mut sessions.get_mut(&s.id).unwrap().slots[0];
            slot.sequence_id = u32::MAX;
            slot.used = true;
        }
        assert!(matches!(mgr.begin_slot(&s.id, 0, 0, false).await, SlotCheck::New(_)));
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
        assert!(!slot.in_use);
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
