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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

use nextnfs_proto::nfs4_proto::{Compound4res, CreateSession4resok, NfsStat4};

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

/// Session manager — tracks the NFSv4.1 clients and their sessions.
///
/// Session ids are `boot ‖ counter`: 8 bytes chosen at random when the
/// manager is made (each server start) and a counter that only goes up, so
/// an id is never handed out twice — not after a DESTROY_SESSION, not to two
/// CREATE_SESSIONs at once, not across a restart (#128). Client ids are made
/// the same way (random high 32 bits, counter low), so a client id from
/// before a restart is unknown afterwards (NFS4ERR_STALE_CLIENTID).
#[derive(Debug, Clone)]
pub struct SessionManager {
    /// Lock order: `clients` before `sessions`.
    clients: Arc<RwLock<HashMap<u64, ClientRecord>>>,
    sessions: Arc<RwLock<HashMap<SessionId, Session>>>,
    boot: [u8; 8],
    client_prefix: u64,
    next_client_id: Arc<AtomicU32>,
    next_session: Arc<AtomicU64>,
}

/// A client registered by EXCHANGE_ID: its CREATE_SESSION sequence
/// (RFC 8881 §18.36.4) and the reply to the last CREATE_SESSION, for a retry.
#[derive(Debug, Clone, Default)]
struct ClientRecord {
    /// Last `csa_sequence` accepted; EXCHANGE_ID returns
    /// `eir_sequenceid` = this + 1.
    sequence_id: u32,
    last_reply: Option<CreateSession4resok>,
}

/// `eir_sequenceid` EXCHANGE_ID gives a new client: its first CREATE_SESSION
/// carries this `csa_sequence`.
pub const FIRST_CREATE_SESSION_SEQUENCE: u32 = 1;

impl SessionManager {
    pub fn new() -> Self {
        let mut boot: [u8; 8] = rand::random();
        if boot == [0; 8] {
            boot[0] = 1;
        }
        let client_prefix = u64::from(rand::random::<u32>().max(1)) << 32;
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            boot,
            client_prefix,
            next_client_id: Arc::new(AtomicU32::new(1)),
            next_session: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Allocate a new client ID and register the client (EXCHANGE_ID). Its
    /// first CREATE_SESSION carries [`FIRST_CREATE_SESSION_SEQUENCE`].
    pub async fn allocate_client_id(&self) -> u64 {
        let client_id =
            self.client_prefix | u64::from(self.next_client_id.fetch_add(1, Ordering::Relaxed));
        let record = ClientRecord {
            sequence_id: FIRST_CREATE_SESSION_SEQUENCE.wrapping_sub(1),
            last_reply: None,
        };
        self.clients.write().await.insert(client_id, record);
        client_id
    }

    /// A session id never handed out before by this manager (or, but for an
    /// 8-byte random collision, by an earlier server start).
    fn new_session_id(&self) -> SessionId {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&self.boot);
        id[8..].copy_from_slice(&self.next_session.fetch_add(1, Ordering::Relaxed).to_be_bytes());
        id
    }

    /// Insert `session` under a fresh id; never replaces a live session.
    fn insert_new(&self, sessions: &mut HashMap<SessionId, Session>, mut session: Session) -> Session {
        loop {
            match sessions.entry(session.id) {
                Entry::Vacant(v) => {
                    v.insert(session.clone());
                    return session;
                }
                Entry::Occupied(_) => session.id = self.new_session_id(),
            }
        }
    }

    fn new_session(&self, client_id: u64, max_slots: u32, fore_channel_attrs: ChannelAttrs) -> Session {
        Session {
            id: self.new_session_id(),
            client_id,
            slots: (0..max_slots).map(|_| SlotState::default()).collect(),
            fore_channel_attrs,
            bound_connections: HashSet::new(),
        }
    }

    /// Create a new session with default channel attributes, without the
    /// client and sequence checks of [`Self::create_session_for_client`].
    pub async fn create_session(&self, client_id: u64, max_slots: u32) -> Session {
        self.create_session_with(client_id, max_slots, ChannelAttrs::default())
            .await
    }

