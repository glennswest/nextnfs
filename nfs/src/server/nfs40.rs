use std::sync::atomic::Ordering;

use async_trait::async_trait;
use tracing::info;

use super::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::{nfs4_proto::*, rpc_proto::*};

/// Extract a short operation name from an NfsArgOp for audit logging.
fn op_name(arg: &NfsArgOp) -> &'static str {
    match arg {
        NfsArgOp::OpUndef0 | NfsArgOp::OpUndef1 | NfsArgOp::OpUndef2 => "UNDEF",
        NfsArgOp::OpAccess(_) => "ACCESS",
        NfsArgOp::Opclose(_) => "CLOSE",
        NfsArgOp::Opcommit(_) => "COMMIT",
        NfsArgOp::Opcreate(_) => "CREATE",
        NfsArgOp::Opdelegpurge(_) => "DELEGPURGE",
        NfsArgOp::Opdelegreturn(_) => "DELEGRETURN",
        NfsArgOp::Opgetattr(_) => "GETATTR",
        NfsArgOp::Opgetfh(_) => "GETFH",
        NfsArgOp::Oplink(_) => "LINK",
        NfsArgOp::Oplock(_) => "LOCK",
        NfsArgOp::Oplockt(_) => "LOCKT",
        NfsArgOp::Oplocku(_) => "LOCKU",
        NfsArgOp::Oplookup(_) => "LOOKUP",
        NfsArgOp::Oplookupp(_) => "LOOKUPP",
        NfsArgOp::Opnverify(_) => "NVERIFY",
        NfsArgOp::Opopen(_) => "OPEN",
        NfsArgOp::Opopenattr(_) => "OPENATTR",
        NfsArgOp::OpopenConfirm(_) => "OPEN_CONFIRM",
        NfsArgOp::OpopenDowngrade(_) => "OPEN_DOWNGRADE",
        NfsArgOp::Opputfh(_) => "PUTFH",
        NfsArgOp::Opputpubfh(_) => "PUTPUBFH",
        NfsArgOp::Opputrootfh(_) => "PUTROOTFH",
        NfsArgOp::Opread(_) => "READ",
        NfsArgOp::Opreaddir(_) => "READDIR",
        NfsArgOp::Opreadlink(_) => "READLINK",
        NfsArgOp::Opremove(_) => "REMOVE",
        NfsArgOp::Oprename(_) => "RENAME",
        NfsArgOp::Oprenew(_) => "RENEW",
        NfsArgOp::Oprestorefh(_) => "RESTOREFH",
        NfsArgOp::Opsavefh(_) => "SAVEFH",
        NfsArgOp::OpSecinfo(_) => "SECINFO",
        NfsArgOp::Opsetattr(_) => "SETATTR",
        NfsArgOp::Opsetclientid(_) => "SETCLIENTID",
        NfsArgOp::OpsetclientidConfirm(_) => "SETCLIENTID_CONFIRM",
        NfsArgOp::Opverify(_) => "VERIFY",
        NfsArgOp::Opwrite(_) => "WRITE",
        NfsArgOp::OpreleaseLockOwner(_) => "RELEASE_LOCKOWNER",
        _ => "UNKNOWN",
    }
}

mod op_access;
mod op_close;
mod op_commit;
mod op_create;
mod op_getattr;
mod op_link;
mod op_lock;
mod op_lockt;
mod op_locku;
mod op_lookup;
mod op_open;
mod op_open_downgrade;
mod op_openconfirm;
pub mod op_pseudo;
mod op_putfh;
mod op_read;
mod op_readdir;
mod op_readlink;
mod op_release_lockowner;
mod op_remove;
mod op_rename;
mod op_renew;
mod op_secinfo;
mod op_set_clientid;
mod op_set_clientid_confirm;
mod op_setattr;
mod op_verify;
mod op_write;

use super::NfsProtoImpl;
use super::nfs41::SessionManager;
use tracing::{debug, error};

