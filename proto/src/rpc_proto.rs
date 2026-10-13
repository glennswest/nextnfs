extern crate serde;
extern crate serde_bytes;
extern crate serde_derive;

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

/// RPCSEC_GSS procedures (RFC 2203 §5).
pub const RPCSEC_GSS_DATA: u32 = 0;
pub const RPCSEC_GSS_INIT: u32 = 1;
pub const RPCSEC_GSS_CONTINUE_INIT: u32 = 2;
pub const RPCSEC_GSS_DESTROY: u32 = 3;
/// RPCSEC_GSS services.
pub const RPC_GSS_SVC_NONE: u32 = 1;
pub const RPC_GSS_SVC_INTEGRITY: u32 = 2;
pub const RPC_GSS_SVC_PRIVACY: u32 = 3;

/// RPCSEC_GSS credential (RFC 2203 §5): `rpc_gss_cred_t`, a union on
/// `version` (only 1 is defined) of `rpc_gss_cred_vers_1_t`.
#[derive(Clone, Debug)]
pub struct RpcSecGssCred {
    /// RPCSEC_GSS version, 1.
    pub version: u32,
    /// GSS procedure: RPCSEC_GSS_DATA(0), INIT(1), CONTINUE_INIT(2), DESTROY(3).
    pub gss_proc: u32,
    /// Sequence number for data integrity.
    pub seq_num: u32,
    /// GSS service: None(1), Integrity(2), Privacy(3).
    pub service: u32,
    /// Context handle (opaque, server-assigned).
    pub handle: Vec<u8>,
}

impl RpcSecGssCred {
    /// Parses the credential body; `None` if it is malformed.
    pub fn parse(body: &[u8]) -> Option<Self> {
        let word = |i: usize| body.get(i..i + 4).map(|b| u32::from_be_bytes(b.try_into().expect("4")));
        let (version, gss_proc, seq_num, service, len) = (word(0)?, word(4)?, word(8)?, word(12)?, word(16)? as usize);
        let handle = body.get(20..20usize.checked_add(len)?)?.to_vec();
        Some(RpcSecGssCred { version, gss_proc, seq_num, service, handle })
    }

    /// The credential body as sent on the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(20 + self.handle.len() + 3);
        for w in [self.version, self.gss_proc, self.seq_num, self.service, self.handle.len() as u32] {
            b.extend_from_slice(&w.to_be_bytes());
        }
        b.extend_from_slice(&self.handle);
        b.resize(b.len() + (4 - self.handle.len() % 4) % 4, 0);
        b
    }
}

/// The raw parts of an RPCSEC_GSS call the server needs before it can
/// decode the arguments: the header the verifier signs (RFC 2203 §5.3.1:
/// from the XID through the credential) and the argument bytes, which
/// are wrapped for the integrity and privacy services.
#[derive(Clone, Debug, Default)]
pub struct GssCallData {
    pub header: Vec<u8>,
    pub args: Vec<u8>,
}

/// RPC authentication credentials (RFC 5531 §8.2).
#[derive(Clone, Debug)]
#[repr(u32)]
pub enum OpaqueAuth {
    AuthNull(Vec<u8>) = 0,
    AuthUnix(AuthUnix) = 1,
    // not supported
    AuthShort = 2,
    AuthDes = 3,
    /// RPCSEC_GSS (RFC 2203) — flavor 6.
    AuthGss(RpcSecGssCred) = 6,
    /// An RPCSEC_GSS verifier (flavor 6 on the wire): the GSS MIC token.
    GssVerf(Vec<u8>) = 0x106,
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
    /// Set for RPCSEC_GSS calls; `args` is then decoded by the server
    /// after the credential and verifier are checked.
    #[serde(skip)]
    pub gss: Option<GssCallData>,
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

#[derive(Debug, Clone, Deserialize)]
#[repr(u32)]
pub enum AcceptBody {
    Success(Compound4res) = 0,
    ProgUnavail = 1,
    /// remote can't support version #
    ProgMismatch(MismatchInfo) = 2,
    ProcUnavail = 3,
    /// procedure can't decode params
    GarbageArgs = 4,
    /// SUCCESS with results already encoded (RPCSEC_GSS INIT results, or
    /// results wrapped for the integrity/privacy services). The bytes
    /// are XDR, so a multiple of 4 long.
    #[serde(skip_deserializing)]
    SuccessRaw(Vec<u8>) = 0x100,
}

/// `accept_stat` and its arm. Hand-written so `SuccessRaw` goes out as
/// SUCCESS followed by its bytes as they are.
impl serde::Serialize for AcceptBody {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeTuple};