    /// Create a new session with the negotiated fore-channel attributes,
    /// without the client and sequence checks of
    /// [`Self::create_session_for_client`].
    pub async fn create_session_with(
        &self,
        client_id: u64,
        max_slots: u32,
        fore_channel_attrs: ChannelAttrs,
    ) -> Session {
        let session = self.new_session(client_id, max_slots, fore_channel_attrs);
        let mut sessions = self.sessions.write().await;
        self.insert_new(&mut sessions, session)
    }

    /// CREATE_SESSION (RFC 8881 §18.36.4), checked against the client record
    /// as one step: an unknown client is NFS4ERR_STALE_CLIENTID;
    /// `csa_sequence` = last + 1 creates a session and `reply` builds the
    /// answer, which is kept; = last is a retry and gets the kept answer
    /// again (the same session, no second one); anything else is
    /// NFS4ERR_SEQ_MISORDERED.
    pub async fn create_session_for_client(
        &self,
        client_id: u64,
        csa_sequence: u32,
        max_slots: u32,
        fore_channel_attrs: ChannelAttrs,
        reply: impl FnOnce(&Session) -> CreateSession4resok,
    ) -> Result<CreateSession4resok, NfsStat4> {
        let mut clients = self.clients.write().await;
        let Some(client) = clients.get_mut(&client_id) else {
            return Err(NfsStat4::Nfs4errStaleClientid);
        };
        if csa_sequence == client.sequence_id {
            if let Some(cached) = &client.last_reply {
                return Ok(cached.clone());
            }
        }
        if csa_sequence != client.sequence_id.wrapping_add(1) {
            return Err(NfsStat4::Nfs4errSeqMisordered);
        }
        let session = self.new_session(client_id, max_slots, fore_channel_attrs);
        let session = {
            let mut sessions = self.sessions.write().await;
            self.insert_new(&mut sessions, session)
        };
        let res = reply(&session);
        client.sequence_id = csa_sequence;
        client.last_reply = Some(res.clone());
        Ok(res)
    }

