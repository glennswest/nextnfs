//! RPC-over-RDMA transport layer (RFC 8166, RFC 8267).
//!
//! Provides RDMA transport framing for NFS operations. The RPCRdma protocol
//! wraps RPC messages in RDMA headers that enable zero-copy data transfer
//! for large READ/WRITE payloads via RDMA Send/Receive and Read/Write verbs.
//!
//! ## Protocol Overview (RFC 8166)
//!
//! - **RDMA_MSG** — RPC call/reply carried inline in RDMA Send
//! - **RDMA_NOMSG** — RPC payload transferred via RDMA Read/Write (no inline data)
//! - **RDMA_MSGP** — RPC message with padding for alignment
//! - **RDMA_DONE** — Signals completion of a chunked transfer
//! - **RDMA_ERROR** — Protocol error notification
//!
//! ## Architecture
//!
//! The transport operates over RDMA queue pairs (QPs). Each connection has:
//! - A completion queue (CQ) for send/receive completions
//! - A queue pair for RDMA Send/Receive/Read/Write operations
//! - Memory regions registered for zero-copy DMA access
//!
//! On Linux, this uses `rdma-core` (libibverbs) via the system's RDMA stack.
//! The transport is behind the `rdma` feature flag since it requires
//! RDMA-capable hardware (InfiniBand, RoCEv2, iWARP).

use std::fmt;

/// RPCRdma protocol version (RFC 8166 §3).
pub const RPCRDMA_VERSION: u32 = 1;

/// Maximum inline threshold — payloads larger than this use RDMA Read/Write.
pub const DEFAULT_INLINE_THRESHOLD: u32 = 4096;

/// RPCRdma message types (RFC 8166 §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RdmaProc {
    /// RPC message carried inline in RDMA Send.
    RdmaMsg = 0,
    /// RPC payload transferred via RDMA Read/Write (no inline data).
    RdmaNoMsg = 1,
    /// RPC message with padding for alignment.
    RdmaMsgP = 2,
    /// Signals completion of a chunked transfer.
    RdmaDone = 3,
    /// Protocol error notification.
    RdmaError = 4,
}

impl TryFrom<u32> for RdmaProc {
    type Error = u32;
    fn try_from(v: u32) -> Result<Self, u32> {
        match v {
            0 => Ok(RdmaProc::RdmaMsg),
            1 => Ok(RdmaProc::RdmaNoMsg),
            2 => Ok(RdmaProc::RdmaMsgP),
            3 => Ok(RdmaProc::RdmaDone),
            4 => Ok(RdmaProc::RdmaError),
            _ => Err(v),
        }
    }
}

/// RPCRdma message header (RFC 8166 §4.1).
///
/// Every RPC-over-RDMA message begins with this fixed header, followed by
/// optional chunk lists for read/write operations.
#[derive(Clone, Debug)]
pub struct RdmaHeader {
    /// Transaction ID (maps to RPC XID).
    pub xid: u32,
    /// Protocol version (must be RPCRDMA_VERSION=1).
    pub vers: u32,
    /// Credits — flow control tokens granted to peer.
    pub credits: u32,
    /// Message type.
    pub proc_type: RdmaProc,
}

impl RdmaHeader {
    /// Serialize header to big-endian bytes (4 × u32 = 16 bytes).
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&self.xid.to_be_bytes());
        buf[4..8].copy_from_slice(&self.vers.to_be_bytes());
        buf[8..12].copy_from_slice(&self.credits.to_be_bytes());
        buf[12..16].copy_from_slice(&(self.proc_type as u32).to_be_bytes());
        buf
    }

    /// Parse header from big-endian bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, RdmaError> {
        if buf.len() < 16 {
            return Err(RdmaError::ShortHeader(buf.len()));
        }
        let xid = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let vers = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let credits = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let proc_val = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        let proc_type = RdmaProc::try_from(proc_val)
            .map_err(RdmaError::UnknownProc)?;

        if vers != RPCRDMA_VERSION {
            return Err(RdmaError::VersionMismatch(vers));
        }

        Ok(RdmaHeader {
            xid,
            vers,
            credits,
            proc_type,
        })
    }
}

/// RDMA chunk descriptor — describes a remote memory region for
/// RDMA Read or Write operations (RFC 8166 §4.2).
#[derive(Clone, Debug)]
pub struct RdmaSegment {
    /// Handle for the registered memory region.
    pub handle: u32,
    /// Length of the data segment in bytes.
    pub length: u32,
    /// Offset within the memory region.
    pub offset: u64,
}

