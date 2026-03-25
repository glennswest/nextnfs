//! NFSv4.2 operation stubs.
//!
//! NFSv4.2 (RFC 7862) extends v4.1 with server-side copy, sparse file
//! support, and I/O hints.  These are optional — a server that advertises
//! v4.2 can return NFS4ERR_NOTSUPP for unimplemented ops and clients
//! will fall back gracefully.
//!
//! Currently the server negotiates v4.0 (v4.1+ returns MINOR_VERS_MISMATCH).
//! When v4.1 sessions are implemented, these stubs can be wired in:
//!
//! - COPY (op 60) — server-side file copy
//! - OFFLOAD_CANCEL (op 66) — cancel async copy
//! - OFFLOAD_STATUS (op 67) — poll async copy progress
//! - SEEK (op 69) — find data/hole in sparse files
//! - ALLOCATE (op 59) — preallocate space
//! - DEALLOCATE (op 64) — punch holes (deallocate space)
//! - IO_ADVISE (op 63) — I/O access pattern hints
//! - LAYOUTERROR (op 64) — report pNFS layout errors
//! - LAYOUTSTATS (op 65) — report pNFS layout statistics
//! - CLONE (op 71) — reflink/clone file range
//!
//! The minimum set for a working v4.2 mount is: COPY + SEEK + IO_ADVISE.
//! IO_ADVISE can safely return NFS4_OK with no action.

/// NFSv4.2 operation numbers (RFC 7862 §15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Nfs42Op {
    Allocate = 59,
    Copy = 60,
    CopyNotify = 61,
    Deallocate = 62,
    IoAdvise = 63,
    LayoutError = 64,
    LayoutStats = 65,
    OffloadCancel = 66,
    OffloadStatus = 67,
    ReadPlus = 68,
    Seek = 69,
    WriteSame = 70,
    Clone = 71,
}

/// SEEK data/hole type (RFC 7862 §15.11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DataContent {
    Data = 0,
    Hole = 1,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nfs42_op_values() {
        assert_eq!(Nfs42Op::Allocate as u32, 59);
        assert_eq!(Nfs42Op::Copy as u32, 60);
        assert_eq!(Nfs42Op::CopyNotify as u32, 61);
        assert_eq!(Nfs42Op::Deallocate as u32, 62);
        assert_eq!(Nfs42Op::IoAdvise as u32, 63);
        assert_eq!(Nfs42Op::LayoutError as u32, 64);
        assert_eq!(Nfs42Op::LayoutStats as u32, 65);
        assert_eq!(Nfs42Op::OffloadCancel as u32, 66);
        assert_eq!(Nfs42Op::OffloadStatus as u32, 67);
        assert_eq!(Nfs42Op::ReadPlus as u32, 68);
        assert_eq!(Nfs42Op::Seek as u32, 69);
        assert_eq!(Nfs42Op::WriteSame as u32, 70);
        assert_eq!(Nfs42Op::Clone as u32, 71);
    }

    #[test]
    fn test_data_content_values() {
        assert_eq!(DataContent::Data as u32, 0);
        assert_eq!(DataContent::Hole as u32, 1);
    }

    #[test]
    fn test_nfs42_op_equality() {
        assert_eq!(Nfs42Op::Copy, Nfs42Op::Copy);
        assert_ne!(Nfs42Op::Copy, Nfs42Op::Seek);
    }

    #[test]
    fn test_nfs42_op_clone() {
        let a = Nfs42Op::Clone;
        let b = a;
        assert_eq!(a, b);
    }
}