    /// The fore-channel attributes negotiated for a session (no copy of
    /// its slots), or None for an unknown session.
    pub async fn fore_channel_attrs(&self, id: &SessionId) -> Option<ChannelAttrs> {
        let sessions = self.sessions.read().await;
        sessions.get(id).map(|s| s.fore_channel_attrs.clone())
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

    /// Destroy a client and all its sessions (DESTROY_CLIENTID).
    pub async fn destroy_client(&self, client_id: u64) -> usize {
        let mut clients = self.clients.write().await;
        clients.remove(&client_id);
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
        assert_eq!(cid as u32, 1);

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
        assert_eq!(id1 as u32, 1);
        assert_eq!(id2, id1 + 1);
        assert_eq!(id3, id1 + 2);
        // The high half is the per-start prefix, never 0.
        assert_ne!(id1 >> 32, 0);
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
        assert_eq!(cid as u32, 1);
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

    /// #128 (finding 124a859a1973): ids came from `sessions.len()`, so
    /// A, B; destroy A; C got B's id and replaced B, resetting its slots.
    #[tokio::test]
    async fn test_session_id_not_reused_after_destroy() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let a = mgr.create_session(cid, 2).await;
        let b = mgr.create_session(cid, 2).await;
        let SlotCheck::New(claim) = mgr.begin_slot(&b.id, 0, 1, true).await else { panic!() };
        claim.finish(&reply("b"), false).await;

        assert!(mgr.destroy_session(&a.id).await);
        let c = mgr.create_session(cid, 2).await;
        assert_ne!(c.id, a.id);
        assert_ne!(c.id, b.id);
        // B is untouched: its slot still holds seqid 1 and the cached reply.
        match mgr.begin_slot(&b.id, 0, 1, true).await {
            SlotCheck::Replay(r) => assert_eq!(r.tag, "b"),
            other => panic!("B's slot was reset: {other:?}"),
        }
        assert_eq!(mgr.get_session(&c.id).await.unwrap().id, c.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_create_session_unique() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let tasks: Vec<_> = (0..64)
            .map(|_| {
                let mgr = mgr.clone();
                tokio::spawn(async move { mgr.create_session(cid, 1).await.id })
            })
            .collect();
        let mut ids = HashSet::new();
        for t in tasks {
            assert!(ids.insert(t.await.unwrap()), "duplicate session id");
        }
        assert_eq!(mgr.sessions.read().await.len(), 64);
    }

    /// A restart is a new manager: its session and client ids differ from
    /// the old ones, so an old id is BADSESSION / STALE_CLIENTID.
    #[tokio::test]
    async fn test_ids_unique_across_restart() {
        let before = SessionManager::new();
        let cid = before.allocate_client_id().await;
        let old = before.create_session(cid, 1).await;

        let after = SessionManager::new();
        let cid2 = after.allocate_client_id().await;
        let new = after.create_session(cid2, 1).await;
        assert_ne!(old.id, new.id);
        assert_ne!(cid, cid2);
        assert!(matches!(after.begin_slot(&old.id, 0, 1, false).await, SlotCheck::BadSession));
        let r = after
            .create_session_for_client(cid, 1, 1, ChannelAttrs::default(), |s| resok(s, 1))
            .await;
        assert_eq!(r, Err(NfsStat4::Nfs4errStaleClientid));
    }

    fn resok(s: &Session, seq: u32) -> CreateSession4resok {
        let chan = nextnfs_proto::nfs4_proto::ChannelAttrs4 {
            ca_headerpadsize: 0,
            ca_maxrequestsize: 0,
            ca_maxresponsesize: 0,
            ca_maxresponsesize_cached: 0,
            ca_maxoperations: 0,
            ca_maxrequests: s.slots.len() as u32,
            ca_rdma_ird: vec![],
        };
        CreateSession4resok {
            csr_sessionid: s.id,
            csr_sequenceid: seq,
            csr_flags: 0,
            csr_fore_chan_attrs: chan.clone(),
            csr_back_chan_attrs: chan,
        }
    }

    #[tokio::test]
    async fn test_create_session_sequence_and_replay() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let attrs = ChannelAttrs::default;

        // Before any CREATE_SESSION only the first sequence is accepted.
        assert_eq!(
            mgr.create_session_for_client(cid, 2, 1, attrs(), |s| resok(s, 2)).await,
            Err(NfsStat4::Nfs4errSeqMisordered)
        );
        assert_eq!(
            mgr.create_session_for_client(cid, 0, 1, attrs(), |s| resok(s, 0)).await,
            Err(NfsStat4::Nfs4errSeqMisordered)
        );
        let first = mgr
            .create_session_for_client(cid, FIRST_CREATE_SESSION_SEQUENCE, 1, attrs(), |s| resok(s, 1))
            .await
            .unwrap();

        // A retry gets the same reply and makes no second session.
        let again = mgr
            .create_session_for_client(cid, 1, 1, attrs(), |_| panic!("retry executed again"))
            .await
            .unwrap();
        assert_eq!(again, first);
        assert_eq!(mgr.sessions.read().await.len(), 1);

        // Next sequence makes a new session; skipping one is misordered.
        let second = mgr
            .create_session_for_client(cid, 2, 1, attrs(), |s| resok(s, 2))
            .await
            .unwrap();
        assert_ne!(second.csr_sessionid, first.csr_sessionid);
        assert_eq!(
            mgr.create_session_for_client(cid, 4, 1, attrs(), |s| resok(s, 4)).await,
            Err(NfsStat4::Nfs4errSeqMisordered)
        );
        assert_eq!(mgr.sessions.read().await.len(), 2);

        // Unknown client, and a client gone by DESTROY_CLIENTID.
        assert_eq!(
            mgr.create_session_for_client(cid + 100, 1, 1, attrs(), |s| resok(s, 1)).await,
            Err(NfsStat4::Nfs4errStaleClientid)
        );
        assert_eq!(mgr.destroy_client(cid).await, 2);
        assert_eq!(
            mgr.create_session_for_client(cid, 3, 1, attrs(), |s| resok(s, 3)).await,
            Err(NfsStat4::Nfs4errStaleClientid)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_create_session_same_sequence_one_session() {
        let mgr = SessionManager::new();
        let cid = mgr.allocate_client_id().await;
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let mgr = mgr.clone();
                tokio::spawn(async move {
                    mgr.create_session_for_client(cid, 1, 1, ChannelAttrs::default(), |s| resok(s, 1))
                        .await
                        .unwrap()
                        .csr_sessionid
                })
            })
            .collect();
        let mut ids = HashSet::new();
        for t in tasks {
            ids.insert(t.await.unwrap());
        }
        // Every concurrent retry got the one session the first one made.
        assert_eq!(ids.len(), 1);
        assert_eq!(mgr.sessions.read().await.len(), 1);
    }
}
