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
        }

        tracing::debug!(
            total_len = message_data.len(),
            is_last = is_last,
            "RPC record assembled"
        );
        // Never return Err from decode — tokio-util's Framed terminates the
        // stream after the first error (sets has_errored=true). Instead, wrap
        // parse failures in MsgType::ParseError so the server can send
        // GarbageArgs while keeping the TCP connection alive.
        match RpcCallMsg::from_bytes(message_data) {
            Ok(msg) => Ok(Some(msg)),
            Err(e) => {
                tracing::warn!("RPC parse error (returning ParseError): {}", e);
                Ok(Some(RpcCallMsg {
                    xid: 0,
                    body: rpc_proto::MsgType::ParseError(e.to_string()),
                }))
            }
        }
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
            6 => {
                // RPCSEC_GSS (RFC 2203 §5.2.2): parse credential body
                if body.len() >= 12 {
                    let mut pos = 0usize;
                    let gss_proc = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    let seq_num = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    let service = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    let handle = if pos + 4 <= body.len() {
                        let handle_len = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
                        pos += 4;
                        if pos + handle_len <= body.len() {
                            body[pos..pos + handle_len].to_vec()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    };
                    Ok(OpaqueAuth::AuthGss(rpc_proto::RpcSecGssCred {
                        gss_proc,
                        seq_num,
                        service,
                        handle,
                    }))
                } else {
                    Ok(OpaqueAuth::AuthNull(Vec::new()))
                }
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

    // Parse COMPOUND args only for NFS program (100003) non-NULL procedures.
    // Other programs (e.g. nfslocalio 400122) have different arg formats.
    let args = if proc_num == 0 || prog != 100003 {
        None
    } else {
        // Remaining bytes are the COMPOUND args — deserialize via serde_xdr
        let pos = cursor.position() as usize;
        let remaining = &buffer[pos..];
        let mut args_cursor = Cursor::new(remaining.to_vec());
        match from_reader::<_, nfs4_proto::Compound4args>(&mut args_cursor) {
            Ok(compound) => Some(compound),
            Err(e) => {
                let hex_preview: String = remaining.iter().take(64)
                    .map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                tracing::warn!(
                    proc = proc_num,
                    prog = prog,
                    vers = vers,
                    remaining_len = remaining.len(),
                    hex = %hex_preview,
                    "compound deserialization failed: {}", e
                );
                // Return ParseError with preserved XID so server can send
                // GarbageArgs with the correct XID and keep connection alive
                return Ok(RpcCallMsg {
                    xid,
                    body: MsgType::ParseError(format!(
                        "prog={} vers={} proc={}: {}", prog, vers, proc_num, e
                    )),
                });
            }
        }
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

#[cfg(test)]
mod codec_tests {
    use super::*;
    use bytes::BytesMut;
    use tokio_util::codec::{Decoder, Encoder};

    fn make_codec() -> XDRProtoCodec {
        XDRProtoCodec::new()
    }

    #[test]
    fn test_codec_default() {
        let codec = XDRProtoCodec::default();
        // Just ensure it creates without panic
        let _ = format!("{:?}", codec);
    }

    #[test]
    fn test_decode_empty_buffer() {
        let mut codec = make_codec();
        let mut buf = BytesMut::new();
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_decode_incomplete_header() {
        let mut codec = make_codec();
        let mut buf = BytesMut::from(&[0x80, 0x00][..]);
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_decode_incomplete_fragment() {
        let mut codec = make_codec();
        // Header says 100 bytes, but we only provide 10
        let header = (100u32 | (1 << 31)).to_be_bytes();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&[0u8; 10]); // only 10 of 100 bytes
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_decode_oversized_frame_rejected() {
        let mut codec = make_codec();
        // MAX is 8MB — try 9MB
        let length = 9 * 1024 * 1024;
        let header = (length as u32 | (1 << 31)).to_be_bytes();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&header);
        let result = codec.decode(&mut buf);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_decode_eof_empty_buffer() {
        let mut codec = make_codec();
        let mut buf = BytesMut::new();
        let result = codec.decode_eof(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_decode_eof_partial_record_discarded() {
        let mut codec = make_codec();
        let mut buf = BytesMut::from(&[0x80, 0x00, 0x00, 0x10, 0x01, 0x02][..]);
        // Header says 16 bytes, but only 2 bytes of data — incomplete
        let result = codec.decode_eof(&mut buf).unwrap();
        assert!(result.is_none());
        // Buffer should be cleared
        assert!(buf.is_empty());
    }

    #[test]
    fn test_encode_reply() {
        use rpc_proto::{AcceptBody, AcceptedReply, MsgType, OpaqueAuth, ReplyBody, RpcReplyMsg};
        use nfs4_proto::{Compound4res, NfsStat4};

        let mut codec = make_codec();
        let reply = Box::new(RpcReplyMsg {
            xid: 42,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(vec![]),
                reply_data: AcceptBody::Success(Compound4res {
                    status: NfsStat4::Nfs4Ok,
                    tag: "".to_string(),
                    resarray: vec![],
                }),
            })),
        });
        let mut buf = BytesMut::new();
        codec.encode(reply, &mut buf).unwrap();

        // Buffer should have 4-byte header + serialized payload
        assert!(buf.len() > 4);
        // First 4 bytes: header with last-fragment bit set
        let header = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert!(header & (1 << 31) != 0);
        let payload_len = (header & ((1 << 31) - 1)) as usize;
        assert_eq!(payload_len, buf.len() - 4);
    }

    #[test]
    fn test_decode_multi_fragment() {
        // Build a valid RPC NULL call split into two fragments
        // Build RPC CALL: xid=1, msg_type=0(CALL), rpcvers=2, prog=100003, vers=4, proc=0
        // cred = AUTH_NULL, verf = AUTH_NULL
        let mut rpc_body = Vec::new();
        rpc_body.extend_from_slice(&1u32.to_be_bytes()); // xid
        rpc_body.extend_from_slice(&0u32.to_be_bytes()); // CALL
        rpc_body.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
        rpc_body.extend_from_slice(&100003u32.to_be_bytes()); // prog
        rpc_body.extend_from_slice(&4u32.to_be_bytes()); // vers
        rpc_body.extend_from_slice(&0u32.to_be_bytes()); // proc 0 (NULL)
        // AUTH_NULL cred: flavor=0, len=0
        rpc_body.extend_from_slice(&0u32.to_be_bytes());
        rpc_body.extend_from_slice(&0u32.to_be_bytes());
        // AUTH_NULL verf: flavor=0, len=0
        rpc_body.extend_from_slice(&0u32.to_be_bytes());
        rpc_body.extend_from_slice(&0u32.to_be_bytes());

        // Split into 2 fragments: first 20 bytes, then the rest
        let split_at = 20;
        let frag1 = &rpc_body[..split_at];
        let frag2 = &rpc_body[split_at..];

        let mut buf = BytesMut::new();
        // Fragment 1: NOT last (bit 31 = 0)
        let header1 = (frag1.len() as u32).to_be_bytes();
        buf.extend_from_slice(&header1);
        buf.extend_from_slice(frag1);
        // Fragment 2: last (bit 31 = 1)
        let header2 = (frag2.len() as u32 | (1 << 31)).to_be_bytes();
        buf.extend_from_slice(&header2);
        buf.extend_from_slice(frag2);

        let mut codec = make_codec();
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert_eq!(msg.xid, 1);
    }

    #[test]
    fn test_from_bytes_invalid_msg_type() {
        // msg_type = 1 (REPLY, not CALL) should fail
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // xid
        data.extend_from_slice(&1u32.to_be_bytes()); // msg_type = REPLY (not CALL)
        let result = from_bytes(data);
        assert!(result.is_err());
    }

    #[test]
    fn test_from_bytes_truncated_header() {
        // Only 8 bytes — not enough for full RPC header
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // xid
        data.extend_from_slice(&0u32.to_be_bytes()); // CALL
        // Missing rpcvers, prog, vers, proc, cred, verf
        let result = from_bytes(data);
        assert!(result.is_err());
    }

    #[test]
    fn test_from_bytes_null_procedure() {
        // Valid NULL call (proc=0) — args should be None
        let mut data = Vec::new();
        data.extend_from_slice(&42u32.to_be_bytes()); // xid
        data.extend_from_slice(&0u32.to_be_bytes()); // CALL
        data.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
        data.extend_from_slice(&100003u32.to_be_bytes()); // prog
        data.extend_from_slice(&4u32.to_be_bytes()); // vers
        data.extend_from_slice(&0u32.to_be_bytes()); // proc 0
        // AUTH_NULL cred
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        // AUTH_NULL verf
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());

        let result = from_bytes(data).unwrap();
        assert_eq!(result.xid, 42);
        match result.body {
            rpc_proto::MsgType::Call(body) => {
                assert_eq!(body.proc, 0);
                assert!(body.args.is_none());
            }
            _ => panic!("Expected Call"),
        }
    }

    #[test]
    fn test_from_bytes_unsupported_auth_flavor() {
        // Auth flavor 99 should be treated as AuthNull with opaque body
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // xid
        data.extend_from_slice(&0u32.to_be_bytes()); // CALL
        data.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
        data.extend_from_slice(&100003u32.to_be_bytes());
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes()); // proc 0
        // Unknown auth cred: flavor=99, len=4, body=[1,2,3,4]
        data.extend_from_slice(&99u32.to_be_bytes());
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&[1, 2, 3, 4]);
        // AUTH_NULL verf
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());

        let result = from_bytes(data).unwrap();
        match result.body {
            rpc_proto::MsgType::Call(body) => {
                // Unknown flavor treated as AuthNull with opaque body preserved
                match &body.cred {
                    rpc_proto::OpaqueAuth::AuthNull(body) => {
                        assert_eq!(body, &[1, 2, 3, 4]);
                    }
                    other => panic!("Expected AuthNull fallback, got {:?}", other),
                }
            }
            _ => panic!("Expected Call"),
        }
    }
}