        struct Words<'a>(&'a [u8]);
        impl serde::Serialize for Words<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                if self.0.len() % 4 != 0 {
                    return Err(S::Error::custom("raw RPC results are not 4-byte aligned"));
                }
                let mut t = serializer.serialize_tuple(self.0.len() / 4)?;
                for w in self.0.chunks(4) {
                    t.serialize_element(&u32::from_be_bytes(w.try_into().expect("4")))?;
                }
                t.end()
            }
        }

        let mut t = serializer.serialize_tuple(2)?;
        match self {
            AcceptBody::Success(res) => {
                t.serialize_element(&0u32)?;
                t.serialize_element(res)?;
            }
            AcceptBody::SuccessRaw(bytes) => {
                t.serialize_element(&0u32)?;
                t.serialize_element(&Words(bytes))?;
            }
            AcceptBody::ProgUnavail => t.serialize_element(&1u32)?,
            AcceptBody::ProgMismatch(m) => {
                t.serialize_element(&2u32)?;
                t.serialize_element(m)?;
            }
            AcceptBody::ProcUnavail => t.serialize_element(&3u32)?,
            AcceptBody::GarbageArgs => t.serialize_element(&4u32)?,
        }
        t.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u32)]
pub enum RejectedReply {
    RpcMismatch(MismatchInfo) = 0,
    AuthError(AuthStat) = 1,
}

///   Why authentication failed (`auth_stat`, RFC 5531 §9 and RFC 2203 §5.3.3.3)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u32)]
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
    /// RPCSEC_GSS: no credentials for user / unknown context
    RpcsecGssCredProblem = 13,
    /// RPCSEC_GSS: problem with the context (expired, sequence too big)
    RpcsecGssCtxProblem = 14,
}

/// Sent as its number (the derived form would send the variant index).
impl serde::Serialize for AuthStat {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(*self as u32)
    }
}

