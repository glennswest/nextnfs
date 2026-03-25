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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_byte_lock(
        fh_id: NfsFh4,
        client_id: u64,
        owner: &[u8],
        lock_type: NfsLockType4,
        offset: u64,
        length: u64,
    ) -> LockingState {
        LockingState::new_byte_range_lock(
            fh_id,
            [0u8; 12],
            client_id,
            owner.to_vec(),
            lock_type,
            offset,
            length,
        )
    }

    #[test]
    fn test_open_lock_never_conflicts() {
        let fh_id = [1u8; 26];
        let lock = LockingState::new_shared_reservation(
            fh_id,
            [0u8; 12],
            1,
            b"owner".to_vec(),
            1,
            0,
        );
        // Open locks never conflict with byte-range requests
        assert!(!lock.conflicts_with(0, 100, &NfsLockType4::WriteLt, b"other", 2));
    }

    #[test]
    fn test_write_vs_write_conflict() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WriteLt, 0, 100);
        assert!(lock.conflicts_with(50, 100, &NfsLockType4::WriteLt, b"owner2", 2));
    }

    #[test]
    fn test_read_vs_read_no_conflict() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::ReadLt, 0, 100);
        assert!(!lock.conflicts_with(50, 50, &NfsLockType4::ReadLt, b"owner2", 2));
    }

    #[test]
    fn test_read_vs_write_conflict() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::ReadLt, 0, 100);
        assert!(lock.conflicts_with(50, 50, &NfsLockType4::WriteLt, b"owner2", 2));
    }

    #[test]
    fn test_write_vs_read_conflict() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WriteLt, 0, 100);
        assert!(lock.conflicts_with(50, 50, &NfsLockType4::ReadLt, b"owner2", 2));
    }

    #[test]
    fn test_same_owner_no_conflict() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WriteLt, 0, 100);
        // Same client_id + owner = lock coalesce, no conflict
        assert!(!lock.conflicts_with(50, 100, &NfsLockType4::WriteLt, b"owner1", 1));
    }

    #[test]
    fn test_non_overlapping_ranges() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WriteLt, 0, 100);
        // [100..200) doesn't overlap [0..100)
        assert!(!lock.conflicts_with(100, 100, &NfsLockType4::WriteLt, b"owner2", 2));
    }

    #[test]
    fn test_adjacent_ranges_no_conflict() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WriteLt, 0, 50);
        // [50..) starts exactly where [0..50) ends
        assert!(!lock.conflicts_with(50, 50, &NfsLockType4::WriteLt, b"owner2", 2));
    }

    #[test]
    fn test_zero_length_means_eof() {
        let fh_id = [1u8; 26];
        // length=0 means "to end of file"
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WriteLt, 0, 0);
        // Should conflict with any range
        assert!(lock.conflicts_with(999999, 1, &NfsLockType4::WriteLt, b"owner2", 2));
    }

    #[test]
    fn test_request_zero_length_means_eof() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WriteLt, 100, 50);
        // Request with length=0 goes to EOF, overlaps [100..150)
        assert!(lock.conflicts_with(0, 0, &NfsLockType4::WriteLt, b"owner2", 2));
    }

    #[test]
    fn test_writew_lt_is_write() {
        let fh_id = [1u8; 26];
        let lock = make_byte_lock(fh_id, 1, b"owner1", NfsLockType4::WritewLt, 0, 100);
        // WritewLt should also conflict like WriteLt
        assert!(lock.conflicts_with(50, 50, &NfsLockType4::ReadLt, b"owner2", 2));
    }

    #[test]
    fn test_shared_reservation_fields() {
        let fh_id = [1u8; 26];
        let lock = LockingState::new_shared_reservation(
            fh_id, [5u8; 12], 42, b"myowner".to_vec(), 3, 1,
        );
        assert_eq!(lock.client_id, 42);
        assert_eq!(lock.owner, b"myowner");
        assert_eq!(lock.share_access, Some(3));
        assert_eq!(lock.share_deny, Some(1));
        assert!(lock.start.is_none());
        assert!(lock.length.is_none());
        assert!(matches!(lock.lock_type, LockType::Open));
    }

    #[test]
    fn test_byte_range_lock_fields() {
        let fh_id = [2u8; 26];
        let lock = LockingState::new_byte_range_lock(
            fh_id, [7u8; 12], 99, b"lockowner".to_vec(),
            NfsLockType4::ReadLt, 1024, 4096,
        );
        assert_eq!(lock.client_id, 99);
        assert_eq!(lock.start, Some(1024));
        assert_eq!(lock.length, Some(4096));
        assert!(matches!(lock.lock_type, LockType::ByteRange));
        assert!(matches!(lock.nfs_lock_type, Some(NfsLockType4::ReadLt)));
        assert_eq!(lock.seqid, 1);
    }
}