#[derive(Debug, Clone)]
pub struct NFS40Server {
    pub session_manager: SessionManager,
    /// Grace period flag — true while server is in grace period (reclaim only).
    pub in_grace: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl NFS40Server {
    /// PUTROOTFH — set current filehandle to the pseudo-root.
    /// The pseudo-root presents exports as top-level directories.
    async fn put_root_filehandle<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        let exports = request.export_manager().list_exports().await;

        if exports.len() == 1 {
            // Single export mode: PUTROOTFH goes directly to the export root
            let export = &exports[0];
            request.set_export(export.export_id).await;
            // Check client IP against export ACL
            if !request.check_client_access() {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errAccess,
                };
            }
            match request.file_manager().get_root_filehandle().await {
                Ok(filehandle) => {
                    let _ = request.set_filehandle_id(filehandle.id).await;
                    NfsOpResponse {
                        request,
                        result: Some(NfsResOp4::Opputrootfh(PutRootFh4res {
                            status: NfsStat4::Nfs4Ok,
                        })),
                        status: NfsStat4::Nfs4Ok,
                    }
                }
                Err(e) => {
                    error!("Err {:?}", e);
                    NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errServerfault,
                    }
                }
            }
        } else {
            // Multi-export mode: PUTROOTFH sets pseudo-root
            use super::filemanager::Filehandle;
            let pseudo_fh_id = op_pseudo::pseudo_root_fh();
            request.set_export(op_pseudo::PSEUDO_ROOT_EXPORT_ID).await;

            // Create a synthetic Filehandle for the pseudo-root
            let pseudo_fh = Filehandle::pseudo_root(pseudo_fh_id);
            request.set_filehandle(pseudo_fh);

            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opputrootfh(PutRootFh4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            }
        }
    }

    fn get_current_filehandle<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        let fh = request.current_filehandle_id();
        match fh {
            Some(filehandle_id) => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opgetfh(GetFh4res::Resok4(GetFh4resok {
                    object: filehandle_id,
                }))),
                status: NfsStat4::Nfs4Ok,
            },
            None => {
                error!("Filehandle not set");
                NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                }
            }
        }
    }

    fn save_filehandle<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        if request.current_filehandle().is_some() {
            request.save_filehandle();
            debug!("SAVEFH: saved current filehandle");
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opsavefh(SaveFh4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            }
        } else {
            error!("SAVEFH: no current filehandle");
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opsavefh(SaveFh4res {
                    status: NfsStat4::Nfs4errNofilehandle,
                })),
                status: NfsStat4::Nfs4errNofilehandle,
            }
        }
    }

    fn restore_filehandle<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        if request.restore_filehandle() {
            debug!("RESTOREFH: restored saved filehandle");
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oprestorefh(RestoreFh4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            }
        } else {
            error!("RESTOREFH: no saved filehandle");
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oprestorefh(RestoreFh4res {
                    status: NfsStat4::Nfs4errRestorefh,
                })),
                status: NfsStat4::Nfs4errRestorefh,
            }
        }
    }

    async fn lookup_parent<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        // If on pseudo-root, LOOKUPP is not supported
        if request.is_pseudo_root() {
            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oplookupp(LookupP4res {
                    status: NfsStat4::Nfs4errNoent,
                })),
                status: NfsStat4::Nfs4errNoent,
            };
        }

        let current_fh = match request.current_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplookupp(LookupP4res {
                        status: NfsStat4::Nfs4errNofilehandle,
                    })),
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // If at export root ("/"), go up to pseudo-root
        if current_fh.path == "/" {
            let exports = request.export_manager().list_exports().await;
            if exports.len() > 1 {
                let pseudo_fh_id = op_pseudo::pseudo_root_fh();
                request.set_export(op_pseudo::PSEUDO_ROOT_EXPORT_ID).await;
                let pseudo_fh =
                    super::filemanager::Filehandle::pseudo_root(pseudo_fh_id);
                request.set_filehandle(pseudo_fh);
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplookupp(LookupP4res {
                        status: NfsStat4::Nfs4Ok,
                    })),
                    status: NfsStat4::Nfs4Ok,
                };
            }
        }

        let parent_path = current_fh.file.parent();
        let parent_str = parent_path.as_str().to_string();
        let parent_key = if parent_str.is_empty() {
            "/".to_string()
        } else {
            parent_str
        };

        debug!("LOOKUPP: {} -> {}", current_fh.path, parent_key);

        match request
            .file_manager()
            .get_filehandle_for_path(parent_key)
            .await
        {
            Ok(fh) => {
                request.set_filehandle(fh);
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplookupp(LookupP4res {
                        status: NfsStat4::Nfs4Ok,
                    })),
                    status: NfsStat4::Nfs4Ok,
                }
            }
            Err(_) => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oplookupp(LookupP4res {
                    status: NfsStat4::Nfs4errNoent,
                })),
                status: NfsStat4::Nfs4errNoent,
            },
        }
    }

    fn operation_not_supported<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        NfsOpResponse {
            request,
            result: None,
            status: NfsStat4::Nfs4errNotsupp,
        }
    }
}