impl<'de> serde::Deserialize<'de> for AuthStat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = <u32 as serde::Deserialize>::deserialize(deserializer)?;
        Ok(match n {
            0 => AuthStat::UndefAuthCred,
            1 => AuthStat::AuthBadCred,
            2 => AuthStat::AuthRejectedCred,
            3 => AuthStat::AuthBadverf,
            4 => AuthStat::AuthRejectedverf,
            5 => AuthStat::AuthTooWeak,
            13 => AuthStat::RpcsecGssCredProblem,
            14 => AuthStat::RpcsecGssCtxProblem,
            n => return Err(<D::Error as serde::de::Error>::custom(format!("unknown auth_stat {}", n))),
        })
    }
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
        let bytes = crate::xdr::to_bytes(&auth).unwrap();
        let decoded: AuthUnix = crate::xdr::from_bytes(&bytes).unwrap();
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
    fn test_rpc_reply_auth_badcred_wire_format() {
        // MSG_DENIED / AUTH_ERROR / AUTH_BADCRED (RFC 5531 §9), sent for
        // RPCSEC_GSS credentials (#99)
        let reply = RpcReplyMsg {
            xid: 7,
            body: MsgType::Reply(ReplyBody::MsgDenied(RejectedReply::AuthError(
                AuthStat::AuthBadCred,
            ))),
        };
        let bytes = reply.to_bytes().unwrap();
        assert_eq!(
            bytes,
            vec![0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]
        );
    }

    #[test]
    fn test_auth_stat_gss_wire_values() {
        for (stat, code) in [(AuthStat::AuthTooWeak, 5u8), (AuthStat::RpcsecGssCredProblem, 13), (AuthStat::RpcsecGssCtxProblem, 14)] {
            let reply = RpcReplyMsg {
                xid: 1,
                body: MsgType::Reply(ReplyBody::MsgDenied(RejectedReply::AuthError(stat))),
            };
            let bytes = reply.to_bytes().unwrap();
            assert_eq!(&bytes[16..20], &[0, 0, 0, code]);
        }
    }

    #[test]
    fn test_gss_cred_parse_encode() {
        let cred = RpcSecGssCred { version: 1, gss_proc: 0, seq_num: 9, service: 2, handle: vec![1, 2, 3, 4, 5] };
        let body = cred.encode();
        assert_eq!(&body[..20], &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 2, 0, 0, 0, 5]);
        assert_eq!(body.len(), 28);
        let back = RpcSecGssCred::parse(&body).unwrap();
        assert_eq!((back.version, back.seq_num, back.service, back.handle), (1, 9, 2, vec![1, 2, 3, 4, 5]));
        assert!(RpcSecGssCred::parse(&body[..22]).is_none());
        assert!(RpcSecGssCred::parse(&[0, 0, 0, 1]).is_none());
    }

    #[test]
    fn test_success_raw_wire_format() {
        let reply = RpcReplyMsg {
            xid: 2,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::GssVerf(vec![9, 9, 9]),
                reply_data: AcceptBody::SuccessRaw(vec![0xa, 0xb, 0xc, 0xd]),
            })),
        };
        assert_eq!(
            reply.to_bytes().unwrap(),
            vec![0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 3, 9, 9, 9, 0, 0, 0, 0, 0, 0xa, 0xb, 0xc, 0xd]
        );
        let bad = RpcReplyMsg {
            xid: 2,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(vec![]),
                reply_data: AcceptBody::SuccessRaw(vec![1, 2, 3]),
            })),
        };
        assert!(bad.to_bytes().is_err());
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
            gss: None,
        };
        assert_eq!(cb.rpcvers, 2);
        assert_eq!(cb.prog, 100003);
        assert_eq!(cb.vers, 4);
        assert_eq!(cb.proc, 1);
    }

    #[test]
    fn test_rpcsec_gss_cred_fields() {
        let cred = RpcSecGssCred {
            version: 1,
            gss_proc: 0,  // RPCSEC_GSS_DATA
            seq_num: 42,
            service: 1,   // rpc_gss_svc_none
            handle: vec![0xDE, 0xAD],
        };
        assert_eq!(cred.gss_proc, 0);
        assert_eq!(cred.seq_num, 42);
        assert_eq!(cred.service, 1);
        assert_eq!(cred.handle, vec![0xDE, 0xAD]);
    }

    #[test]
    fn test_auth_gss_variant() {
        let auth = OpaqueAuth::AuthGss(RpcSecGssCred {
            version: 1,
            gss_proc: 1,  // RPCSEC_GSS_INIT
            seq_num: 0,
            service: 2,   // rpc_gss_svc_integrity
            handle: vec![],
        });
        assert!(matches!(auth, OpaqueAuth::AuthGss(_)));
    }

    #[test]
    fn test_auth_gss_roundtrip() {
        let auth = OpaqueAuth::AuthGss(RpcSecGssCred {
            version: 1,
            gss_proc: 0,
            seq_num: 99,
            service: 3,  // rpc_gss_svc_privacy
            handle: vec![1, 2, 3, 4, 5, 6, 7, 8],
        });
        let bytes = crate::xdr::to_bytes(&auth).unwrap();
        let decoded: OpaqueAuth = crate::xdr::from_bytes(&bytes).unwrap();
        match decoded {
            OpaqueAuth::AuthGss(cred) => {
                assert_eq!(cred.gss_proc, 0);
                assert_eq!(cred.seq_num, 99);
                assert_eq!(cred.service, 3);
                assert_eq!(cred.handle, vec![1, 2, 3, 4, 5, 6, 7, 8]);
            }
            _ => panic!("Expected AuthGss after roundtrip"),
        }
    }
}
