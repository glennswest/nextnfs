use nextnfs_proto::nfs4_proto::{NfsFh4, NfsLockType4};
use multi_index_map::MultiIndexMap;

pub type LockingStateDb = MultiIndexLockingStateMap;

#[derive(Debug, Clone)]
pub enum LockType {
    Open,
    ByteRange,
}

#[derive(MultiIndexMap, Debug, Clone)]
#[multi_index_derive(Debug, Clone)]
pub struct LockingState {
    #[multi_index(hashed_unique)]
    pub stateid: [u8; 12],
    pub seqid: u32,
    #[multi_index(hashed_non_unique)]
    pub client_id: u64,
    #[multi_index(hashed_non_unique)]
    pub owner: Vec<u8>,
    pub lock_type: LockType,
    #[multi_index(hashed_non_unique)]
    pub filehandle_id: NfsFh4,
    pub start: Option<u64>,
    pub length: Option<u64>,
    pub share_access: Option<u32>,
    pub share_deny: Option<u32>,
    /// For byte-range locks: read vs write
    pub nfs_lock_type: Option<NfsLockType4>,
}

impl LockingState {
    pub fn new_shared_reservation(
        filehandle_id: NfsFh4,
        stateid: [u8; 12],
        client_id: u64,
        owner: Vec<u8>,
        share_access: u32,
        share_deny: u32,
    ) -> Self {
        LockingState {
            stateid,
            seqid: 1,
            client_id,
            owner,
            lock_type: LockType::Open,
            filehandle_id,
            start: None,
            length: None,
            share_access: Some(share_access),
            share_deny: Some(share_deny),
            nfs_lock_type: None,
        }
    }

    pub fn new_byte_range_lock(
        filehandle_id: NfsFh4,
        stateid: [u8; 12],
        client_id: u64,
        owner: Vec<u8>,
        lock_type: NfsLockType4,
        offset: u64,
        length: u64,
    ) -> Self {
        LockingState {
            stateid,
            seqid: 1,
            client_id,
            owner,
            lock_type: LockType::ByteRange,
            filehandle_id,
            start: Some(offset),
            length: Some(length),
            share_access: None,
            share_deny: None,
            nfs_lock_type: Some(lock_type),
        }
    }

    /// Check if this byte-range lock conflicts with a proposed lock.
    /// Returns true if there IS a conflict.
    pub fn conflicts_with(
        &self,
        offset: u64,
        length: u64,
        lock_type: &NfsLockType4,
        owner: &[u8],
        client_id: u64,
    ) -> bool {
        // Only byte-range locks can conflict
        let (my_start, my_length) = match (&self.lock_type, self.start, self.length) {
            (LockType::ByteRange, Some(s), Some(l)) => (s, l),
            _ => return false,
        };

        // Same owner on same client — no conflict (lock upgrade/coalesce)
        if self.client_id == client_id && self.owner == owner {
            return false;
        }

        // Check range overlap
        // NFS length 0 means "to end of file" (0xFFFFFFFFFFFFFFFF)
        let my_end = if my_length == 0 {
            u64::MAX
        } else {
            my_start.saturating_add(my_length)
        };
        let req_end = if length == 0 {
            u64::MAX
        } else {
            offset.saturating_add(length)
        };

        if my_start >= req_end || offset >= my_end {
            return false; // no overlap
        }

        // Overlapping ranges — read locks don't conflict with each other
        let my_is_write = matches!(
            self.nfs_lock_type,
            Some(NfsLockType4::WriteLt) | Some(NfsLockType4::WritewLt)
        );
        let req_is_write = matches!(
            lock_type,
            NfsLockType4::WriteLt | NfsLockType4::WritewLt
        );

        // Conflict if either is a write lock
        my_is_write || req_is_write
    }
}
