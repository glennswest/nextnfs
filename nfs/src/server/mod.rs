pub mod clientmanager;
pub mod export_manager;
pub mod filemanager;
pub mod nfs40;
pub mod nfs41;
pub mod nfs42;
pub mod operation;
pub mod request;
pub mod response;
pub mod state_recovery;

use async_trait::async_trait;

use request::NfsRequest;
use tracing::debug;

use nextnfs_proto::rpc_proto::{
    AcceptBody, AcceptedReply, CallBody, MismatchInfo, MsgType, OpaqueAuth, ReplyBody, RpcCallMsg,
    RpcReplyMsg,
};

#[async_trait]
pub trait NfsProtoImpl: Sync {
    fn minor_version(&self) -> u32;

    fn new() -> Self;

    fn hash(&self) -> u64;

    async fn null<'a>(
        &self,
        _: CallBody,
        mut request: NfsRequest<'a>,
    ) -> (NfsRequest<'a>, ReplyBody);

    async fn compound<'a>(
        &self,
        msg: CallBody,
        mut request: NfsRequest<'a>,
    ) -> (NfsRequest<'a>, ReplyBody);
}

#[derive(Debug, Clone)]
pub struct NFSService<Proto> {
    server: Proto,
}

impl<Proto> NFSService<Proto>
where
    Proto: NfsProtoImpl,
{
    pub fn new(protocol: Proto) -> Self {
        NFSService { server: protocol }
    }

    pub async fn call(
        &self,
        rpc_call_message: RpcCallMsg,
        request: NfsRequest<'_>,
    ) -> Box<RpcReplyMsg> {
        debug!("{:?}", rpc_call_message);

        match rpc_call_message.body {
            MsgType::Call(call_body) => {
                // Validate RPC program number — only NFS (100003) is supported.
                // Respond with ProgUnavail for unknown programs (e.g. nfslocalio 400122).
                if call_body.prog != 100003 {
                    debug!("unknown RPC program {} (proc {}), returning ProgUnavail", call_body.prog, call_body.proc);
                    request.close().await;
                    return Box::new(RpcReplyMsg {
                        xid: rpc_call_message.xid,
                        body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                            verf: OpaqueAuth::AuthNull(Vec::new()),
                            reply_data: AcceptBody::ProgUnavail,
                        })),
                    });
                }
                // Validate NFS version — only NFSv4 (version 4) is supported.
                if call_body.vers != 4 {
                    debug!("unsupported NFS version {} (prog {}), returning ProgMismatch", call_body.vers, call_body.prog);
                    request.close().await;
                    return Box::new(RpcReplyMsg {
                        xid: rpc_call_message.xid,
                        body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                            verf: OpaqueAuth::AuthNull(Vec::new()),
                            reply_data: AcceptBody::ProgMismatch(MismatchInfo::new(4, 4)),
                        })),
                    });
                }

                let (request, body) = match call_body.proc {
                    0 => self.server.null(call_body, request).await,
                    1 => self.server.compound(call_body, request).await,
                    _ => {
                        debug!("unknown RPC procedure {}", call_body.proc);
                        request.close().await;
                        return Box::new(RpcReplyMsg {
                            xid: rpc_call_message.xid,
                            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                                verf: OpaqueAuth::AuthNull(Vec::new()),
                                reply_data: AcceptBody::ProcUnavail,
                            })),
                        });
                    }
                };

                // end request
                request.close().await;
                let rpc_reply_message = RpcReplyMsg {
                    xid: rpc_call_message.xid,
                    body: MsgType::Reply(body),
                };
                debug!("{:?}", rpc_reply_message);
                Box::new(rpc_reply_message)
            }
            _ => {
                debug!("received non-Call RPC message, returning GarbageArgs");
                Box::new(RpcReplyMsg {
                    xid: rpc_call_message.xid,
                    body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                        verf: OpaqueAuth::AuthNull(Vec::new()),
                        reply_data: AcceptBody::GarbageArgs,
                    })),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;
    use nfs40::NFS40Server;
    use nextnfs_proto::nfs4_proto::Compound4args;

    fn make_rpc_call(proc: u32) -> RpcCallMsg {
        RpcCallMsg {
            xid: 42,
            body: MsgType::Call(CallBody {
                rpcvers: 2,
                prog: 100003,
                vers: 4,
                proc,
                cred: OpaqueAuth::AuthNull(vec![]),
                verf: OpaqueAuth::AuthNull(vec![]),
                args: Some(Compound4args {
                    tag: "test".to_string(),
                    minor_version: 0,
                    argarray: vec![],
                }),
            }),
        }
    }

    #[tokio::test]
    async fn test_rpc_null_procedure() {
        let service = NFSService::new(NFS40Server::new());
        let request = create_nfs40_server(None).await;
        let msg = make_rpc_call(0);
        let reply = service.call(msg, request).await;
        assert_eq!(reply.xid, 42);
        match reply.body {
            MsgType::Reply(ReplyBody::MsgAccepted(accepted)) => {
                match accepted.reply_data {
                    AcceptBody::Success(_) => {} // NULL returns success
                    _ => panic!("Expected Success for NULL"),
                }
            }
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_rpc_compound_procedure() {
        let service = NFSService::new(NFS40Server::new());
        let request = create_nfs40_server(None).await;
        let msg = make_rpc_call(1);
        let reply = service.call(msg, request).await;
        assert_eq!(reply.xid, 42);
        match reply.body {
            MsgType::Reply(ReplyBody::MsgAccepted(accepted)) => {
                match accepted.reply_data {
                    AcceptBody::Success(_) => {}
                    _ => panic!("Expected Success for COMPOUND"),
                }
            }
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_rpc_proc_unavail() {
        let service = NFSService::new(NFS40Server::new());
        let request = create_nfs40_server(None).await;
        let msg = make_rpc_call(99);
        let reply = service.call(msg, request).await;
        assert_eq!(reply.xid, 42);
        match reply.body {
            MsgType::Reply(ReplyBody::MsgAccepted(accepted)) => {
                assert!(matches!(accepted.reply_data, AcceptBody::ProcUnavail));
            }
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_rpc_garbage_args_non_call() {
        let service = NFSService::new(NFS40Server::new());
        let request = create_nfs40_server(None).await;
        let msg = RpcCallMsg {
            xid: 99,
            body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(vec![]),
                reply_data: AcceptBody::GarbageArgs,
            })),
        };
        let reply = service.call(msg, request).await;
        assert_eq!(reply.xid, 99);
        match reply.body {
            MsgType::Reply(ReplyBody::MsgAccepted(accepted)) => {
                assert!(matches!(accepted.reply_data, AcceptBody::GarbageArgs));
            }
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_rpc_prog_unavail_unknown_program() {
        let service = NFSService::new(NFS40Server::new());
        let request = create_nfs40_server(None).await;
        // nfslocalio program 400122
        let msg = RpcCallMsg {
            xid: 55,
            body: MsgType::Call(CallBody {
                rpcvers: 2,
                prog: 400122,
                vers: 1,
                proc: 1,
                cred: OpaqueAuth::AuthNull(vec![]),
                verf: OpaqueAuth::AuthNull(vec![]),
                args: None,
            }),
        };
        let reply = service.call(msg, request).await;
        assert_eq!(reply.xid, 55);
        match reply.body {
            MsgType::Reply(ReplyBody::MsgAccepted(accepted)) => {
                assert!(matches!(accepted.reply_data, AcceptBody::ProgUnavail));
            }
            _ => panic!("Expected MsgAccepted with ProgUnavail"),
        }
    }

    #[tokio::test]
    async fn test_rpc_prog_mismatch_wrong_version() {
        let service = NFSService::new(NFS40Server::new());
        let request = create_nfs40_server(None).await;
        // NFS program but wrong version (v3)
        let msg = RpcCallMsg {
            xid: 56,
            body: MsgType::Call(CallBody {
                rpcvers: 2,
                prog: 100003,
                vers: 3,
                proc: 1,
                cred: OpaqueAuth::AuthNull(vec![]),
                verf: OpaqueAuth::AuthNull(vec![]),
                args: None,
            }),
        };
        let reply = service.call(msg, request).await;
        assert_eq!(reply.xid, 56);
        match reply.body {
            MsgType::Reply(ReplyBody::MsgAccepted(accepted)) => {
                assert!(matches!(accepted.reply_data, AcceptBody::ProgMismatch(_)));
            }
            _ => panic!("Expected MsgAccepted with ProgMismatch"),
        }
    }

    #[tokio::test]
    async fn test_rpc_xid_preserved() {
        let service = NFSService::new(NFS40Server::new());
        for xid in [0, 1, 0xDEADBEEF, u32::MAX] {
            let request = create_nfs40_server(None).await;
            let mut msg = make_rpc_call(0);
            msg.xid = xid;
            let reply = service.call(msg, request).await;
            assert_eq!(reply.xid, xid);
        }
    }
}