impl RdmaSegment {
    /// Serialize segment to bytes (4 + 4 + 8 = 16 bytes).
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&self.handle.to_be_bytes());
        buf[4..8].copy_from_slice(&self.length.to_be_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_be_bytes());
        buf
    }

    /// Parse segment from bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, RdmaError> {
        if buf.len() < 16 {
            return Err(RdmaError::ShortHeader(buf.len()));
        }
        Ok(RdmaSegment {
            handle: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            length: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
            offset: u64::from_be_bytes(buf[8..16].try_into().unwrap()),
        })
    }
}

/// Read chunk — client offers a remote buffer for the server to RDMA Write into.
/// Used for NFS READ: the server writes file data directly into client memory.
#[derive(Clone, Debug)]
pub struct ReadChunk {
    /// Position in the RPC reply where this data belongs.
    pub position: u32,
    /// The RDMA segment descriptor.
    pub target: RdmaSegment,
}

/// Write chunk — client offers a remote buffer for the server to RDMA Read from.
/// Used for NFS WRITE: the server reads file data directly from client memory.
#[derive(Clone, Debug)]
pub struct WriteChunk {
    /// The RDMA segment descriptors (may be a scatter/gather list).
    pub targets: Vec<RdmaSegment>,
}

/// RDMA transport configuration.
#[derive(Clone, Debug)]
pub struct RdmaConfig {
    /// RDMA device name (e.g., "mlx5_0", "rxe0").
    pub device: String,
    /// Port number on the RDMA device (usually 1).
    pub port: u8,
    /// GID index for RoCEv2 addressing.
    pub gid_index: u32,
    /// Maximum inline data size before using RDMA Read/Write.
    pub inline_threshold: u32,
    /// Number of send queue entries.
    pub send_queue_depth: u32,
    /// Number of receive queue entries.
    pub recv_queue_depth: u32,
    /// Maximum number of scatter/gather entries per WR.
    pub max_sge: u32,
}

impl Default for RdmaConfig {
    fn default() -> Self {
        RdmaConfig {
            device: String::new(),
            port: 1,
            gid_index: 0,
            inline_threshold: DEFAULT_INLINE_THRESHOLD,
            send_queue_depth: 128,
            recv_queue_depth: 128,
            max_sge: 4,
        }
    }
}

/// RDMA transport errors.
#[derive(Debug)]
pub enum RdmaError {
    /// Header too short to parse.
    ShortHeader(usize),
    /// Unknown RPCRdma procedure type.
    UnknownProc(u32),
    /// Version mismatch (expected RPCRDMA_VERSION=1).
    VersionMismatch(u32),
    /// RDMA device not found or unavailable.
    DeviceNotFound(String),
    /// Memory registration failed.
    MemoryRegistration(String),
    /// Queue pair creation failed.
    QueuePairError(String),
    /// I/O error.
    Io(std::io::Error),
}

impl fmt::Display for RdmaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RdmaError::ShortHeader(len) => write!(f, "RDMA header too short: {} bytes", len),
            RdmaError::UnknownProc(v) => write!(f, "unknown RDMA proc type: {}", v),
            RdmaError::VersionMismatch(v) => write!(f, "RDMA version mismatch: {} (expected {})", v, RPCRDMA_VERSION),
            RdmaError::DeviceNotFound(d) => write!(f, "RDMA device not found: {}", d),
            RdmaError::MemoryRegistration(e) => write!(f, "RDMA memory registration failed: {}", e),
            RdmaError::QueuePairError(e) => write!(f, "RDMA queue pair error: {}", e),
            RdmaError::Io(e) => write!(f, "RDMA I/O error: {}", e),
        }
    }
}

impl std::error::Error for RdmaError {}

impl From<std::io::Error> for RdmaError {
    fn from(e: std::io::Error) -> Self {
        RdmaError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rdma_header_roundtrip() {
        let header = RdmaHeader {
            xid: 0xDEADBEEF,
            vers: RPCRDMA_VERSION,
            credits: 32,
            proc_type: RdmaProc::RdmaMsg,
        };
        let bytes = header.to_bytes();
        let parsed = RdmaHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.xid, 0xDEADBEEF);
        assert_eq!(parsed.vers, RPCRDMA_VERSION);
        assert_eq!(parsed.credits, 32);
        assert_eq!(parsed.proc_type, RdmaProc::RdmaMsg);
    }

