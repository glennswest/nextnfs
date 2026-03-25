//! Sun RPC (RFC 5531) client implementation.
//!
//! Handles TCP record-marking framing, RPC message construction, and AUTH_SYS credentials.

#![allow(dead_code)]

use super::xdr::{XdrDecoder, XdrEncoder};
use bytes::{BufMut, BytesMut};
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, trace};

// RPC constants
const RPC_VERSION: u32 = 2;
const MSG_TYPE_CALL: u32 = 0;
const MSG_TYPE_REPLY: u32 = 1;
const REPLY_ACCEPTED: u32 = 0;
const ACCEPT_SUCCESS: u32 = 0;
const AUTH_SYS: u32 = 1;
const AUTH_NONE: u32 = 0;

// NFS program numbers
pub const NFS_PROGRAM: u32 = 100003;
pub const MOUNT_PROGRAM: u32 = 100005;
pub const NLM_PROGRAM: u32 = 100021;

// NFS versions
pub const NFS_V3: u32 = 3;
pub const NFS_V4: u32 = 4;
pub const MOUNT_V3: u32 = 3;

/// RPC client that speaks to an NFS server over TCP.
pub struct RpcClient {
    stream: Mutex<TcpStream>,
    xid: AtomicU32,
    uid: u32,
    gid: u32,
    server: String,
    port: u16,
}

impl RpcClient {
    /// Connect to an NFS server.
    pub async fn connect(server: &str, port: u16, uid: u32, gid: u32) -> anyhow::Result<Self> {
        let addr = format!("{}:{}", server, port);
        debug!("Connecting to {}", addr);
        let stream = TcpStream::connect(&addr).await?;
        debug!("Connected to {}", addr);

        Ok(Self {
            stream: Mutex::new(stream),
            xid: AtomicU32::new(1),
            uid,
            gid,
            server: server.to_string(),
            port,
        })
    }

    /// Get the next transaction ID.
    fn next_xid(&self) -> u32 {
        self.xid.fetch_add(1, Ordering::Relaxed)
    }

    /// Build AUTH_SYS credentials.
    fn auth_sys(&self) -> XdrEncoder {
        let mut cred_body = XdrEncoder::new();
        cred_body.put_u32(0); // stamp
        cred_body.put_string("nextnfstest"); // machine name
        cred_body.put_u32(self.uid); // uid
        cred_body.put_u32(self.gid); // gid
        cred_body.put_u32(0); // aux gids count
        cred_body
    }

    /// Send an RPC call and receive the reply.
    /// Returns the reply body (after RPC header parsing).
    pub async fn call(
        &self,
        program: u32,
        version: u32,
        procedure: u32,
        args: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let xid = self.next_xid();

        // Build RPC call header
        let mut msg = XdrEncoder::new();
        msg.put_u32(xid); // XID
        msg.put_u32(MSG_TYPE_CALL); // message type
        msg.put_u32(RPC_VERSION); // RPC version
        msg.put_u32(program); // program
        msg.put_u32(version); // version
        msg.put_u32(procedure); // procedure

        // Credentials (AUTH_SYS)
        let cred_body = self.auth_sys();
        let cred_bytes = cred_body.finish();
        msg.put_u32(AUTH_SYS); // flavor
        msg.put_opaque(&cred_bytes); // body

        // Verifier (AUTH_NONE)
        msg.put_u32(AUTH_NONE); // flavor
        msg.put_u32(0); // body length (empty)

        // Append procedure arguments
        let msg_bytes = msg.finish();

        // Build TCP record-marked message
        let total_len = msg_bytes.len() + args.len();
        let mut frame = BytesMut::with_capacity(4 + total_len);
        // Last fragment bit (0x80000000) OR'd with length
        frame.put_u32((0x80000000 | total_len as u32) as u32);
        frame.put_slice(&msg_bytes);
        frame.put_slice(args);

        trace!(
            "RPC call: xid={} prog={} ver={} proc={} args_len={}",
            xid, program, version, procedure, args.len()
        );

        // Send
        let mut stream = self.stream.lock().await;
        stream.write_all(&frame).await?;
        stream.flush().await?;

        // Receive reply (TCP record marking)
        let reply = self.read_record(&mut *stream).await?;

        // Parse RPC reply header
        let mut dec = XdrDecoder::new(&reply);
        let reply_xid = dec.get_u32()?;
        if reply_xid != xid {
            anyhow::bail!(
                "XID mismatch: expected {}, got {}",
                xid,
                reply_xid
            );
        }

        let msg_type = dec.get_u32()?;
        if msg_type != MSG_TYPE_REPLY {
            anyhow::bail!("Expected REPLY (1), got {}", msg_type);
        }

        let reply_stat = dec.get_u32()?;
        if reply_stat != REPLY_ACCEPTED {
            let reject_stat = dec.get_u32()?;
            anyhow::bail!("RPC rejected: reply_stat={}, reject_stat={}", reply_stat, reject_stat);
        }

        // Parse verifier in accepted reply
        let _verf_flavor = dec.get_u32()?;
        let verf_body = dec.get_opaque()?;
        let _ = verf_body; // Ignore verifier

        let accept_stat = dec.get_u32()?;
        if accept_stat != ACCEPT_SUCCESS {
            anyhow::bail!("RPC accept error: {}", accept_stat_name(accept_stat));
        }

        // Return remaining bytes as the procedure reply
        let pos = dec.position();
        Ok(reply[pos..].to_vec())
    }

    /// Send a NULL RPC call (procedure 0) to test connectivity.
    pub async fn null_call(&self, program: u32, version: u32) -> anyhow::Result<()> {
        let reply = self.call(program, version, 0, &[]).await?;
        // NULL should return empty body
        Ok(())
    }

    /// Read a complete TCP record-marked message.
    async fn read_record(&self, stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
        let mut result = Vec::new();

        loop {
            // Read fragment header (4 bytes)
            let mut header_buf = [0u8; 4];
            stream.read_exact(&mut header_buf).await?;
            let header = u32::from_be_bytes(header_buf);

            let last = (header & 0x80000000) != 0;
            let frag_len = (header & 0x7FFFFFFF) as usize;

            if frag_len > 16 * 1024 * 1024 {
                anyhow::bail!("Fragment too large: {} bytes", frag_len);
            }

            // Read fragment body
            let mut frag = vec![0u8; frag_len];
            stream.read_exact(&mut frag).await?;
            result.extend_from_slice(&frag);

            if last {
                break;
            }
        }

        trace!("Received record: {} bytes", result.len());
        Ok(result)
    }
}

fn accept_stat_name(stat: u32) -> &'static str {
    match stat {
        0 => "SUCCESS",
        1 => "PROG_UNAVAIL",
        2 => "PROG_MISMATCH",
        3 => "PROC_UNAVAIL",
        4 => "GARBAGE_ARGS",
        5 => "SYSTEM_ERR",
        _ => "UNKNOWN",
    }
}
