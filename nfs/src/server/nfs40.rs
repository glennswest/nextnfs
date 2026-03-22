use async_trait::async_trait;

use super::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::{nfs4_proto::*, rpc_proto::*};

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
mod op_set_clientid;
mod op_set_clientid_confirm;
mod op_setattr;
mod op_write;

use super::NfsProtoImpl;
use tracing::{debug, error};

#[derive(Debug, Clone)]
pub struct NFS40Server;

impl NFS40Server {
    async fn put_root_filehandle<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
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

        // Get parent path
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
        Self {}
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

                        // misc not yet supported
                        NfsArgOp::Opnverify(_) => self.operation_not_supported(request),
                        NfsArgOp::Opopenattr(_) => self.operation_not_supported(request),
                        NfsArgOp::OpopenDowngrade(_) => self.operation_not_supported(request),
                        NfsArgOp::OpSecinfo(_) => self.operation_not_supported(request),
                        NfsArgOp::Opverify(_) => self.operation_not_supported(request),
                    };
                    let res = response.result;
                    last_status = response.status;
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
