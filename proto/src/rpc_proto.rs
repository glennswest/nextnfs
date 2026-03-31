extern crate serde;
extern crate serde_bytes;
extern crate serde_derive;
extern crate serde_xdr;

use serde_derive::{Deserialize, Serialize};

use super::{
    from_bytes,
    nfs4_proto::{Compound4args, Compound4res},
    to_bytes,
};

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct AuthUnix {
    pub stamp: u32,
    pub machinename: String,
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

#[derive(Clone, Debug)]
#[repr(u32)]
pub enum OpaqueAuth {
    AuthNull(Vec<u8>) = 0,
    AuthUnix(AuthUnix) = 1,
    // not supported
    AuthShort = 2,
    AuthDes = 3,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallBody {
    pub rpcvers: u32,
    pub prog: u32,
    pub vers: u32,
    pub proc: u32,
    pub cred: OpaqueAuth,
    pub verf: OpaqueAuth,
    pub args: Option<Compound4args>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[repr(u32)]
pub enum MsgType {
    Call(CallBody) = 0,
    Reply(ReplyBody) = 1,
    /// Parse error — the RPC message could not be decoded.
    /// Contains the error message. XID is preserved in the parent RpcCallMsg
    /// when the header was partially readable, or 0 if not.
    #[serde(skip)]
    ParseError(String) = 99,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedReply {
    pub verf: OpaqueAuth,
    pub reply_data: AcceptBody,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MismatchInfo {
    low: u32,
    high: u32,
}

impl MismatchInfo {
    pub fn new(low: u32, high: u32) -> Self {
        Self { low, high }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u32)]
pub enum ReplyBody {
    MsgAccepted(AcceptedReply) = 0,
    MsgDenied(RejectedReply) = 1,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[repr(u32)]
pub enum AcceptBody {
    Success(Compound4res) = 0,
    ProgUnavail = 1,
    /// remote can't support version #
    ProgMismatch(MismatchInfo) = 2,
    ProcUnavail = 3,
    /// procedure can't decode params
    GarbageArgs = 4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u32)]
pub enum RejectedReply {
    RpcMismatch(MismatchInfo) = 0,
    AuthError(AuthStat) = 1,
}

#[derive(Debug, Clone, Deserialize, Default, Serialize)]
#[repr(u32)]
///   Why authentication failed
pub enum AuthStat {
    UndefAuthCred = 0,
    /// bad credentials (seal broken)
    #[default]
    AuthBadCred = 1,
    /// client must begin new session
    AuthRejectedCred = 2,
    /// bad verifier (seal broken)    
    AuthBadverf = 3,
    /// verifier expired or replayed  
    AuthRejectedverf = 4,
    /// rejected for security reasons
    AuthTooWeak = 5,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcCallMsg {
    pub xid: u32,
    pub body: MsgType,
}

impl RpcCallMsg {
    pub fn from_bytes(buffer: Vec<u8>) -> Result<Self, anyhow::Error> {
        from_bytes(buffer)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcCompoundCallMsg {
    pub xid: u32,
    pub body: MsgType,
}

#[derive(Debug, Serialize)]
pub struct RpcReplyMsg {
    pub xid: u32,
    pub body: MsgType,
}

impl RpcReplyMsg {
    pub fn to_bytes(&self) -> Result<Vec<u8>, anyhow::Error> {
        let result = to_bytes(self);
        match result {
            Ok(bytes) => Ok(bytes),
            Err(e) => Err(anyhow::anyhow!("Error serializing message: {:?}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs4_proto::*;

    #[test]
    fn test_auth_unix_defaults() {
        let auth = AuthUnix::default();
        assert_eq!(auth.stamp, 0);
        assert_eq!(auth.machinename, "");
        assert_eq!(auth.uid, 0);
        assert_eq!(auth.gid, 0);
        assert!(auth.gids.is_empty());
    }

    #[test]
    fn test_auth_unix_roundtrip() {
        let auth = AuthUnix {
            stamp: 12345,
            machinename: "testhost".to_string(),
            uid: 1000,
            gid: 1000,
            gids: vec![1000, 100, 10],
        };
        let bytes = serde_xdr::to_bytes(&auth).unwrap();
        let decoded: AuthUnix = serde_xdr::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.stamp, 12345);
        assert_eq!(decoded.machinename, "testhost");
        assert_eq!(decoded.uid, 1000);
        assert_eq!(decoded.gid, 1000);
        assert_eq!(decoded.gids, vec![1000, 100, 10]);
    }

    #[test]
    fn test_rpc_reply_success_serializes() {
        let reply = RpcReplyMsg {
            xid: 42,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(vec![]),
                reply_data: AcceptBody::Success(Compound4res {
                    status: NfsStat4::Nfs4Ok,
                    tag: "test".to_string(),
                    resarray: vec![],
                }),
            })),
        };
        let bytes = reply.to_bytes();
        assert!(bytes.is_ok());
        assert!(!bytes.unwrap().is_empty());
    }

    #[test]
    fn test_rpc_reply_proc_unavail_serializes() {
        let reply = RpcReplyMsg {
            xid: 99,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(vec![]),
                reply_data: AcceptBody::ProcUnavail,
            })),
        };
        let bytes = reply.to_bytes();
        assert!(bytes.is_ok());
    }

    #[test]
    fn test_rpc_reply_garbage_args_serializes() {
        let reply = RpcReplyMsg {
            xid: 100,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(vec![]),
                reply_data: AcceptBody::GarbageArgs,
            })),
        };
        let bytes = reply.to_bytes();
        assert!(bytes.is_ok());
    }

    #[test]
    fn test_rpc_reply_xid_in_bytes() {
        let reply = RpcReplyMsg {
            xid: 0xDEADBEEF,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(vec![]),
                reply_data: AcceptBody::ProcUnavail,
            })),
        };
        let bytes = reply.to_bytes().unwrap();
        // XID should be the first 4 bytes in big-endian
        assert_eq!(&bytes[0..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_auth_stat_default() {
        let stat = AuthStat::default();
        assert!(matches!(stat, AuthStat::AuthBadCred));
    }

    #[test]
    fn test_call_body_fields() {
        let cb = CallBody {
            rpcvers: 2,
            prog: 100003,
            vers: 4,
            proc: 1,
            cred: OpaqueAuth::AuthNull(vec![]),
            verf: OpaqueAuth::AuthNull(vec![]),
            args: None,
        };
        assert_eq!(cb.rpcvers, 2);
        assert_eq!(cb.prog, 100003);
        assert_eq!(cb.vers, 4);
        assert_eq!(cb.proc, 1);
    }
}