    #[test]
    fn test_rdma_header_all_proc_types() {
        for (proc_type, value) in [
            (RdmaProc::RdmaMsg, 0),
            (RdmaProc::RdmaNoMsg, 1),
            (RdmaProc::RdmaMsgP, 2),
            (RdmaProc::RdmaDone, 3),
            (RdmaProc::RdmaError, 4),
        ] {
            let header = RdmaHeader {
                xid: 42,
                vers: RPCRDMA_VERSION,
                credits: 16,
                proc_type,
            };
            let bytes = header.to_bytes();
            let parsed = RdmaHeader::from_bytes(&bytes).unwrap();
            assert_eq!(parsed.proc_type, proc_type);
            assert_eq!(parsed.proc_type as u32, value);
        }
    }

    #[test]
    fn test_rdma_header_short_buffer() {
        let bytes = [0u8; 8]; // too short
        let result = RdmaHeader::from_bytes(&bytes);
        assert!(result.is_err());
        match result.unwrap_err() {
            RdmaError::ShortHeader(len) => assert_eq!(len, 8),
            _ => panic!("Expected ShortHeader error"),
        }
    }

    #[test]
    fn test_rdma_header_version_mismatch() {
        let mut bytes = [0u8; 16];
        bytes[4..8].copy_from_slice(&99u32.to_be_bytes()); // wrong version
        let result = RdmaHeader::from_bytes(&bytes);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RdmaError::VersionMismatch(99)));
    }

    #[test]
    fn test_rdma_header_unknown_proc() {
        let mut bytes = [0u8; 16];
        bytes[4..8].copy_from_slice(&RPCRDMA_VERSION.to_be_bytes());
        bytes[12..16].copy_from_slice(&255u32.to_be_bytes()); // unknown proc
        let result = RdmaHeader::from_bytes(&bytes);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RdmaError::UnknownProc(255)));
    }

    #[test]
    fn test_rdma_segment_roundtrip() {
        let seg = RdmaSegment {
            handle: 0x12345678,
            length: 65536,
            offset: 0xABCDEF0123456789,
        };
        let bytes = seg.to_bytes();
        let parsed = RdmaSegment::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.handle, 0x12345678);
        assert_eq!(parsed.length, 65536);
        assert_eq!(parsed.offset, 0xABCDEF0123456789);
    }

    #[test]
    fn test_rdma_segment_short_buffer() {
        let bytes = [0u8; 8]; // too short
        let result = RdmaSegment::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_rdma_config_default() {
        let config = RdmaConfig::default();
        assert_eq!(config.port, 1);
        assert_eq!(config.gid_index, 0);
        assert_eq!(config.inline_threshold, DEFAULT_INLINE_THRESHOLD);
        assert_eq!(config.send_queue_depth, 128);
        assert_eq!(config.recv_queue_depth, 128);
        assert_eq!(config.max_sge, 4);
    }

    #[test]
    fn test_rdma_error_display() {
        let err = RdmaError::DeviceNotFound("mlx5_0".to_string());
        assert!(err.to_string().contains("mlx5_0"));

        let err = RdmaError::VersionMismatch(99);
        assert!(err.to_string().contains("99"));

        let err = RdmaError::UnknownProc(42);
        assert!(err.to_string().contains("42"));
    }

    #[test]
    fn test_rdma_proc_try_from() {
        assert_eq!(RdmaProc::try_from(0), Ok(RdmaProc::RdmaMsg));
        assert_eq!(RdmaProc::try_from(1), Ok(RdmaProc::RdmaNoMsg));
        assert_eq!(RdmaProc::try_from(4), Ok(RdmaProc::RdmaError));
        assert_eq!(RdmaProc::try_from(5), Err(5));
        assert_eq!(RdmaProc::try_from(99), Err(99));
    }

    #[test]
    fn test_read_chunk_fields() {
        let chunk = ReadChunk {
            position: 100,
            target: RdmaSegment {
                handle: 1,
                length: 4096,
                offset: 0,
            },
        };
        assert_eq!(chunk.position, 100);
        assert_eq!(chunk.target.length, 4096);
    }

    #[test]
    fn test_write_chunk_scatter_gather() {
        let chunk = WriteChunk {
            targets: vec![
                RdmaSegment { handle: 1, length: 4096, offset: 0 },
                RdmaSegment { handle: 2, length: 4096, offset: 4096 },
                RdmaSegment { handle: 3, length: 2048, offset: 8192 },
            ],
        };
        assert_eq!(chunk.targets.len(), 3);
        let total: u32 = chunk.targets.iter().map(|s| s.length).sum();
        assert_eq!(total, 10240);
    }
}
