pub mod nfs4_proto;
pub mod rpc_proto;
pub mod utils;

use bytes::{Buf, BytesMut};
use serde_xdr::{from_reader, to_writer};
use std::io::Cursor;
use tokio_util::codec::{Decoder, Encoder};
// use tracing::trace;

use self::rpc_proto::{RpcCallMsg, RpcReplyMsg};

#[derive(Debug)]
pub struct XDRProtoCodec {}

const MAX: usize = 8 * 1024 * 1024;

impl Default for XDRProtoCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl XDRProtoCodec {
    pub fn new() -> XDRProtoCodec {
        XDRProtoCodec {}
    }
}

impl Decoder for XDRProtoCodec {
    type Item = RpcCallMsg;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut message_data = Vec::new();
        let mut is_last = false;
        while !is_last {
            if src.len() < 4 {
                // Not enough data to read length marker.
                return Ok(None);
            }

            // Read the frame: https://datatracker.ietf.org/doc/html/rfc1057#section-10
            let mut header_bytes = [0u8; 4];
            header_bytes.copy_from_slice(&src[..4]);

            let fragment_header = u32::from_be_bytes(header_bytes) as usize;
            is_last = (fragment_header & (1 << 31)) > 0;
            let length = fragment_header & ((1 << 31) - 1);

            // Check that the length is not too large to avoid a denial of
            // service attack where the server runs out of memory.
            if length > MAX {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Frame of length {} is too large.", length),
                ));
            }

            if src.len() < 4 + length {
                // The full string has not yet arrived.
                src.reserve(4 + length - src.len());
                return Ok(None);
            }
            let fragment = src[4..4 + length].to_vec();
            src.advance(4 + length);

            message_data.extend_from_slice(&fragment[..]);
            // TODO remove due to performance reasons
            // trace!(
            //     length = length,
            //     is_last = is_last,
            //     "Finishing Reading fragment"
            // );
        }

        RpcCallMsg::from_bytes(message_data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            .map(Some)
    }

    /// Handle EOF on the stream. If we have a complete message, decode it.
    /// If there are leftover bytes that don't form a complete message,
    /// silently discard them (client disconnected mid-stream).
    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match self.decode(buf)? {
            Some(frame) => Ok(Some(frame)),
            None => {
                // Client disconnected — any leftover bytes are from an
                // incomplete record. This is normal for NFS client teardown.
                if !buf.is_empty() {
                    tracing::debug!(
                        remaining = buf.len(),
                        "client disconnected with partial record, discarding"
                    );
                    buf.clear();
                }
                Ok(None)
            }
        }
    }
}

impl Encoder<Box<RpcReplyMsg>> for XDRProtoCodec {
    type Error = std::io::Error;

    fn encode(&mut self, message: Box<RpcReplyMsg>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let buffer_message = message
            .to_bytes()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let buffer_header = u32::to_be_bytes(buffer_message.len() as u32 + (1 << 31));
        // Reserve space in the buffer.
        dst.reserve(4 + buffer_message.len());

        // Write the length and string to the buffer.
        dst.extend_from_slice(&buffer_header);
        dst.extend_from_slice(&buffer_message);
        Ok(())
    }
}

/// Parse an RPC call message from raw bytes.
///
/// Manually parses the RPC header (RFC 5531) including opaque_auth
/// credentials and verifier, then uses serde_xdr only for the NFSv4
/// COMPOUND args. This avoids mismatches between serde_xdr's enum
/// deserialization and the RFC 5531 opaque_auth wire format.
pub fn from_bytes(buffer: Vec<u8>) -> Result<RpcCallMsg, anyhow::Error> {
    use rpc_proto::{AuthUnix, CallBody, MsgType, OpaqueAuth};
    use std::io::Read;

    let mut cursor = Cursor::new(&buffer);

    // Helper: read a big-endian u32
    let read_u32 = |c: &mut Cursor<&Vec<u8>>| -> anyhow::Result<u32> {
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf)?;
        Ok(u32::from_be_bytes(buf))
    };

    // Helper: read XDR opaque<> (variable-length: u32 length + data + padding)
    let read_opaque = |c: &mut Cursor<&Vec<u8>>| -> anyhow::Result<Vec<u8>> {
        let len = {
            let mut buf = [0u8; 4];
            c.read_exact(&mut buf)?;
            u32::from_be_bytes(buf) as usize
        };
        let mut data = vec![0u8; len];
        c.read_exact(&mut data)?;
        // XDR pads to 4-byte boundary
        let pad = (4 - (len % 4)) % 4;
        if pad > 0 {
            let mut skip = vec![0u8; pad];
            c.read_exact(&mut skip)?;
        }
        Ok(data)
    };

    // Helper: parse opaque_auth (RFC 5531 §8.2)
    let read_opaque_auth = |c: &mut Cursor<&Vec<u8>>| -> anyhow::Result<OpaqueAuth> {
        let flavor = {
            let mut buf = [0u8; 4];
            c.read_exact(&mut buf)?;
            u32::from_be_bytes(buf)
        };
        let body = read_opaque(c)?;

        match flavor {
            0 => Ok(OpaqueAuth::AuthNull(body)),
            1 => {
                // AUTH_SYS: parse body as authsys_parms via serde_xdr
                let mut body_cursor = Cursor::new(body);
                let auth: AuthUnix = from_reader(&mut body_cursor)?;
                Ok(OpaqueAuth::AuthUnix(auth))
            }
            _ => Ok(OpaqueAuth::AuthNull(body)), // unsupported → treat as opaque
        }
    };

    // Parse RPC call header
    let xid = read_u32(&mut cursor)?;
    let msg_type = read_u32(&mut cursor)?;
    if msg_type != 0 {
        anyhow::bail!("expected CALL (0), got msg_type={}", msg_type);
    }

    let rpcvers = read_u32(&mut cursor)?;
    let prog = read_u32(&mut cursor)?;
    let vers = read_u32(&mut cursor)?;
    let proc_num = read_u32(&mut cursor)?;
    let cred = read_opaque_auth(&mut cursor)?;
    let verf = read_opaque_auth(&mut cursor)?;

    // Parse COMPOUND args (if not NULL procedure)
    let args = if proc_num == 0 {
        None
    } else {
        // Remaining bytes are the COMPOUND args — deserialize via serde_xdr
        let pos = cursor.position() as usize;
        let remaining = &buffer[pos..];
        let mut args_cursor = Cursor::new(remaining.to_vec());
        let compound: nfs4_proto::Compound4args = from_reader(&mut args_cursor)?;
        Some(compound)
    };

    Ok(RpcCallMsg {
        xid,
        body: MsgType::Call(CallBody {
            rpcvers,
            prog,
            vers,
            proc: proc_num,
            cred,
            verf,
            args,
        }),
    })
}

pub fn to_bytes(message: &RpcReplyMsg) -> Result<Vec<u8>, anyhow::Error> {
    let mut bytes = Vec::new();
    let result = to_writer(&mut bytes, message);
    // todo add proper logging
    match result {
        Ok(()) => Ok(bytes),
        Err(e) => Err(anyhow::anyhow!("Error serializing message: {:?}", e)),
    }
}