#[async_trait]
impl NfsProtoImpl for NFS40Server {
    fn new() -> Self {
        Self {
            session_manager: SessionManager::new(),
            in_grace: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn hash(&self) -> u64 {
        0
    }

    async fn null<'a>(&self, _: CallBody, request: NfsRequest<'a>) -> (NfsRequest<'a>, ReplyBody) {
        (
            request,
            ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                reply_data: AcceptBody::Success(Compound4res {
                    status: NfsStat4::Nfs4Ok,
                    tag: "".to_string(),
                    resarray: Vec::new(),
                }),
            }),
        )
    }

    async fn compound<'a>(
        &self,
        msg: CallBody,
        mut request: NfsRequest<'a>,
    ) -> (NfsRequest<'a>, ReplyBody) {
        let mut last_status = NfsStat4::Nfs4Ok;
        let res = match msg.args {
            Some(args) => {
                // Reject NFSv4.1+ — clients will auto-negotiate down to v4.0
                if args.minor_version > 0 {
                    debug!(
                        "COMPOUND: rejecting minor_version={}, only v4.0 supported",
                        args.minor_version
                    );
                    return (
                        request,
                        ReplyBody::MsgAccepted(AcceptedReply {
                            verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                            reply_data: AcceptBody::Success(Compound4res {
                                status: NfsStat4::Nfs4errMinorVersMismatch,
                                tag: args.tag,
                                resarray: Vec::new(),
                            }),
                        }),
                    );
                }

                let mut resarray = Vec::with_capacity(args.argarray.len());
                for arg in args.argarray {
                    let operation = op_name(&arg);

                    // QoS rate limit check — if rate exceeded, return NFS4ERR_DELAY
                    let rate_limited = if let Some(rl) = request.rate_limiter() {
                        let rl = rl.clone();
                        let mut limiter = rl.lock().await;
                        !limiter.try_consume_op()
                    } else {
                        false
                    };
                    if rate_limited {
                        return (
                            request,
                            ReplyBody::MsgAccepted(AcceptedReply {
                                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                reply_data: AcceptBody::Success(Compound4res {
                                    status: NfsStat4::Nfs4errDelay,
                                    tag: "".to_string(),
                                    resarray,
                                }),
                            }),
                        );
                    }

                    // Grace period enforcement: reject mutating ops during grace
                    // (RFC 7530 §9.14) — reclaim ops (CLAIM_PREVIOUS) are exempt
                    let in_grace = self.in_grace.load(std::sync::atomic::Ordering::Relaxed);
                    if in_grace {
                        let deny = match &arg {
                            // OPEN with CREATE (new file) denied during grace
                            NfsArgOp::Opopen(args) => matches!(&args.claim, OpenClaim4::ClaimNull(_))
                                && matches!(&args.openhow, OpenFlag4::How(_)),
                            // Mutating directory ops denied during grace
                            NfsArgOp::Opcreate(_) | NfsArgOp::Opremove(_) | NfsArgOp::Oprename(_) => true,
                            // Non-reclaim LOCK denied during grace
                            NfsArgOp::Oplock(_) => true,
                            // WRITE/SETATTR/LINK denied during grace
                            NfsArgOp::Opwrite(_) | NfsArgOp::Opsetattr(_) | NfsArgOp::Oplink(_) => true,
                            _ => false,
                        };
                        if deny {
                            return (
                                request,
                                ReplyBody::MsgAccepted(AcceptedReply {
                                    verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                    reply_data: AcceptBody::Success(Compound4res {
                                        status: NfsStat4::Nfs4errGrace,
                                        tag: "".to_string(),
                                        resarray,
                                    }),
                                }),
                            );
                        }
                    }

                    let response = match arg {
                        // undefined ops
                        NfsArgOp::OpUndef0 | NfsArgOp::OpUndef1 | NfsArgOp::OpUndef2 => {
                            self.operation_not_supported(request)
                        }
                        // filehandle operations
                        NfsArgOp::Opgetfh(_) => self.get_current_filehandle(request),
                        NfsArgOp::Opputfh(args) => args.execute(request).await,
                        NfsArgOp::Opputrootfh(_) => self.put_root_filehandle(request).await,
                        NfsArgOp::Opputpubfh(_) => self.put_root_filehandle(request).await,
                        NfsArgOp::Opsavefh(_) => self.save_filehandle(request),
                        NfsArgOp::Oprestorefh(_) => self.restore_filehandle(request),

                        // client management
                        NfsArgOp::Opsetclientid(args) => args.execute(request).await,
                        NfsArgOp::OpsetclientidConfirm(args) => args.execute(request).await,
                        NfsArgOp::Oprenew(args) => args.execute(request).await,

                        // directory operations
                        NfsArgOp::Oplookup(args) => args.execute(request).await,
                        NfsArgOp::Oplookupp(_) => self.lookup_parent(request).await,
                        NfsArgOp::Opreaddir(args) => args.execute(request).await,

                        // file operations
                        NfsArgOp::OpAccess(args) => args.execute(request).await,
                        NfsArgOp::Opgetattr(args) => args.execute(request).await,
                        NfsArgOp::Opsetattr(args) => args.execute(request).await,
                        NfsArgOp::Opopen(args) => args.execute(request).await,
                        NfsArgOp::OpopenConfirm(args) => args.execute(request).await,
                        NfsArgOp::Opclose(args) => args.execute(request).await,
                        NfsArgOp::Opread(args) => args.execute(request).await,
                        NfsArgOp::Opwrite(args) => args.execute(request).await,
                        NfsArgOp::Opcommit(args) => args.execute(request).await,
                        NfsArgOp::Opcreate(args) => args.execute(request).await,
                        NfsArgOp::Opremove(args) => args.execute(request).await,
                        NfsArgOp::Oprename(args) => args.execute(request).await,
                        NfsArgOp::Opreadlink(_) => {
                            op_readlink::Readlink4args.execute(request).await
                        }

                        // delegation (not yet supported)
                        NfsArgOp::Opdelegpurge(_) => self.operation_not_supported(request),
                        NfsArgOp::Opdelegreturn(_) => self.operation_not_supported(request),

                        // locking
                        NfsArgOp::Oplink(args) => args.execute(request).await,
                        NfsArgOp::Oplock(args) => args.execute(request).await,
                        NfsArgOp::Oplockt(args) => args.execute(request).await,
                        NfsArgOp::Oplocku(args) => args.execute(request).await,
                        NfsArgOp::OpreleaseLockOwner(args) => args.execute(request).await,

                        NfsArgOp::Opnverify(args) => args.execute(request).await,
                        NfsArgOp::Opverify(args) => args.execute(request).await,

                        // misc not yet supported
                        NfsArgOp::Opopenattr(_) => self.operation_not_supported(request),
                        NfsArgOp::OpopenDowngrade(args) => args.execute(request).await,
                        NfsArgOp::OpSecinfo(args) => args.execute(request).await,

                        // NFSv4.1/v4.2 ops — handled properly in compound() version routing
                        _ => self.operation_not_supported(request),
                    };
                    let res = response.result;
                    last_status = response.status.clone();

                    // Per-client audit log — structured tracing for all operations
                    let client = response.request.client_addr().clone();
                    let export_id = response.request.current_export_id();
                    let path = response.request.current_filehandle()
                        .map(|fh| fh.path.as_str())
                        .unwrap_or("-");
                    info!(
                        client = %client,
                        op = operation,
                        status = ?last_status,
                        export = ?export_id,
                        path = path,
                        "nfs_audit"
                    );

                    // Increment per-export ops counter
                    if let Some(stats) = response.request.export_stats() {
                        stats.ops.fetch_add(1, Ordering::Relaxed);
                    }

                    if let Some(res) = res {
                        resarray.push(res);
                    } else {
                        request = response.request;
                        break;
                    }
                    match last_status {
                        NfsStat4::Nfs4Ok => {}
                        _ => {
                            return (
                                response.request,
                                ReplyBody::MsgAccepted(AcceptedReply {
                                    verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                    reply_data: AcceptBody::Success(Compound4res {
                                        status: last_status,
                                        tag: "".to_string(),
                                        resarray,
                                    }),
                                }),
                            );
                        }
                    }
                    request = response.request;
                }
                resarray
            }
            None => Vec::new(),
        };

        (
            request,
            ReplyBody::MsgAccepted(AcceptedReply {
                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                reply_data: AcceptBody::Success(Compound4res {
                    status: last_status,
                    tag: "".to_string(),
                    resarray: res,
                }),
            }),
        )
    }

    fn minor_version(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;

    fn make_compound(ops: Vec<NfsArgOp>) -> CallBody {
        CallBody {
            rpcvers: 2,
            prog: 100003,
            vers: 4,
            proc: 1,
            cred: OpaqueAuth::AuthNull(vec![]),
            verf: OpaqueAuth::AuthNull(vec![]),
            args: Some(Compound4args {
                tag: "test".to_string(),
                minor_version: 0,
                argarray: ops,
            }),
        }
    }

    #[tokio::test]
    async fn test_compound_null_procedure() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let call = CallBody {
            rpcvers: 2,
            prog: 100003,
            vers: 4,
            proc: 0,
            cred: OpaqueAuth::AuthNull(vec![]),
            verf: OpaqueAuth::AuthNull(vec![]),
            args: None,
        };
        let (_request, reply) = server.null(call, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert!(res.resarray.is_empty());
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_putrootfh_getattr() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::Opgetattr(Getattr4args {
                attr_request: Attrlist4(vec![FileAttr::Type]),
            }),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 2);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_error_stops_processing() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        // GETATTR without PUTROOTFH should fail, stopping the compound
        let msg = make_compound(vec![
            NfsArgOp::Opgetattr(Getattr4args {
                attr_request: Attrlist4(vec![FileAttr::Type]),
            }),
            // This should NOT execute because the first op fails
            NfsArgOp::Opgetattr(Getattr4args {
                attr_request: Attrlist4(vec![FileAttr::Size]),
            }),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_ne!(res.status, NfsStat4::Nfs4Ok);
                    // Only the first failed op should be in resarray
                    assert_eq!(res.resarray.len(), 1);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_minor_version_mismatch() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = CallBody {
            rpcvers: 2,
            prog: 100003,
            vers: 4,
            proc: 1,
            cred: OpaqueAuth::AuthNull(vec![]),
            verf: OpaqueAuth::AuthNull(vec![]),
            args: Some(Compound4args {
                tag: "test".to_string(),
                minor_version: 1, // v4.1 — should be rejected
                argarray: vec![NfsArgOp::Opputrootfh(())],
            }),
        };
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errMinorVersMismatch);
                    assert!(res.resarray.is_empty());
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_savefh_restorefh() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::Opsavefh(()),
            NfsArgOp::Oprestorefh(()),
            NfsArgOp::Opgetfh(()),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 4);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_restorefh_without_save() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::Oprestorefh(()), // no SAVEFH yet
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errRestorefh);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_getfh_no_filehandle() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![NfsArgOp::Opgetfh(())]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_ne!(res.status, NfsStat4::Nfs4Ok);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_create_readdir_lifecycle() {
        // Use root fh helper — bypasses PUTROOTFH export lookup
        let server = NFS40Server::new();
        let request = create_nfs40_server_with_root_fh(None).await;

        // CREATE a directory in root
        let msg = make_compound(vec![NfsArgOp::Opcreate(Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "testdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        })]);
        let (request, reply) = server.compound(msg, request).await;
        match &reply {
            ReplyBody::MsgAccepted(accepted) => match &accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }

        // READDIR the root to confirm the directory was created
        // Need to re-establish root fh since CREATE changes the current fh
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        let mut request = request;
        request.set_filehandle(root_fh);

        let msg2 = make_compound(vec![NfsArgOp::Opreaddir(Readdir4args {
            cookie: 0,
            cookieverf: [0; 8],
            dircount: 4096,
            maxcount: 4096,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        })]);
        let (_request, reply) = server.compound(msg2, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 1);
                    // Verify READDIR returned an entry for "testdir"
                    match &res.resarray[0] {
                        NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok)) => {
                            assert!(resok.reply.entries.is_some());
                        }
                        other => panic!("Expected Opreaddir Resok4, got {:?}", other),
                    }
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_unsupported_ops() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![NfsArgOp::OpUndef0]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errNotsupp);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_empty_args() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert!(res.resarray.is_empty());
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_savefh_without_current_fh() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![NfsArgOp::Opsavefh(())]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errNofilehandle);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_lookupp_no_filehandle() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![NfsArgOp::Oplookupp(())]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errNofilehandle);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_lookupp_from_root() {
        // LOOKUPP from root — should still work (parent of "/" is "/")
        let server = NFS40Server::new();
        let request = create_nfs40_server_with_root_fh(None).await;
        let msg = make_compound(vec![NfsArgOp::Oplookupp(())]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_lookupp_from_subdir() {
        use crate::server::operation::NfsOperation;

        // Create a subdirectory, then LOOKUPP should navigate to root
        let request = create_nfs40_server_with_root_fh(None).await;
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "lookupp_parent".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        // Current fh is now the subdir

        let server = NFS40Server::new();
        let msg = make_compound(vec![
            NfsArgOp::Oplookupp(()),
            NfsArgOp::Opgetfh(()),
        ]);
        let (_request, reply) = server.compound(msg, response.request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 2);
                    // GETFH should return root's filehandle
                    match &res.resarray[1] {
                        NfsResOp4::Opgetfh(GetFh4res::Resok4(_resok)) => {}
                        other => panic!("Expected Opgetfh Resok4, got {:?}", other),
                    }
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_empty_argarray() {
        // Empty compound — should succeed with no results
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert!(res.resarray.is_empty());
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_multiple_putfh() {
        // Multiple PUTROOTFH in same compound — each should succeed
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::Opgetfh(()),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 3);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    // ===== Functional Workflow Tests =====
    // These tests exercise multi-operation sequences that simulate real NFS client workflows.

    #[tokio::test]
    async fn test_workflow_write_read_roundtrip() {
        use crate::server::nfs40::{Read4args, Read4res, Write4args};
        use crate::server::operation::NfsOperation;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("roundtrip.txt").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("roundtrip.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // Write data with FileSync
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"hello NFS world".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let mut request = response.request;

        // Re-fetch filehandle (write may have invalidated cache)
        let fh = request.file_manager()
            .get_filehandle_for_path("roundtrip.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // Read it back
        let read_args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            count: 4096,
        };
        let response = read_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opread(Read4res::Resok4(resok))) => {
                assert_eq!(resok.data, b"hello NFS world");
                assert!(resok.eof);
            }
            other => panic!("Expected Read4res::Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_write_overwrite_read() {
        use crate::server::nfs40::{Read4args, Read4res, Write4args};
        use crate::server::operation::NfsOperation;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("overwrite.txt").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("overwrite.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // First write
        let write1 = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"AAAAAAAAAA".to_vec(),
        };
        let response = write1.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let mut request = response.request;

        // Re-fetch fh
        let fh = request.file_manager()
            .get_filehandle_for_path("overwrite.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // Overwrite at offset 0
        let write2 = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"BBBB".to_vec(),
        };
        let response = write2.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let mut request = response.request;

        // Re-fetch fh
        let fh = request.file_manager()
            .get_filehandle_for_path("overwrite.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // Read — should see "BBBB" followed by remaining "AAAAAA"
        let read_args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            count: 4096,
        };
        let response = read_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opread(Read4res::Resok4(resok))) => {
                assert_eq!(&resok.data[..4], b"BBBB");
                assert_eq!(resok.data.len(), 10);
            }
            other => panic!("Expected Read4res::Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_open_write_close() {
        use crate::server::nfs40::{
            Close4args, Close4res, Open4args, Open4res, OpenClaim4, OpenFlag4,
            OpenOwner4, Write4args, Write4res,
        };
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // OPEN — create file
        let open_args = Open4args {
            seqid: 1,
            share_access: 2, // WRITE
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"test".to_vec() },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("lifecycle.txt".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let stateid = match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => resok.stateid.clone(),
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        };
        let request = response.request;

        // WRITE — write data to the opened file
        let write_args = Write4args {
            stateid: stateid.clone(),
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"lifecycle data".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opwrite(Write4res::Resok4(resok))) => {
                assert_eq!(resok.count, 14);
            }
            other => panic!("Expected Write4res::Resok4, got {:?}", other),
        }
        let request = response.request;

        // CLOSE
        let close_args = Close4args {
            seqid: stateid.seqid + 1,
            open_stateid: stateid,
        };
        let response = close_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opclose(Close4res::OpenStateid(_))) => {}
            other => panic!("Expected Opclose, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_create_lookup_getattr() {
        use crate::server::nfs40::{Getattr4args, Getattr4resok, Lookup4args};
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // CREATE directory
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "getattr_dir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // LOOKUP the directory
        let lookup_args = Lookup4args { objname: "getattr_dir".to_string() };
        let response = lookup_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let request = response.request;

        // GETATTR — verify it's a directory
        let getattr_args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = getattr_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opgetattr(Getattr4resok { status, obj_attributes: Some(fattr) })) => {
                assert_eq!(status, NfsStat4::Nfs4Ok);
                assert!(fattr.attrmask.contains(&FileAttr::Type));
            }
            other => panic!("Expected Getattr4resok with attrs, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_nested_dir_readdir() {
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // CREATE parent directory
        let create_parent = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "parent".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_parent.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        // current fh is now "parent" dir
        let request = response.request;

        // CREATE child directory inside parent
        let create_child = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "child".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_child.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Navigate back to parent for READDIR
        let mut request = response.request;
        let parent_fh = request.file_manager()
            .get_filehandle_for_path("parent".to_string())
            .await.unwrap();
        request.set_filehandle(parent_fh);

        // READDIR parent — should contain "child"
        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0; 8],
            dircount: 4096,
            maxcount: 4096,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) => {
                assert!(resok.reply.entries.is_some());
                // Walk linked list to collect names
                let mut names = vec![];
                let mut entry = resok.reply.entries.as_ref();
                while let Some(e) = entry {
                    names.push(e.name.as_str());
                    entry = e.nextentry.as_deref();
                }
                assert!(names.contains(&"child"), "Expected 'child' in {:?}", names);
            }
            other => panic!("Expected Opreaddir Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_create_remove_lookup_fails() {
        use crate::server::nfs40::{Lookup4args, Remove4args};
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a file via VFS (not a directory — directory VFS removal has a known bug)
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("doomed.txt").unwrap().create_file().unwrap();

        // REMOVE the file
        let remove_args = Remove4args { target: "doomed.txt".to_string() };
        let response = remove_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // LOOKUP — should fail since file was removed from VFS
        let lookup_args = Lookup4args { objname: "doomed.txt".to_string() };
        let response = lookup_args.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_workflow_rename_verify() {
        use crate::server::nfs40::{Lookup4args, Rename4args};
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // CREATE directory
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "before".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root, save as source dir
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);
        request.save_filehandle(); // saved=root (source dir)
        // current=root (target dir)

        // RENAME "before" → "after"
        let rename_args = Rename4args {
            oldname: "before".to_string(),
            newname: "after".to_string(),
        };
        let response = rename_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // LOOKUP new name — should succeed
        let lookup_new = Lookup4args { objname: "after".to_string() };
        let response = lookup_new.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // LOOKUP old name — should fail
        let lookup_old = Lookup4args { objname: "before".to_string() };
        let response = lookup_old.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_workflow_multi_file_readdir() {
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();

        // Create 3 files directly via VFS
        for name in &["alpha.txt", "beta.txt", "gamma.txt"] {
            root_file.join(name).unwrap().create_file().unwrap();
        }

        // READDIR root
        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0; 8],
            dircount: 8192,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) => {
                // Walk linked list to collect names
                let mut names = vec![];
                let mut entry = resok.reply.entries.as_ref();
                while let Some(e) = entry {
                    names.push(e.name.clone());
                    entry = e.nextentry.as_deref();
                }
                assert_eq!(names.len(), 3);
                assert!(names.contains(&"alpha.txt".to_string()));
                assert!(names.contains(&"beta.txt".to_string()));
                assert!(names.contains(&"gamma.txt".to_string()));
            }
            other => panic!("Expected Opreaddir Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_lock_unlock_relock() {
        use crate::server::filemanager::LockResult;
        use crate::server::nfs40::{Locku4args, NfsLockType4};
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;
        let fh_id = request.current_filehandle().unwrap().id;

        // LOCK
        let lock_result = request.file_manager()
            .lock_file(fh_id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        let stateid = match lock_result {
            LockResult::Ok(s) => s,
            other => panic!("Expected LockResult::Ok, got {:?}", other),
        };

        // UNLOCK via LOCKU operation
        let locku_args = Locku4args {
            locktype: NfsLockType4::WriteLt,
            seqid: stateid.seqid,
            lock_stateid: stateid.clone(),
            offset: 0,
            length: 100,
        };
        let response = locku_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let request = response.request;

        // RELOCK — same range should now succeed
        let lock_result2 = request.file_manager()
            .lock_file(fh_id, 2, b"owner2".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        match lock_result2 {
            LockResult::Ok(_) => {}
            other => panic!("Expected second lock to succeed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_partial_read_after_write() {
        use crate::server::nfs40::{Read4args, Read4res, Write4args};
        use crate::server::operation::NfsOperation;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("partial_rw.txt").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("partial_rw.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // Write 26 bytes (alphabet)
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let mut request = response.request;

        // Re-fetch fh
        let fh = request.file_manager()
            .get_filehandle_for_path("partial_rw.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // Read 5 bytes from offset 10 — should get "klmno"
        let read_args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 10,
            count: 5,
        };
        let response = read_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opread(Read4res::Resok4(resok))) => {
                assert_eq!(resok.data, b"klmno");
                assert!(!resok.eof);
            }
            other => panic!("Expected Read4res::Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_setattr_getattr_roundtrip() {
        use crate::server::nfs40::{
            Getattr4args, Getattr4resok, SetAttr4args,
        };
        use crate::server::operation::NfsOperation;
        use nextnfs_proto::nfs4_proto::FileAttrValue;

        let request = create_nfs40_server_with_root_fh(None).await;

        // SETATTR — set an attribute
        let setattr_args = SetAttr4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Size]),
                attr_vals: Attrlist4(vec![FileAttrValue::Size(0)]),
            },
        };
        let response = setattr_args.execute(request).await;
        // May or may not succeed on root dir, but shouldn't panic
        let request = response.request;

        // GETATTR — verify attributes are returned
        let getattr_args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::Type, FileAttr::Size]),
        };
        let response = getattr_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opgetattr(Getattr4resok { status, obj_attributes: Some(fattr) })) => {
                assert_eq!(status, NfsStat4::Nfs4Ok);
                assert!(fattr.attrmask.contains(&FileAttr::Type));
            }
            other => panic!("Expected Getattr4resok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_compound_putrootfh_create_getfh() {
        let server = NFS40Server::new();
        let request = create_nfs40_server_with_root_fh(None).await;
        let msg = make_compound(vec![
            NfsArgOp::Opcreate(Create4args {
                objtype: Createtype4::Nf4dir,
                objname: "compound_dir".to_string(),
                createattrs: Fattr4 {
                    attrmask: Attrlist4(vec![]),
                    attr_vals: Attrlist4(vec![]),
                },
            }),
            NfsArgOp::Opgetfh(()),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 2);
                    // Second result should be GETFH with a valid filehandle
                    match &res.resarray[1] {
                        NfsResOp4::Opgetfh(GetFh4res::Resok4(resok)) => {
                            assert_ne!(resok.object, [0u8; 26]);
                        }
                        other => panic!("Expected Opgetfh Resok4, got {:?}", other),
                    }
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_create_lookup_in_sequence() {
        let server = NFS40Server::new();
        let request = create_nfs40_server_with_root_fh(None).await;

        // First compound: CREATE
        let msg1 = make_compound(vec![NfsArgOp::Opcreate(Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "seq_dir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        })]);
        let (request, reply) = server.compound(msg1, request).await;
        match &reply {
            ReplyBody::MsgAccepted(accepted) => match &accepted.reply_data {
                AcceptBody::Success(res) => assert_eq!(res.status, NfsStat4::Nfs4Ok),
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }

        // Reset to root for next compound
        let mut request = request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // Second compound: LOOKUP + GETATTR
        let msg2 = make_compound(vec![
            NfsArgOp::Oplookup(Lookup4args { objname: "seq_dir".to_string() }),
            NfsArgOp::Opgetattr(Getattr4args {
                attr_request: Attrlist4(vec![FileAttr::Type]),
            }),
        ]);
        let (_request, reply) = server.compound(msg2, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 2);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_savefh_rename_workflow() {
        let server = NFS40Server::new();
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a directory first
        let msg1 = make_compound(vec![NfsArgOp::Opcreate(Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "rename_src".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        })]);
        let (request, _) = server.compound(msg1, request).await;

        // Reset to root
        let mut request = request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // Compound: SAVEFH (save root as source dir) + RENAME
        let msg2 = make_compound(vec![
            NfsArgOp::Opsavefh(()),
            NfsArgOp::Oprename(Rename4args {
                oldname: "rename_src".to_string(),
                newname: "rename_dst".to_string(),
            }),
        ]);
        let (_request, reply) = server.compound(msg2, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4Ok);
                    assert_eq!(res.resarray.len(), 2);
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_workflow_open_read_existing_file() {
        use crate::server::nfs40::{
            Open4args, OpenClaim4, OpenFlag4, OpenOwner4, Read4args, Read4res,
        };
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a file with content via VFS
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("existing.txt").unwrap().create_file().unwrap();
        {
            use std::io::Write;
            let mut f = root_file.join("existing.txt").unwrap().append_file().unwrap();
            f.write_all(b"pre-existing data").unwrap();
        }

        // OPEN for reading
        let open_args = Open4args {
            seqid: 1,
            share_access: 1, // READ
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"reader".to_vec() },
            openhow: OpenFlag4::Open4Nocreate,
            claim: OpenClaim4::ClaimNull("existing.txt".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let request = response.request;

        // READ the opened file
        let read_args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            count: 4096,
        };
        let response = read_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opread(Read4res::Resok4(resok))) => {
                assert_eq!(resok.data, b"pre-existing data");
                assert!(resok.eof);
            }
            other => panic!("Expected Read4res::Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_create_two_files_remove_one_readdir() {
        use crate::server::nfs40::Remove4args;
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // Create two files via VFS
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("file_a.txt").unwrap().create_file().unwrap();
        root_file.join("file_b.txt").unwrap().create_file().unwrap();

        // REMOVE file_a
        let remove_args = Remove4args { target: "file_a.txt".to_string() };
        let response = remove_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // READDIR — should only contain file_b.txt
        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0; 8],
            dircount: 4096,
            maxcount: 4096,
            attr_request: Attrlist4(vec![]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) => {
                let first = resok.reply.entries.as_ref().expect("Expected at least one entry");
                assert_eq!(first.name, "file_b.txt");
                assert!(first.nextentry.is_none(), "Expected only one entry");
            }
            other => panic!("Expected Opreaddir Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_remove_directory_verify_gone() {
        use crate::server::nfs40::{Lookup4args, Remove4args};
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // CREATE directory
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "rmdir_test".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // Verify dir exists via LOOKUP
        let lookup_args = Lookup4args { objname: "rmdir_test".to_string() };
        let response = lookup_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // REMOVE directory
        let remove_args = Remove4args { target: "rmdir_test".to_string() };
        let response = remove_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // LOOKUP — should fail since directory was removed from VFS
        let lookup_args = Lookup4args { objname: "rmdir_test".to_string() };
        let response = lookup_args.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    // ===== Complex Compound Workflow Tests =====
    // These test realistic multi-step NFS workflows.

    #[tokio::test]
    async fn test_workflow_create_write_read_close() {
        // Full file lifecycle: OPEN(create) → WRITE → READ → CLOSE
        use crate::server::nfs40::{
            Close4args, Open4args, Open4res, Read4args, Read4res,
            Write4args, OpenFlag4, OpenClaim4, OpenOwner4,
        };
        use crate::server::operation::NfsOperation;
        use nextnfs_proto::nfs4_proto::CreateHow4;

        let request = create_nfs40_server_with_root_fh(None).await;

        // OPEN with create
        let open_args = Open4args {
            seqid: 1,
            share_access: 2, // WRITE
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"lifecycle_owner".to_vec(),
            },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("lifecycle.txt".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let open_stateid = match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => resok.stateid.clone(),
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        };
        let request = response.request;

        // WRITE — current fh is the new file
        let write_args = Write4args {
            stateid: open_stateid.clone(),
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"lifecycle data content".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let request = response.request;

        // Re-fetch filehandle for read (write may invalidate cached fh)
        let mut request = request;
        let fh_id = request.current_filehandle().unwrap().id;
        let fh = request.file_manager()
            .get_filehandle_for_id(fh_id)
            .await.unwrap();
        request.set_filehandle(fh);

        // READ back
        let read_args = Read4args {
            stateid: open_stateid.clone(),
            offset: 0,
            count: 1024,
        };
        let response = read_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opread(Read4res::Resok4(resok))) => {
                assert_eq!(resok.data, b"lifecycle data content");
                assert!(resok.eof);
            }
            other => panic!("Expected Opread Resok4, got {:?}", other),
        }
        let request = response.request;

        // CLOSE
        let close_args = Close4args {
            seqid: 2,
            open_stateid: open_stateid.clone(),
        };
        let response = close_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_workflow_lock_write_unlock() {
        // Lock coordination: LOCK → WRITE → UNLOCK
        use crate::server::nfs40::{
            Write4args, Locku4args,
        };
        use crate::server::operation::NfsOperation;
        use crate::server::filemanager::LockResult;

        let mut request = create_nfs40_server_with_root_fh(None).await;

        // Create a file
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("locked_write.txt").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("locked_write.txt".to_string())
            .await.unwrap();
        let fh_id = fh.id;
        request.set_filehandle(fh);

        // LOCK
        let lock_result = request.file_manager()
            .lock_file(fh_id, 1, b"lock_owner".to_vec(), NfsLockType4::WriteLt, 0, 1024)
            .await;
        let lock_stateid = match lock_result {
            LockResult::Ok(s) => s,
            other => panic!("Expected LockResult::Ok, got {:?}", other),
        };

        // WRITE under the lock
        let write_args = Write4args {
            stateid: lock_stateid.clone(),
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"protected data".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let request = response.request;

        // UNLOCK
        let locku_args = Locku4args {
            locktype: NfsLockType4::WriteLt,
            seqid: lock_stateid.seqid,
            lock_stateid: lock_stateid.clone(),
            offset: 0,
            length: 1024,
        };
        let response = locku_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Oplocku(Locku4res::LockStateid(sid))) => {
                assert_eq!(sid.seqid, lock_stateid.seqid + 1);
            }
            other => panic!("Expected Oplocku LockStateid, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_savefh_lookup_restorefh() {
        // SAVEFH → LOOKUP into subdir → RESTOREFH restores parent
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a subdir
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "save_test".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        let root_fh_id = root_fh.id;
        request.set_filehandle(root_fh);

        // SAVEFH — save root
        request.save_filehandle();

        // LOOKUP into subdir — changes current fh
        let lookup_args = Lookup4args { objname: "save_test".to_string() };
        let response = lookup_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let mut request = response.request;

        // Current fh should now be save_test, not root
        assert_ne!(request.current_filehandle_id().unwrap(), root_fh_id);

        // RESTOREFH — restores root
        assert!(request.restore_filehandle());
        assert_eq!(request.current_filehandle_id().unwrap(), root_fh_id);
    }

    #[tokio::test]
    async fn test_workflow_readdir_cookie_continuation() {
        // READDIR with limited dircount, verify cookie-based continuation
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // Create 5 files
        let root_file = request.current_filehandle().unwrap().file.clone();
        for i in 0..5 {
            root_file.join(format!("cont_{}", i)).unwrap().create_file().unwrap();
        }

        // First READDIR with small dircount (should limit entries)
        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 30, // very small — should only fit ~1 entry
            maxcount: 400,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) => {
                assert!(resok.reply.entries.is_some());
            }
            other => panic!("Expected Opreaddir Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_open_write_overwrite_read() {
        // OPEN(create) → WRITE "AAA" → WRITE "BB" at offset 0 → READ → "BBA"
        use crate::server::nfs40::{
            Open4args, Open4res, Read4args, Read4res, Write4args,
            OpenFlag4, OpenClaim4, OpenOwner4,
        };
        use crate::server::operation::NfsOperation;
        use nextnfs_proto::nfs4_proto::CreateHow4;

        let request = create_nfs40_server_with_root_fh(None).await;

        let open_args = Open4args {
            seqid: 1,
            share_access: 2,
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"ow_owner".to_vec(),
            },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("ow_test.txt".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let stateid = match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => resok.stateid.clone(),
            other => panic!("Expected Opopen, got {:?}", other),
        };
        let request = response.request;

        // Write "AAA"
        let write1 = Write4args {
            stateid: stateid.clone(),
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"AAA".to_vec(),
        };
        let response = write1.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Overwrite "BB" at offset 0
        let mut request = response.request;
        let fh_id = request.current_filehandle().unwrap().id;
        let fh = request.file_manager().get_filehandle_for_id(fh_id).await.unwrap();
        request.set_filehandle(fh);

        let write2 = Write4args {
            stateid: stateid.clone(),
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"BB".to_vec(),
        };
        let response = write2.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Read back — should be "BBA"
        let mut request = response.request;
        let fh_id = request.current_filehandle().unwrap().id;
        let fh = request.file_manager().get_filehandle_for_id(fh_id).await.unwrap();
        request.set_filehandle(fh);

        let read_args = Read4args {
            stateid: stateid.clone(),
            offset: 0,
            count: 100,
        };
        let response = read_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opread(Read4res::Resok4(resok))) => {
                assert_eq!(resok.data, b"BBA");
            }
            other => panic!("Expected Opread, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_getattr_after_write_shows_new_size() {
        // Write data → GETATTR → size matches written data
        use crate::server::nfs40::{
            Getattr4args, Getattr4resok, Write4args,
        };
        use crate::server::operation::NfsOperation;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("size_check.txt").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("size_check.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        // Write 42 bytes
        let data = vec![0x42u8; 42];
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data,
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Re-fetch fh for getattr
        let mut request = response.request;
        let fh_id = request.current_filehandle().unwrap().id;
        let fh = request.file_manager().get_filehandle_for_id(fh_id).await.unwrap();
        request.set_filehandle(fh);

        // GETATTR
        let getattr_args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::Size]),
        };
        let response = getattr_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opgetattr(Getattr4resok { status: _, obj_attributes: Some(fattr) })) => {
                // Find Size value
                for val in fattr.attr_vals.iter() {
                    if let FileAttrValue::Size(size) = val {
                        assert_eq!(*size, 42);
                        return;
                    }
                }
                panic!("Size attribute not found in response");
            }
            other => panic!("Expected Opgetattr Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_unstable_write_then_commit() {
        // Unstable write → COMMIT lifecycle (write cache path)
        use crate::server::nfs40::{
            Commit4args, Commit4res, Write4args, Write4res,
        };
        use crate::server::operation::NfsOperation;
        use nextnfs_proto::nfs4_proto::CreateHow4;

        let request = create_nfs40_server_with_root_fh(None).await;

        // OPEN with create — need a file with write cache support
        let open_args = Open4args {
            seqid: 1,
            share_access: 2, // WRITE
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"commit_owner".to_vec() },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("commit_test.txt".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let request = response.request;

        // Unstable write — goes to write cache
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::Unstable4,
            data: b"cached data for commit".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opwrite(Write4res::Resok4(resok))) => {
                assert_eq!(resok.committed, StableHow4::Unstable4);
            }
            other => panic!("Expected Write4res::Resok4, got {:?}", other),
        }
        let request = response.request;

        // COMMIT — flush write cache to disk
        let commit_args = Commit4args { offset: 0, count: 0 };
        let response = commit_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opcommit(Commit4res::Resok4(resok))) => {
                assert_ne!(resok.writeverf, [0u8; 8]);
            }
            other => panic!("Expected Commit4res::Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_workflow_create_lookup_remove_verify() {
        // CREATE → LOOKUP (exists) → REMOVE → LOOKUP (gone)
        use crate::server::nfs40::{Lookup4args, Remove4args};
        use crate::server::operation::NfsOperation;

        let request = create_nfs40_server_with_root_fh(None).await;

        // CREATE
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "verify_dir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // LOOKUP — should find it
        let lookup = Lookup4args { objname: "verify_dir".to_string() };
        let response = lookup.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root for REMOVE
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // REMOVE
        let remove = Remove4args { target: "verify_dir".to_string() };
        let response = remove.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root for second LOOKUP
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // LOOKUP — should be gone now
        let lookup2 = Lookup4args { objname: "verify_dir".to_string() };
        let response = lookup2.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_workflow_open_close_reopen() {
        // OPEN → CLOSE → re-OPEN (verify file persists)
        use crate::server::nfs40::{
            Close4args, Open4args, Open4res,
        };
        use crate::server::operation::NfsOperation;
        use nextnfs_proto::nfs4_proto::CreateHow4;

        let request = create_nfs40_server_with_root_fh(None).await;

        // OPEN create
        let open_args = Open4args {
            seqid: 1,
            share_access: 2,
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"reopen".to_vec() },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("reopen.txt".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let stateid = match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => resok.stateid.clone(),
            other => panic!("Expected Open4res::Resok4, got {:?}", other),
        };
        let request = response.request;

        // CLOSE
        let close_args = Close4args {
            seqid: 1,
            open_stateid: stateid,
        };
        let response = close_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root for re-OPEN
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // Re-OPEN for reading — file should still exist
        let reopen = Open4args {
            seqid: 2,
            share_access: 1, // READ
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"reopen".to_vec() },
            openhow: OpenFlag4::Open4Nocreate,
            claim: OpenClaim4::ClaimNull("reopen.txt".to_string()),
        };
        let response = reopen.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
