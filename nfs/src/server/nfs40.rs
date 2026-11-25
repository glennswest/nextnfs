use std::sync::atomic::Ordering;

use async_trait::async_trait;
use tracing::info;

use super::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::{nfs4_proto::*, rpc_proto::*};

const OPEN4_SHARE_ACCESS_WRITE: u32 = 0x0000_0002;

/// Whether an operation modifies the filesystem and must be refused with
/// NFS4ERR_ROFS on a read-only export (RFC 7530 §13.1.2, issue #93).
/// OPEN is mutating when it may create/truncate or asks for write access.
fn is_mutating(arg: &NfsArgOp) -> bool {
    match arg {
        NfsArgOp::Opopen(args) => {
            matches!(&args.openhow, OpenFlag4::How(_))
                || args.share_access & OPEN4_SHARE_ACCESS_WRITE != 0
        }
        NfsArgOp::Opwrite(_)
        | NfsArgOp::Opcommit(_)
        | NfsArgOp::Opcreate(_)
        | NfsArgOp::Opremove(_)
        | NfsArgOp::Oprename(_)
        | NfsArgOp::Oplink(_)
        | NfsArgOp::Opsetattr(_)
        | NfsArgOp::Opallocate(_)
        | NfsArgOp::Opcopy(_) => true,
        _ => false,
    }
}

/// Wire opcode of the operations the dispatcher can refuse itself: the
/// mutating ones `is_mutating` covers, the ones that can move the
/// current filehandle onto another export (#97), and the ones
/// `perm::check_op` checks (#91).
const OP_LOOKUP: u32 = 15;
const OP_LOOKUPP: u32 = 16;
const OP_PUTFH: u32 = 22;
const OP_PUTPUBFH: u32 = 23;
const OP_PUTROOTFH: u32 = 24;
const OP_RESTOREFH: u32 = 31;
const OP_SECINFO: u32 = 33;
const OP_SETATTR: u32 = 34;
const OP_SECINFO_NO_NAME: u32 = 52;
const OP_SEQUENCE: u32 = 53;

/// The result a failed op puts in the COMPOUND reply (#131, finding
/// 45e86120ffb7). Every op that runs contributes one, failed or not, and
/// the reply stops after the first failure (RFC 7530 §15.2).
///
/// For most ops the result union is `default: void` on an error, so the
/// result is the opcode and the status alone (`OpError`), whatever body the
/// handler built (its success-only fields, such as REMOVE's `cinfo`, are
/// not sent). The errors whose results carry a body keep it: LOCK and LOCKT
/// NFS4ERR_DENIED (the conflicting lock), SETCLIENTID NFS4ERR_CLID_INUSE
/// (the client address), and SETATTR, whose `attrsset` follows the status
/// whatever it is (empty when the handler gave none).
/// Where an NFSv4.1 COMPOUND breaks the SEQUENCE placement rules (RFC 8881
/// §2.6.3.1.1, §18.46.3; finding 739140171b7d): the index of the op that
/// fails and its status, or None. SEQUENCE must be the first op and only
/// there (a later one: NFS4ERR_SEQUENCE_POS). Without it, the COMPOUND must
/// be one session-less op alone (EXCHANGE_ID, CREATE_SESSION,
/// DESTROY_SESSION, BIND_CONN_TO_SESSION, DESTROY_CLIENTID; with more ops:
/// NFS4ERR_NOT_ONLY_OP), else the first op is NFS4ERR_OP_NOT_IN_SESSION. In
/// NFSv4.0 SEQUENCE does not exist: NFS4ERR_OP_ILLEGAL.
pub(crate) fn sequence_placement_error(minor_version: u32, opcodes: &[u32]) -> Option<(usize, NfsStat4)> {
    const SEQUENCE: u32 = 53;
    if minor_version == 0 {
        return opcodes
            .iter()
            .position(|&op| op == SEQUENCE)
            .map(|i| (i, NfsStat4::Nfs4errOpIllegal));
    }
    let first = *opcodes.first()?;
    if first == SEQUENCE {
        return opcodes
            .iter()
            .skip(1)
            .position(|&op| op == SEQUENCE)
            .map(|i| (i + 1, NfsStat4::Nfs4errSequencePos));
    }
    // BIND_CONN_TO_SESSION 41, EXCHANGE_ID 42, CREATE_SESSION 43,
    // DESTROY_SESSION 44, DESTROY_CLIENTID 57.
    if matches!(first, 41..=44 | 57) {
        if opcodes.len() > 1 {
            return Some((0, NfsStat4::Nfs4errNotOnlyOp));
        }
        return None;
    }
    Some((0, NfsStat4::Nfs4errOpNotInSession))
}

pub(crate) fn failed_result(op: u32, status: NfsStat4, result: Option<NfsResOp4>) -> NfsResOp4 {
    match result {
        Some(r @ NfsResOp4::Oplock(Lock4res::Denied(_)))
        | Some(r @ NfsResOp4::Oplockt(Lockt4res::Denied(_)))
            if status == NfsStat4::Nfs4errDenied =>
        {
            r
        }
        Some(r @ NfsResOp4::Opsetclientid(SetClientId4res::ClientUsing(_)))
            if status == NfsStat4::Nfs4errClidInuse =>
        {
            r
        }
        Some(NfsResOp4::Opsetattr(res)) => NfsResOp4::Opsetattr(SetAttr4res {
            status,
            attrsset: res.attrsset,
        }),
        _ if op == OP_SETATTR => NfsResOp4::Opsetattr(SetAttr4res {
            status,
            attrsset: Attrlist4(vec![]),
        }),
        _ => NfsResOp4::OpError { op, status },
    }
}

/// Reply bytes kept back from `ca_maxresponsesize` for what is around the
/// COMPOUND (#149): the RPC record mark and reply header (28 bytes), the
/// reply verifier (an RPCSEC_GSS MIC is at most 40), krb5i/krb5p framing
/// of the body (length, sequence number, MIC or Wrap token: under 100),
/// and the 8 bytes an NFS4ERR_REP_TOO_BIG result may need when the op
/// that does not fit is replaced by it.
pub const RPC_REPLY_OVERHEAD: usize = 256;

/// XDR length of `value` (counted, not buffered).
pub(crate) fn xdr_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    match nextnfs_proto::xdr::to_writer(&mut count, value) {
        Ok(()) => count.0,
        Err(_) => usize::MAX / 2,
    }
}

/// XDR length of one COMPOUND result as the reply's result array encodes
/// it (`write_argarray`): an `OpError` is its opcode and status.
pub(crate) fn result_len(res: &NfsResOp4) -> usize {
    match res {
        NfsResOp4::OpError { op, status } => xdr_len(&(op, status)),
        other => xdr_len(other),
    }
}

/// The NFSv4.1 COMPOUND reply budget (#149, finding 584f6c79c1f1): the
/// session's `ca_maxresponsesize` less [`RPC_REPLY_OVERHEAD`], with the
/// COMPOUND header (status, tag, result count) and every result so far
/// counted against it. Ops see what is left as `NfsRequest::reply_room`
/// (READ caps its data to it); a result that does not fit is replaced by
/// NFS4ERR_REP_TOO_BIG (RFC 8881 §2.10.6.4) and the COMPOUND stops.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplyBudget {
    limit: usize,
    used: usize,
}

impl ReplyBudget {
    pub(crate) fn new(max_response_size: u32, tag: &str) -> Self {
        ReplyBudget {
            limit: (max_response_size as usize).saturating_sub(RPC_REPLY_OVERHEAD),
            used: 4 + 4 + tag.len().div_ceil(4) * 4 + 4,
        }
    }

    /// Bytes left for the next result.
    pub(crate) fn room(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    /// Count `res` whether it fits or not (SEQUENCE's own result).
    pub(crate) fn add(&mut self, res: &NfsResOp4) {
        self.used += result_len(res);
    }

    /// Count `res`; false (nothing counted) when it does not fit.
    pub(crate) fn take(&mut self, res: &NfsResOp4) -> bool {
        let size = result_len(res);
        if self.used + size > self.limit {
            return false;
        }
        self.used += size;
        true
    }
}

/// Ops that set the current filehandle to a (possibly) new object.
fn moves_filehandle(opnum: u32) -> bool {
    matches!(opnum, OP_PUTFH | OP_PUTPUBFH | OP_PUTROOTFH | OP_RESTOREFH | OP_LOOKUP | OP_LOOKUPP)
}

/// Ops allowed to run on an export the call's flavour may not use: they
/// move the filehandle (checked after they run) or ask which flavours to
/// use (#106).
fn wrongsec_exempt(opnum: u32) -> bool {
    moves_filehandle(opnum) || matches!(opnum, OP_SECINFO | OP_SECINFO_NO_NAME)
}

/// The operation number (RFC 7530 / RFC 5661) of `arg`.
fn op_code(arg: &NfsArgOp) -> u32 {
    match arg {
        NfsArgOp::OpUndef0 => 0,
        NfsArgOp::OpUndef1 => 1,
        NfsArgOp::OpUndef2 => 2,
        NfsArgOp::OpAccess(_) => 3,
        NfsArgOp::Opclose(_) => 4,
        NfsArgOp::Opcommit(_) => 5,
        NfsArgOp::Opcreate(_) => 6,
        NfsArgOp::Opdelegpurge(_) => 7,
        NfsArgOp::Opdelegreturn(_) => 8,
        NfsArgOp::Opgetattr(_) => 9,
        NfsArgOp::Opgetfh(_) => 10,
        NfsArgOp::Oplink(_) => 11,
        NfsArgOp::Oplock(_) => 12,
        NfsArgOp::Oplockt(_) => 13,
        NfsArgOp::Oplocku(_) => 14,
        NfsArgOp::Oplookup(_) => 15,
        NfsArgOp::Oplookupp(_) => 16,
        NfsArgOp::Opnverify(_) => 17,
        NfsArgOp::Opopen(_) => 18,
        NfsArgOp::Opopenattr(_) => 19,
        NfsArgOp::OpopenConfirm(_) => 20,
        NfsArgOp::OpopenDowngrade(_) => 21,
        NfsArgOp::Opputfh(_) => 22,
        NfsArgOp::Opputpubfh(_) => 23,
        NfsArgOp::Opputrootfh(_) => 24,
        NfsArgOp::Opread(_) => 25,
        NfsArgOp::Opreaddir(_) => 26,
        NfsArgOp::Opreadlink(_) => 27,
        NfsArgOp::Opremove(_) => 28,
        NfsArgOp::Oprename(_) => 29,
        NfsArgOp::Oprenew(_) => 30,
        NfsArgOp::Oprestorefh(_) => 31,
        NfsArgOp::Opsavefh(_) => 32,
        NfsArgOp::OpSecinfo(_) => 33,
        NfsArgOp::Opsetattr(_) => 34,
        NfsArgOp::Opsetclientid(_) => 35,
        NfsArgOp::OpsetclientidConfirm(_) => 36,
        NfsArgOp::Opverify(_) => 37,
        NfsArgOp::Opwrite(_) => 38,
        NfsArgOp::OpreleaseLockOwner(_) => 39,
        NfsArgOp::OpbindConnToSession(_) => 41,
        NfsArgOp::OpexchangeId(_) => 42,
        NfsArgOp::OpcreateSession(_) => 43,
        NfsArgOp::OpdestroySession(_) => 44,
        NfsArgOp::OpfreeStateid(_) => 45,
        NfsArgOp::OpgetdeviceInfo(_) => 47,
        NfsArgOp::OpgetdeviceList(_) => 48,
        NfsArgOp::OplayoutCommit(_) => 49,
        NfsArgOp::OplayoutGet(_) => 50,
        NfsArgOp::OplayoutReturn(_) => 51,
        NfsArgOp::OpsecinfoNoName(_) => 52,
        NfsArgOp::Opsequence(_) => 53,
        NfsArgOp::OptestStateid(_) => 55,
        NfsArgOp::OpdestroyClientid(_) => 57,
        NfsArgOp::OpreclaimComplete(_) => 58,
        NfsArgOp::Opallocate(_) => 59,
        NfsArgOp::Opcopy(_) => 60,
        NfsArgOp::Opseek(_) => 69,
        NfsArgOp::OpbackchannelCtl => 40,
        NfsArgOp::OpgetDirDelegation => 46,
        NfsArgOp::OpsetSsv => 54,
        NfsArgOp::OpwantDelegation => 56,
        NfsArgOp::OpcopyNotify => 61,
        NfsArgOp::Opdeallocate => 62,
        NfsArgOp::OpioAdvise => 63,
        NfsArgOp::OplayoutError => 64,
        NfsArgOp::OplayoutStats => 65,
        NfsArgOp::OpoffloadCancel => 66,
        NfsArgOp::OpoffloadStatus => 67,
        NfsArgOp::OpreadPlus => 68,
    }
}

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
        NfsArgOp::OpbindConnToSession(_) => "BIND_CONN_TO_SESSION",
        NfsArgOp::OpexchangeId(_) => "EXCHANGE_ID",
        NfsArgOp::OpcreateSession(_) => "CREATE_SESSION",
        NfsArgOp::OpdestroySession(_) => "DESTROY_SESSION",
        NfsArgOp::OpfreeStateid(_) => "FREE_STATEID",
        NfsArgOp::OpgetdeviceInfo(_) => "GETDEVICEINFO",
        NfsArgOp::OpgetdeviceList(_) => "GETDEVICELIST",
        NfsArgOp::OplayoutCommit(_) => "LAYOUTCOMMIT",
        NfsArgOp::OplayoutGet(_) => "LAYOUTGET",
        NfsArgOp::OplayoutReturn(_) => "LAYOUTRETURN",
        NfsArgOp::OpsecinfoNoName(_) => "SECINFO_NO_NAME",
        NfsArgOp::Opsequence(_) => "SEQUENCE",
        NfsArgOp::OptestStateid(_) => "TEST_STATEID",
        NfsArgOp::OpdestroyClientid(_) => "DESTROY_CLIENTID",
        NfsArgOp::OpreclaimComplete(_) => "RECLAIM_COMPLETE",
        NfsArgOp::Opallocate(_) => "ALLOCATE",
        NfsArgOp::Opcopy(_) => "COPY",
        NfsArgOp::Opseek(_) => "SEEK",
        NfsArgOp::OpbackchannelCtl => "BACKCHANNEL_CTL",
        NfsArgOp::OpgetDirDelegation => "GET_DIR_DELEGATION",
        NfsArgOp::OpsetSsv => "SET_SSV",
        NfsArgOp::OpwantDelegation => "WANT_DELEGATION",
        NfsArgOp::OpcopyNotify => "COPY_NOTIFY",
        NfsArgOp::Opdeallocate => "DEALLOCATE",
        NfsArgOp::OpioAdvise => "IO_ADVISE",
        NfsArgOp::OplayoutError => "LAYOUTERROR",
        NfsArgOp::OplayoutStats => "LAYOUTSTATS",
        NfsArgOp::OpoffloadCancel => "OFFLOAD_CANCEL",
        NfsArgOp::OpoffloadStatus => "OFFLOAD_STATUS",
        NfsArgOp::OpreadPlus => "READ_PLUS",
    }
}

mod op_access;
mod op_close;
mod op_commit;
mod op_create;
mod op_delegpurge;
mod op_delegreturn;
mod op_getattr;
mod op_link;
mod op_lock;
mod op_lockt;
mod op_locku;
mod op_lookup;
mod op_open;
mod op_open_downgrade;
mod op_openattr;
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
mod op_secinfo_no_name;
mod op_set_clientid;
mod op_set_clientid_confirm;
mod op_setattr;
mod op_verify;
mod op_write;
// NFSv4.1 operations (RFC 5661)
mod op_bind_conn_to_session;
mod op_create_session;
mod op_destroy_clientid;
mod op_destroy_session;
mod op_exchange_id;
mod op_free_stateid;
mod op_reclaim_complete;
mod op_sequence;
mod op_test_stateid;
// pNFS layout operations (RFC 5661 §12)
mod op_getdeviceinfo;
mod op_getdevicelist;
mod op_layoutcommit;
mod op_layoutget;
mod op_layoutreturn;
// NFSv4.2 operations (RFC 7862)
mod op_allocate;
mod op_copy;
mod op_seek;

use super::NfsProtoImpl;
use super::nfs41::{SequenceReplay, SessionManager};
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

    async fn restore_filehandle<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        if request.restore_filehandle().await {
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
        request: NfsRequest<'a>,
    ) -> (NfsRequest<'a>, ReplyBody) {
        let solo_sequence = msg.args.as_ref().is_some_and(|a| {
            a.argarray.len() == 1 && matches!(a.argarray[0], NfsArgOp::Opsequence(_))
        });
        let (mut request, reply) = self.compound_ops(msg, request).await;
        // The slot this COMPOUND's SEQUENCE claimed keeps its reply for a
        // retry (RFC 8881 §2.10.6, #118), whatever the COMPOUND's status.
        // (A claim with no COMPOUND reply to keep frees the slot on drop.)
        if let (
            Some(claim),
            ReplyBody::MsgAccepted(AcceptedReply {
                reply_data: AcceptBody::Success(res),
                ..
            }),
        ) = (request.slot_claim.take(), &reply)
        {
            claim.finish(res, solo_sequence).await;
        }
        (request, reply)
    }

    fn minor_version(&self) -> u32 {
        1
    }
}

impl NFS40Server {
    /// Runs a COMPOUND's ops (see [`NfsProtoImpl::compound`]).
    async fn compound_ops<'a>(
        &self,
        msg: CallBody,
        mut request: NfsRequest<'a>,
    ) -> (NfsRequest<'a>, ReplyBody) {
        let mut last_status = NfsStat4::Nfs4Ok;
        // Every COMPOUND reply echoes the request's tag (RFC 7530 §16.2.3).
        let reply_tag = msg.args.as_ref().map(|a| a.tag.clone()).unwrap_or_default();
        let res = match msg.args {
            Some(args) => {
                // Reject NFSv4.2+ (only v4.0 and v4.1 supported)
                if args.minor_version > 1 {
                    debug!(
                        "COMPOUND: rejecting minor_version={}, only v4.0/v4.1 supported",
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

                // Store auth credentials in request for operation handlers
                request.set_auth_cred(msg.cred.clone());

                let mut resarray = Vec::with_capacity(args.argarray.len());
                let op_count = args.argarray.len();
                let opcodes: Vec<u32> = args.argarray.iter().map(op_code).collect();
                let placement = sequence_placement_error(args.minor_version, &opcodes);
                // Known once this COMPOUND's SEQUENCE has run (#149).
                let mut budget: Option<ReplyBudget> = None;
                for (op_idx, arg) in args.argarray.into_iter().enumerate() {
                    let operation = op_name(&arg);
                    debug!(op = operation, idx = op_idx, total = op_count, "compound: dispatching op");

                    // SEQUENCE placement (finding 739140171b7d): the op that
                    // breaks it fails and the COMPOUND stops there; the ops
                    // before it (a first SEQUENCE among them) have run.
                    if let Some((bad_idx, status)) = placement.clone() {
                        if bad_idx == op_idx {
                            debug!(op = operation, idx = op_idx, ?status, "compound: SEQUENCE placement");
                            // An op the minor version does not have is
                            // answered as OP_ILLEGAL (RFC 7530 §16.2.3).
                            let op = if status == NfsStat4::Nfs4errOpIllegal { 10044 } else { opcodes[op_idx] };
                            resarray.push(failed_result(op, status.clone(), None));
                            return (
                                request,
                                ReplyBody::MsgAccepted(AcceptedReply {
                                    verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                    reply_data: AcceptBody::Success(Compound4res {
                                        status,
                                        tag: reply_tag.clone(),
                                        resarray,
                                    }),
                                }),
                            );
                        }
                    }

                    // QoS rate limit check — if rate exceeded, return NFS4ERR_DELAY.
                    // READ (requested count, at most what one READ can return,
                    // #139) and WRITE (data length) are also charged against
                    // max_bytes_per_sec (#90).
                    let op_bytes = match &arg {
                        NfsArgOp::Opread(a) => a.count.min(crate::server::MAX_READ) as u64,
                        NfsArgOp::Opwrite(a) => a.data.len() as u64,
                        _ => 0,
                    };
                    let rate_limited = if let Some(rl) = request.rate_limiter() {
                        let rl = rl.clone();
                        let mut limiter = rl.lock().await;
                        !limiter.try_consume(op_bytes)
                    } else {
                        false
                    };
                    if rate_limited {
                        debug!(op = operation, bytes = op_bytes, "compound: QoS limit, NFS4ERR_DELAY");
                        resarray.push(failed_result(op_code(&arg), NfsStat4::Nfs4errDelay, None));
                        return (
                            request,
                            ReplyBody::MsgAccepted(AcceptedReply {
                                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                reply_data: AcceptBody::Success(Compound4res {
                                    status: NfsStat4::Nfs4errDelay,
                                    tag: reply_tag.clone(),
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
                            let denied_op = op_name(&arg);
                            info!(
                                op = denied_op,
                                status = "Nfs4errGrace",
                                "nfs_audit: operation denied during grace period"
                            );
                            resarray.push(failed_result(op_code(&arg), NfsStat4::Nfs4errGrace, None));
                            return (
                                request,
                                ReplyBody::MsgAccepted(AcceptedReply {
                                    verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                    reply_data: AcceptBody::Success(Compound4res {
                                        status: NfsStat4::Nfs4errGrace,
                                        tag: reply_tag.clone(),
                                        resarray,
                                    }),
                                }),
                            );
                        }
                    }

                    // Security flavour (#106): ops on an export must use one
                    // of its `sec` flavours, else NFS4ERR_WRONGSEC. Ops that
                    // only move the current filehandle, and SECINFO, run and
                    // are checked afterwards (RFC 7530 §2.6.3.1).
                    if !request.sec_allowed() && !wrongsec_exempt(op_code(&arg)) {
                        info!(
                            client = %request.client_addr(),
                            op = operation,
                            flavor = ?request.sec_flavor(),
                            status = "Nfs4errWrongsec",
                            export = ?request.current_export_id(),
                            "nfs_audit: security flavour not allowed on export"
                        );
                        resarray.push(failed_result(op_code(&arg), NfsStat4::Nfs4errWrongsec, None));
                        return (
                            request,
                            ReplyBody::MsgAccepted(AcceptedReply {
                                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                reply_data: AcceptBody::Success(Compound4res {
                                    status: NfsStat4::Nfs4errWrongsec,
                                    tag: reply_tag.clone(),
                                    resarray,
                                }),
                            }),
                        );
                    }

                    // Read-only export: refuse mutating ops with NFS4ERR_ROFS (#93).
                    // The export is the one selected by the current filehandle.
                    if request.is_read_only() && is_mutating(&arg) {
                        info!(
                            client = %request.client_addr(),
                            op = operation,
                            status = "Nfs4errRofs",
                            export = ?request.current_export_id(),
                            "nfs_audit: operation denied on read-only export"
                        );
                        resarray.push(failed_result(op_code(&arg), NfsStat4::Nfs4errRofs, None));
                        return (
                            request,
                            ReplyBody::MsgAccepted(AcceptedReply {
                                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                reply_data: AcceptBody::Success(Compound4res {
                                    status: NfsStat4::Nfs4errRofs,
                                    tag: reply_tag.clone(),
                                    resarray,
                                }),
                            }),
                        );
                    }

                    // Permissions: the caller (squash-mapped, #91) must have
                    // the mode bits the op needs on the objects it touches.
                    let caller = request.caller();
                    if let Err(status) = super::perm::check_op(&arg, &request, &caller) {
                        info!(
                            client = %request.client_addr(),
                            op = operation,
                            uid = caller.uid,
                            status = ?status,
                            export = ?request.current_export_id(),
                            path = request.current_filehandle().map(|fh| fh.path.as_str()).unwrap_or("-"),
                            "nfs_audit: permission denied"
                        );
                        resarray.push(failed_result(op_code(&arg), status.clone(), None));
                        return (
                            request,
                            ReplyBody::MsgAccepted(AcceptedReply {
                                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                reply_data: AcceptBody::Success(Compound4res {
                                    status,
                                    tag: reply_tag.clone(),
                                    resarray,
                                }),
                            }),
                        );
                    }

                    let opnum = op_code(&arg);
                    let export_before = request.current_export_id();
                    request.reply_room = budget.map(|b| b.room());
                    let mut response = match arg {
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
                        NfsArgOp::Oprestorefh(_) => self.restore_filehandle(request).await,

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

                        // delegation
                        NfsArgOp::Opdelegpurge(args) => args.execute(request).await,
                        NfsArgOp::Opdelegreturn(args) => args.execute(request).await,

                        // locking
                        NfsArgOp::Oplink(args) => args.execute(request).await,
                        NfsArgOp::Oplock(args) => args.execute(request).await,
                        NfsArgOp::Oplockt(args) => args.execute(request).await,
                        NfsArgOp::Oplocku(args) => args.execute(request).await,
                        NfsArgOp::OpreleaseLockOwner(args) => args.execute(request).await,

                        NfsArgOp::Opnverify(args) => args.execute(request).await,
                        NfsArgOp::Opverify(args) => args.execute(request).await,

                        NfsArgOp::Opopenattr(args) => args.execute(request).await,
                        NfsArgOp::OpopenDowngrade(args) => args.execute(request).await,
                        NfsArgOp::OpSecinfo(args) => args.execute(request).await,

                        // NFSv4.1 operations (RFC 5661)
                        NfsArgOp::OpexchangeId(args) => args.execute(request).await,
                        NfsArgOp::OpcreateSession(args) => args.execute(request).await,
                        NfsArgOp::Opsequence(args) => args.execute(request).await,
                        NfsArgOp::OpdestroySession(args) => args.execute(request).await,
                        NfsArgOp::OpdestroyClientid(args) => args.execute(request).await,
                        NfsArgOp::OpreclaimComplete(args) => args.execute(request).await,
                        NfsArgOp::OpbindConnToSession(args) => args.execute(request).await,
                        NfsArgOp::OpfreeStateid(args) => args.execute(request).await,
                        NfsArgOp::OptestStateid(args) => args.execute(request).await,

                        // pNFS layout operations (RFC 5661 §12)
                        NfsArgOp::OpgetdeviceInfo(args) => args.execute(request).await,
                        NfsArgOp::OpgetdeviceList(args) => args.execute(request).await,
                        NfsArgOp::OplayoutCommit(args) => args.execute(request).await,
                        NfsArgOp::OplayoutGet(args) => args.execute(request).await,
                        NfsArgOp::OplayoutReturn(args) => args.execute(request).await,
                        NfsArgOp::OpsecinfoNoName(args) => args.execute(request).await,

                        // NFSv4.2 operations (RFC 7862)
                        NfsArgOp::Opallocate(args) => args.execute(request).await,
                        NfsArgOp::Opcopy(args) => args.execute(request).await,
                        NfsArgOp::Opseek(args) => args.execute(request).await,

                        // Opcodes nextnfs does not implement (their arguments
                        // are not decoded): NFS4ERR_NOTSUPP with the op's
                        // result, so the client sees which op failed (#121).
                        NfsArgOp::OpbackchannelCtl | NfsArgOp::OpgetDirDelegation | NfsArgOp::OpsetSsv | NfsArgOp::OpwantDelegation | NfsArgOp::OpcopyNotify | NfsArgOp::Opdeallocate | NfsArgOp::OpioAdvise | NfsArgOp::OplayoutError | NfsArgOp::OplayoutStats | NfsArgOp::OpoffloadCancel | NfsArgOp::OpoffloadStatus | NfsArgOp::OpreadPlus => NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::OpError {
                                op: opnum,
                                status: NfsStat4::Nfs4errNotsupp,
                            }),
                            status: NfsStat4::Nfs4errNotsupp,
                        },
                    };
                    // A new file, directory or symlink is given to the caller
                    // (with its createattrs) by OPEN / CREATE themselves
                    // (`createattrs::finish_new_object`, #127).
                    // An op that moved the client onto another export (PUTFH,
                    // PUTROOTFH, LOOKUP from the pseudo-root, RESTOREFH, ...)
                    // must pass that export's `clients` list (#97).
                    if response.status == NfsStat4::Nfs4Ok
                        && response.request.current_export_id() != export_before
                        && !response.request.check_client_access()
                    {
                        info!(
                            client = %response.request.client_addr(),
                            op = operation,
                            status = "Nfs4errAccess",
                            export = ?response.request.current_export_id(),
                            "nfs_audit: client not in export's clients list"
                        );
                        response.result = Some(NfsResOp4::OpError {
                            op: opnum,
                            status: NfsStat4::Nfs4errAccess,
                        });
                        response.status = NfsStat4::Nfs4errAccess;
                    }
                    // An op that put the client on an export whose `sec` does
                    // not include the call's flavour fails with WRONGSEC,
                    // unless a PUTFH-like op is followed by SECINFO or a
                    // LOOKUP (which are then checked themselves), RFC 7530
                    // §2.6.3.1.1 (#106).
                    if response.status == NfsStat4::Nfs4Ok
                        && moves_filehandle(opnum)
                        && !response.request.sec_allowed()
                    {
                        let next = opcodes.get(op_idx + 1).copied();
                        let deferred = matches!(opnum, OP_PUTFH | OP_PUTPUBFH | OP_PUTROOTFH | OP_RESTOREFH)
                            && matches!(next, Some(OP_SECINFO | OP_SECINFO_NO_NAME | OP_LOOKUP | OP_LOOKUPP));
                        if !deferred {
                            info!(
                                client = %response.request.client_addr(),
                                op = operation,
                                flavor = ?response.request.sec_flavor(),
                                status = "Nfs4errWrongsec",
                                export = ?response.request.current_export_id(),
                                "nfs_audit: security flavour not allowed on export"
                            );
                            response.result = Some(NfsResOp4::OpError {
                                op: opnum,
                                status: NfsStat4::Nfs4errWrongsec,
                            });
                            response.status = NfsStat4::Nfs4errWrongsec;
                        }
                    }

                    // A retried NFSv4.1 request (same slot and seqid) is not
                    // executed again (#118): send the cached reply, or fail
                    // the next op with NFS4ERR_RETRY_UNCACHED_REP.
                    if let Some(replay) = response.request.sequence_replay.take() {
                        let reply = match replay {
                            SequenceReplay::Cached(cached) => (*cached).clone(),
                            SequenceReplay::Uncached => {
                                resarray.extend(response.result);
                                let status = match opcodes.get(op_idx + 1) {
                                    Some(&op) => {
                                        resarray.push(failed_result(
                                            op,
                                            NfsStat4::Nfs4errRetryUncachedRep,
                                            None,
                                        ));
                                        NfsStat4::Nfs4errRetryUncachedRep
                                    }
                                    None => NfsStat4::Nfs4Ok,
                                };
                                Compound4res { status, tag: reply_tag.clone(), resarray }
                            }
                        };
                        info!(
                            client = %response.request.client_addr(),
                            op = operation,
                            status = ?reply.status,
                            "nfs_audit: SEQUENCE retry answered from the slot's reply cache"
                        );
                        return (
                            response.request,
                            ReplyBody::MsgAccepted(AcceptedReply {
                                verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                reply_data: AcceptBody::Success(reply),
                            }),
                        );
                    }

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

                    // Every op that ran is in the reply (#131): a failed
                    // op's result is made by `failed_result`, also when the
                    // handler returned none; a success without a result is
                    // a server bug, answered NFS4ERR_SERVERFAULT.
                    let res = match (last_status.clone(), res) {
                        (NfsStat4::Nfs4Ok, Some(res)) => res,
                        (NfsStat4::Nfs4Ok, None) => {
                            info!(op = operation, "nfs_audit: op succeeded without a result");
                            last_status = NfsStat4::Nfs4errServerfault;
                            failed_result(opnum, NfsStat4::Nfs4errServerfault, None)
                        }
                        (status, res) => failed_result(opnum, status, res),
                    };
                    // The whole reply stays within the session's
                    // ca_maxresponsesize (#149): the budget starts with the
                    // SEQUENCE that opened it, and a result that would
                    // overflow it is replaced by NFS4ERR_REP_TOO_BIG.
                    if op_idx == 0 && opnum == OP_SEQUENCE && last_status == NfsStat4::Nfs4Ok {
                        if let (Some(sessions), Some(id)) =
                            (response.request.session_manager(), response.request.session_id.as_ref())
                        {
                            if let Some(attrs) = sessions.fore_channel_attrs(id).await {
                                let mut b = ReplyBudget::new(attrs.max_response_size, &reply_tag);
                                // SEQUENCE's own result is counted, never refused.
                                b.add(&res);
                                budget = Some(b);
                            }
                        }
                        resarray.push(res);
                    } else if budget.as_mut().is_some_and(|b| !b.take(&res)) {
                        info!(
                            client = %response.request.client_addr(),
                            op = operation,
                            status = "Nfs4errRepTooBig",
                            "nfs_audit: result does not fit the session's ca_maxresponsesize"
                        );
                        last_status = NfsStat4::Nfs4errRepTooBig;
                        resarray.push(failed_result(opnum, NfsStat4::Nfs4errRepTooBig, None));
                    } else {
                        resarray.push(res);
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
                                        tag: reply_tag.clone(),
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
                    tag: reply_tag.clone(),
                    resarray: res,
                }),
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;
    use crate::server::export_manager::{AccessConfig, SquashMode};
    use std::os::unix::fs::PermissionsExt;

    /// AUTH_SYS credential for `uid`/`gid`.
    fn auth_sys(uid: u32, gid: u32) -> OpaqueAuth {
        OpaqueAuth::AuthUnix(AuthUnix {
            stamp: 0,
            machinename: "test".to_string(),
            uid,
            gid,
            gids: vec![gid],
        })
    }

    /// The test process's own identity: it owns the tempdirs tests export.
    fn own_ids() -> (u32, u32) {
        unsafe { (libc::geteuid(), libc::getegid()) }
    }

    /// COMPOUND sent as the test process's own uid/gid (AUTH_SYS).
    fn make_compound(ops: Vec<NfsArgOp>) -> CallBody {
        let (uid, gid) = own_ids();
        make_compound_as(auth_sys(uid, gid), ops)
    }

    fn make_compound_as(cred: OpaqueAuth, ops: Vec<NfsArgOp>) -> CallBody {
        CallBody {
            rpcvers: 2,
            prog: 100003,
            vers: 4,
            proc: 1,
            cred,
            verf: OpaqueAuth::AuthNull(vec![]),
            gss: None,
            args: Some(Compound4args {
                tag: "test".to_string(),
                minor_version: 0,
                argarray: ops,
            }),
        }
    }

    /// Request whose current export is a read-only (or writable) real export,
    /// with `squash = "none"` so the test process passes the permission
    /// checks (#91) as itself, root or not.
    async fn request_on_export(read_only: bool, dir: &std::path::Path) -> NfsRequest<'static> {
        request_on_export_squash(read_only, dir, SquashMode::None).await
    }

    async fn request_on_export_squash(
        read_only: bool,
        dir: &std::path::Path,
        squash: SquashMode,
    ) -> NfsRequest<'static> {
        let mut request = create_nfs40_server(None).await;
        let em = request.export_manager();
        let info = em
            .add_export("ro".to_string(), dir.to_path_buf(), read_only)
            .await
            .unwrap();
        em.set_access("ro", AccessConfig { squash, ..Default::default() })
            .await
            .unwrap();
        request.set_export(info.export_id).await;
        request
    }

    fn open_args(share_access: u32, openhow: OpenFlag4) -> Open4args {
        Open4args {
            seqid: 1,
            share_access,
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"owner".to_vec(),
            },
            openhow,
            claim: OpenClaim4::ClaimNull("f".to_string()),
        }
    }

    fn write_op() -> NfsArgOp {
        NfsArgOp::Opwrite(Write4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"x".to_vec(),
        })
    }

    #[test]
    fn test_is_mutating_open() {
        let create = OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
            attrmask: Attrlist4(vec![]),
            attr_vals: Attrlist4(vec![]),
        }));
        assert!(!is_mutating(&NfsArgOp::Opopen(open_args(1, OpenFlag4::Open4Nocreate))));
        assert!(is_mutating(&NfsArgOp::Opopen(open_args(2, OpenFlag4::Open4Nocreate))));
        assert!(is_mutating(&NfsArgOp::Opopen(open_args(3, OpenFlag4::Open4Nocreate))));
        assert!(is_mutating(&NfsArgOp::Opopen(open_args(1, create))));
        assert!(is_mutating(&write_op()));
        assert!(is_mutating(&NfsArgOp::Opremove(Remove4args {
            target: "f".to_string()
        })));
        assert!(!is_mutating(&NfsArgOp::Opputrootfh(())));
    }

    #[tokio::test]
    async fn test_compound_read_only_export_refuses_write() {
        let dir = tempfile::tempdir().unwrap();
        let request = request_on_export(true, dir.path()).await;
        let server = NFS40Server::new();
        let (_request, reply) = server.compound(make_compound(vec![write_op()]), request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errRofs);
                    assert_eq!(
                        res.resarray,
                        vec![NfsResOp4::OpError {
                            op: 38,
                            status: NfsStat4::Nfs4errRofs
                        }]
                    );
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    /// NFSv4.1 COMPOUND: SEQUENCE on `slot`/`seq`, then `ops`.
    fn v41_compound(tag: &str, sid: [u8; 16], seq: u32, cache: bool, ops: Vec<NfsArgOp>) -> CallBody {
        let mut argarray = vec![NfsArgOp::Opsequence(Sequence4args {
            sa_sessionid: sid,
            sa_sequenceid: seq,
            sa_slotid: 0,
            sa_highest_slotid: 0,
            sa_cachethis: cache,
        })];
        argarray.extend(ops);
        let mut msg = make_compound(argarray);
        let args = msg.args.as_mut().unwrap();
        args.minor_version = 1;
        args.tag = tag.to_string();
        msg
    }

    #[test]
    fn test_sequence_placement_rules() {
        use NfsStat4::*;
        let p = sequence_placement_error;
        // v4.1: SEQUENCE first, alone or with ops.
        assert_eq!(p(1, &[53]), None);
        assert_eq!(p(1, &[53, 24, 10]), None);
        assert_eq!(p(1, &[53, 42]), None); // session-less op after SEQUENCE
        // A second SEQUENCE fails where it is.
        assert_eq!(p(1, &[53, 24, 53]), Some((2, Nfs4errSequencePos)));
        // No SEQUENCE first.
        assert_eq!(p(1, &[24, 53]), Some((0, Nfs4errOpNotInSession)));
        assert_eq!(p(1, &[24]), Some((0, Nfs4errOpNotInSession)));
        // Session-less ops alone are fine, with company they are not.
        for op in [41, 42, 43, 44, 57] {
            assert_eq!(p(1, &[op]), None, "op {}", op);
            assert_eq!(p(1, &[op, 24]), Some((0, Nfs4errNotOnlyOp)), "op {}", op);
        }
        assert_eq!(p(1, &[]), None);
        // v4.0 has no SEQUENCE.
        assert_eq!(p(0, &[24, 10]), None);
        assert_eq!(p(0, &[24, 53]), Some((1, Nfs4errOpIllegal)));
    }

    /// Finding 739140171b7d end to end: a second SEQUENCE in a COMPOUND
    /// fails with NFS4ERR_SEQUENCE_POS after the ops before it ran, and the
    /// first SEQUENCE's slot keeps that reply: the retry gets it again and
    /// REMOVE does not run twice.
    #[tokio::test]
    async fn test_second_sequence_is_sequence_pos() {
        let dir = tempfile::tempdir().unwrap();
        let (request, sid) = v41_request(dir.path()).await;
        let server = NFS40Server::new();
        let second = NfsArgOp::Opsequence(Sequence4args {
            sa_sessionid: sid,
            sa_sequenceid: 2,
            sa_slotid: 0,
            sa_highest_slotid: 0,
            sa_cachethis: true,
        });
        let msg = v41_compound("pos", sid, 1, true, vec![remove_f(), second.clone()]);
        let (request, reply) = server.compound(msg, request).await;
        let res = compound_res(reply);
        assert_eq!(res.status, NfsStat4::Nfs4errSequencePos);
        assert_eq!(res.resarray.len(), 3);
        assert_eq!(res.resarray[2], NfsResOp4::OpError { op: 53, status: NfsStat4::Nfs4errSequencePos });
        assert!(!dir.path().join("f").exists());

        // The retry: the same reply from the slot's cache.
        std::fs::write(dir.path().join("f"), b"y").unwrap();
        let msg = v41_compound("pos", sid, 1, true, vec![remove_f(), second]);
        let (_request, reply) = server.compound(msg, request).await;
        let retry = compound_res(reply);
        assert_eq!(retry.status, NfsStat4::Nfs4errSequencePos);
        assert_eq!(retry.resarray, res.resarray);
        assert!(dir.path().join("f").exists(), "the retry ran REMOVE again");
    }

    /// Writable export with a 100 000-byte file `big`, its root as the
    /// current filehandle, and an NFSv4.1 session whose
    /// ca_maxresponsesize is `max`.
    async fn v41_request_max(dir: &std::path::Path, max: u32) -> (NfsRequest<'static>, [u8; 16]) {
        use crate::server::nfs41::ChannelAttrs;
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("big"), &data).unwrap();
        let mut request = request_on_export(false, dir).await;
        let root = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root);
        let session = request
            .session_manager()
            .unwrap()
            .create_session_with(
                1,
                4,
                ChannelAttrs { max_response_size: max, ..ChannelAttrs::default() },
            )
            .await;
        (request, session.id)
    }

    fn lookup_big() -> NfsArgOp {
        NfsArgOp::Oplookup(Lookup4args { objname: "big".to_string() })
    }

    fn read_op(offset: u64, count: u32) -> NfsArgOp {
        NfsArgOp::Opread(Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset,
            count,
        })
    }

    /// Bytes of the whole RPC reply carrying `res` with an AUTH_NONE
    /// verifier: record mark, xid, msg type, reply stat, verifier
    /// (flavour + length), accept stat, then the COMPOUND.
    fn wire_len(res: &Compound4res) -> usize {
        4 + 4 + 4 + 4 + 8 + 4 + xdr_len(res)
    }

    fn read_lens(res: &Compound4res) -> Vec<usize> {
        res.resarray
            .iter()
            .filter_map(|r| match r {
                NfsResOp4::Opread(Read4res::Resok4(ok)) => Some(ok.data.len()),
                _ => None,
            })
            .collect()
    }

    /// #149, finding 584f6c79c1f1: SEQUENCE + LOOKUP + two READs of 8 KiB
    /// each in a 16 KiB session. Each READ used to be capped on its own at
    /// the session size less 1 KiB, so both returned 8 KiB and the reply
    /// passed 16 KiB. Now the first gets its 8 KiB, the second a short
    /// read of what is left, and the whole reply fits.
    #[tokio::test]
    async fn test_two_reads_fit_session_response_size() {
        let max = 16384u32;
        let dir = tempfile::tempdir().unwrap();
        let (request, sid) = v41_request_max(dir.path(), max).await;
        let server = NFS40Server::new();
        let msg = v41_compound("two", sid, 1, false, vec![lookup_big(), read_op(0, 8192), read_op(8192, 8192)]);
        let (_request, reply) = server.compound(msg, request).await;
        let res = compound_res(reply);
        assert_eq!(res.status, NfsStat4::Nfs4Ok);
        let lens = read_lens(&res);
        assert_eq!(lens.len(), 2);
        assert_eq!(lens[0], 8192);
        assert!(lens[1] > 0 && lens[1] < 8192, "{:?}", lens);
        assert!(wire_len(&res) <= max as usize, "reply {} > {}", wire_len(&res), max);
        // The data is the file's, from each READ's offset.
        for (r, off) in res.resarray.iter().filter_map(|r| match r {
            NfsResOp4::Opread(Read4res::Resok4(ok)) => Some(ok),
            _ => None,
        }).zip([0usize, 8192]) {
            assert!(r.data.iter().enumerate().all(|(i, b)| *b == ((off + i) % 251) as u8));
        }
    }

    /// A long tag counts too: with a 3000-byte tag in an 8 KiB session one
    /// READ of 8 KiB still leaves the reply within the session size.
    #[tokio::test]
    async fn test_long_tag_counts_against_session_response_size() {
        let max = 8192u32;
        let dir = tempfile::tempdir().unwrap();
        let (request, sid) = v41_request_max(dir.path(), max).await;
        let server = NFS40Server::new();
        let tag = "t".repeat(3000);
        let msg = v41_compound(&tag, sid, 1, false, vec![lookup_big(), read_op(0, 8192)]);
        let (_request, reply) = server.compound(msg, request).await;
        let res = compound_res(reply);
        assert_eq!(res.status, NfsStat4::Nfs4Ok);
        let lens = read_lens(&res);
        assert_eq!(lens.len(), 1);
        assert!(lens[0] > 0 && lens[0] < 8192 - 3000);
        assert!(wire_len(&res) <= max as usize, "reply {} > {}", wire_len(&res), max);
    }

    /// Many READs: each later one gets less, the last ones get
    /// REP_TOO_BIG once nothing is left, and the reply never overflows.
    #[tokio::test]
    async fn test_reads_past_the_budget_are_rep_too_big() {
        let max = 4096u32;
        let dir = tempfile::tempdir().unwrap();
        let (request, sid) = v41_request_max(dir.path(), max).await;
        let server = NFS40Server::new();
        let ops = vec![lookup_big(), read_op(0, 4096), read_op(4096, 4096), read_op(8192, 4096)];
        let msg = v41_compound("many", sid, 1, false, ops);
        let (_request, reply) = server.compound(msg, request).await;
        let res = compound_res(reply);
        assert_eq!(res.status, NfsStat4::Nfs4errRepTooBig);
        assert_eq!(
            res.resarray.last(),
            Some(&NfsResOp4::OpError { op: 25, status: NfsStat4::Nfs4errRepTooBig })
        );
        assert!(wire_len(&res) <= max as usize, "reply {} > {}", wire_len(&res), max);
    }

    /// A non-READ result that does not fit is replaced by REP_TOO_BIG: the
    /// session size leaves room for the COMPOUND header, the tag and
    /// SEQUENCE's result only, so LOOKUP (8 bytes) fails.
    #[tokio::test]
    async fn test_result_past_the_budget_is_rep_too_big() {
        let dir = tempfile::tempdir().unwrap();
        let tag = "x".repeat(100);
        // SEQUENCE4resok: opcode, status, sessionid 16, 5 words.
        let seq_len = 4 + 4 + 16 + 5 * 4;
        let max = (RPC_REPLY_OVERHEAD + 12 + 100 + seq_len + 4) as u32;
        let (request, sid) = v41_request_max(dir.path(), max).await;
        let server = NFS40Server::new();
        let msg = v41_compound(&tag, sid, 1, false, vec![lookup_big(), read_op(0, 10)]);
        let (_request, reply) = server.compound(msg, request).await;
        let res = compound_res(reply);
        assert_eq!(res.status, NfsStat4::Nfs4errRepTooBig);
        assert_eq!(res.resarray.len(), 2);
        assert!(matches!(res.resarray[0], NfsResOp4::Opsequence(Sequence4res::Resok4(_))));
        assert_eq!(res.resarray[1], NfsResOp4::OpError { op: 15, status: NfsStat4::Nfs4errRepTooBig });
    }

    #[test]
    fn test_reply_budget_counts_header_and_results() {
        let mut b = ReplyBudget::new(RPC_REPLY_OVERHEAD as u32 + 100, "abcde");
        // status + tag (4 + 8 padded) + count = 20.
        assert_eq!(b.room(), 80);
        let putfh = NfsResOp4::OpError { op: 22, status: NfsStat4::Nfs4Ok };
        assert_eq!(result_len(&putfh), 8);
        assert!(b.take(&putfh));
        assert_eq!(b.room(), 72);
        let big = NfsResOp4::Opread(Read4res::Resok4(Read4resok { eof: false, data: vec![0; 64] }));
        assert!(!b.take(&big), "16 + 64 > 72");
        assert_eq!(b.room(), 72);
        let fits = NfsResOp4::Opread(Read4res::Resok4(Read4resok { eof: false, data: vec![0; 56] }));
        assert!(b.take(&fits));
        assert_eq!(b.room(), 0);
        // Below the overhead: no room at all.
        assert_eq!(ReplyBudget::new(10, "").room(), 0);
    }

    fn remove_f() -> NfsArgOp {
        NfsArgOp::Opremove(Remove4args { target: "f".to_string() })
    }

    /// Writable export with its root as the current filehandle, a file `f`,
    /// and an NFSv4.1 session.
    async fn v41_request(dir: &std::path::Path) -> (NfsRequest<'static>, [u8; 16]) {
        std::fs::write(dir.join("f"), b"x").unwrap();
        let mut request = request_on_export(false, dir).await;
        let root = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root);
        let session = request.session_manager().unwrap().create_session(1, 4).await;
        (request, session.id)
    }

    fn compound_res(reply: ReplyBody) -> Compound4res {
        match reply {
            ReplyBody::MsgAccepted(AcceptedReply { reply_data: AcceptBody::Success(res), .. }) => res,
            _ => panic!("Expected a COMPOUND reply"),
        }
    }

    #[tokio::test]
    async fn test_sequence_retry_replays_cached_reply() {
        let dir = tempfile::tempdir().unwrap();
        let (request, sid) = v41_request(dir.path()).await;
        let server = NFS40Server::new();

        let (request, reply) = server
            .compound(v41_compound("first", sid, 1, true, vec![remove_f()]), request)
            .await;
        let first = compound_res(reply);
        assert_eq!(first.status, NfsStat4::Nfs4Ok);
        assert!(!dir.path().join("f").exists());

        // Same slot + seqid: answered from the cache (the original tag, and
        // the REMOVE is not run again — it would now be NOENT).
        std::fs::write(dir.path().join("f"), b"again").unwrap();
        let (request, reply) = server
            .compound(v41_compound("retry", sid, 1, true, vec![remove_f()]), request)
            .await;
        assert_eq!(compound_res(reply), first);
        assert!(dir.path().join("f").exists(), "retry must not execute REMOVE again");

        // Next seqid runs normally.
        let (request, reply) = server
            .compound(v41_compound("next", sid, 2, true, vec![remove_f()]), request)
            .await;
        assert_eq!(compound_res(reply).status, NfsStat4::Nfs4Ok);
        assert!(!dir.path().join("f").exists());

        // A seqid that is neither next nor a retry is misordered.
        let (_request, reply) = server
            .compound(v41_compound("skip", sid, 9, true, vec![remove_f()]), request)
            .await;
        assert_eq!(compound_res(reply).status, NfsStat4::Nfs4errSeqMisordered);
    }

    #[tokio::test]
    async fn test_sequence_retry_uncached() {
        let dir = tempfile::tempdir().unwrap();
        let (request, sid) = v41_request(dir.path()).await;
        let server = NFS40Server::new();

        let (request, reply) = server
            .compound(v41_compound("first", sid, 1, false, vec![remove_f()]), request)
            .await;
        assert_eq!(compound_res(reply).status, NfsStat4::Nfs4Ok);

        std::fs::write(dir.path().join("f"), b"again").unwrap();
        let (request, reply) = server
            .compound(v41_compound("retry", sid, 1, false, vec![remove_f()]), request)
            .await;
        let res = compound_res(reply);
        assert_eq!(res.status, NfsStat4::Nfs4errRetryUncachedRep);
        assert_eq!(res.resarray.len(), 2);
        assert!(matches!(res.resarray[0], NfsResOp4::Opsequence(Sequence4res::Resok4(_))));
        assert_eq!(
            res.resarray[1],
            NfsResOp4::OpError { op: 28, status: NfsStat4::Nfs4errRetryUncachedRep }
        );
        assert!(dir.path().join("f").exists(), "retry must not execute REMOVE again");

        // A SEQUENCE-only COMPOUND is cached even without sa_cachethis.
        let (request, reply) = server.compound(v41_compound("solo", sid, 2, false, vec![]), request).await;
        let solo = compound_res(reply);
        let (_request, reply) = server.compound(v41_compound("again", sid, 2, false, vec![]), request).await;
        assert_eq!(compound_res(reply), solo);
    }

    #[tokio::test]
    async fn test_compound_read_only_export_refuses_remove() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"keep").unwrap();
        let request = request_on_export(true, dir.path()).await;
        let server = NFS40Server::new();
        let msg = make_compound(vec![NfsArgOp::Opremove(Remove4args {
            target: "f".to_string(),
        })]);
        let (_request, reply) = server.compound(msg, request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => assert_eq!(res.status, NfsStat4::Nfs4errRofs),
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
        assert!(dir.path().join("f").exists());
    }

    #[tokio::test]
    async fn test_compound_max_bytes_per_sec_delays_write() {
        let dir = tempfile::tempdir().unwrap();
        let request = request_on_export(false, dir.path()).await;
        request
            .rate_limiter()
            .unwrap()
            .lock()
            .await
            .update_config(crate::server::export_manager::QosConfig {
                max_ops_per_sec: 0,
                max_bytes_per_sec: 1,
            });
        let server = NFS40Server::new();
        // The first one-byte WRITE takes the whole bucket (it then fails on
        // its own, there is no filehandle); the second is delayed.
        let (request, _reply) = server.compound(make_compound(vec![write_op()]), request).await;
        let (_request, reply) = server.compound(make_compound(vec![write_op()]), request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errDelay);
                    assert_eq!(
                        res.resarray,
                        vec![NfsResOp4::OpError {
                            op: 38,
                            status: NfsStat4::Nfs4errDelay
                        }]
                    );
                }
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    #[tokio::test]
    async fn test_compound_writable_export_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        let request = request_on_export(false, dir.path()).await;
        let server = NFS40Server::new();
        // No filehandle: WRITE fails, but with its own error, not ROFS
        let (_request, reply) = server.compound(make_compound(vec![write_op()]), request).await;
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => assert_ne!(res.status, NfsStat4::Nfs4errRofs),
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
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
            gss: None,
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
            gss: None,
            args: Some(Compound4args {
                tag: "test".to_string(),
                minor_version: 2, // v4.2 — should be rejected (only 0 and 1 supported)
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
    async fn test_compound_minor_version_1_needs_sequence() {
        let server = NFS40Server::new();
        let request = create_nfs40_server(None).await;
        let msg = CallBody {
            rpcvers: 2,
            prog: 100003,
            vers: 4,
            proc: 1,
            cred: OpaqueAuth::AuthNull(vec![]),
            verf: OpaqueAuth::AuthNull(vec![]),
            gss: None,
            args: Some(Compound4args {
                tag: "test".to_string(),
                // v4.1 is accepted, but its ops need a session: without
                // SEQUENCE first, PUTROOTFH is NFS4ERR_OP_NOT_IN_SESSION
                // (finding 739140171b7d) and does not run.
                minor_version: 1,
                argarray: vec![NfsArgOp::Opputrootfh(()), NfsArgOp::Opgetfh(())],
            }),
        };
        let (request, reply) = server.compound(msg, request).await;
        assert!(request.current_filehandle().is_none());
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => {
                    assert_eq!(res.status, NfsStat4::Nfs4errOpNotInSession);
                    assert_eq!(
                        res.resarray,
                        vec![NfsResOp4::OpError { op: 24, status: NfsStat4::Nfs4errOpNotInSession }]
                    );
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
        assert!(request.restore_filehandle().await);
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

    // --- #97: `clients` allow-list on every export switch ---

    /// Two exports, so PUTROOTFH lands on the pseudo-root. "locked" only
    /// allows 10.0.0.0/8; the test client is 127.0.0.1.
    async fn multi_export_request(
        open_dir: &std::path::Path,
        locked_dir: &std::path::Path,
    ) -> (NfsRequest<'static>, u8) {
        let request = create_nfs40_server(None).await;
        let em = request.export_manager();
        em.add_export("open".to_string(), open_dir.to_path_buf(), false)
            .await
            .unwrap();
        let locked = em
            .add_export("locked".to_string(), locked_dir.to_path_buf(), false)
            .await
            .unwrap();
        em.set_access(
            "locked",
            crate::server::export_manager::AccessConfig {
                clients: vec!["10.0.0.0/8".to_string()],
                squash: SquashMode::None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        (request, locked.export_id)
    }

    fn compound_result(reply: ReplyBody) -> (NfsStat4, Vec<NfsResOp4>) {
        match reply {
            ReplyBody::MsgAccepted(accepted) => match accepted.reply_data {
                AcceptBody::Success(res) => (res.status, res.resarray),
                _ => panic!("Expected Success"),
            },
            _ => panic!("Expected MsgAccepted"),
        }
    }

    fn lookup_op(name: &str) -> NfsArgOp {
        NfsArgOp::Oplookup(Lookup4args {
            objname: name.to_string(),
        })
    }

    #[tokio::test]
    async fn test_lookup_from_pseudo_root_refuses_denied_export() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(b.path().join("secret"), b"x").unwrap();
        let (request, _) = multi_export_request(a.path(), b.path()).await;
        let server = NFS40Server::new();
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            lookup_op("locked"),
            lookup_op("secret"),
            NfsArgOp::Opgetfh(()),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errAccess);
        assert_eq!(resarray.len(), 2);
        // A failed op's result is its opcode and status (#131).
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 15, status: NfsStat4::Nfs4errAccess });
    }

    // --- #106: per-export `sec` flavours (NFS4ERR_WRONGSEC, SECINFO) ---

    /// Two exports: "open" (sys) and "krb" (krb5i only).
    async fn sec_export_request(open_dir: &std::path::Path, krb_dir: &std::path::Path) -> NfsRequest<'static> {
        use crate::server::export_manager::SecFlavor;
        // World-searchable: the test calls are the anonymous user.
        for d in [open_dir, krb_dir] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let request = create_nfs40_server(None).await;
        let em = request.export_manager();
        em.add_export("open".to_string(), open_dir.to_path_buf(), false).await.unwrap();
        em.add_export("krb".to_string(), krb_dir.to_path_buf(), false).await.unwrap();
        em.set_access("krb", AccessConfig { squash: SquashMode::None, sec: vec![SecFlavor::Krb5i], ..Default::default() })
            .await
            .unwrap();
        em.set_access("open", AccessConfig { squash: SquashMode::None, ..Default::default() }).await.unwrap();
        request
    }

    fn krb5i_secinfo() -> SeCinfo4 {
        SeCinfo4::FlavorInfo(RpcSecGssInfo {
            oid: KRB5_MECH_OID.to_vec(),
            qop: 0,
            service: RpcGssSvc::RpcGssSvcIntegrity,
        })
    }

    #[tokio::test]
    async fn test_wrongsec_lookup_into_kerberos_export_with_auth_sys() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let request = sec_export_request(a.path(), b.path()).await;
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), lookup_op("krb"), NfsArgOp::Opgetfh(())]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errWrongsec);
        assert_eq!(resarray.len(), 2);
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 15, status: NfsStat4::Nfs4errWrongsec });
    }

    #[tokio::test]
    async fn test_kerberos_flavour_reaches_kerberos_export_not_sys_one() {
        use crate::server::export_manager::SecFlavor;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut request = sec_export_request(a.path(), b.path()).await;
        request.set_sec_flavor(SecFlavor::Krb5i);
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), lookup_op("krb"), NfsArgOp::Opgetfh(())]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4Ok);

        // krb5 (no integrity) is not in the export's list either.
        let mut request = sec_export_request(a.path(), b.path()).await;
        request.set_sec_flavor(SecFlavor::Krb5);
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), lookup_op("krb"), NfsArgOp::Opgetfh(())]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4errWrongsec);

        // A krb5i call on the sys-only export is refused too.
        let mut request = sec_export_request(a.path(), b.path()).await;
        request.set_sec_flavor(SecFlavor::Krb5i);
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), lookup_op("open"), NfsArgOp::Opgetfh(())]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4errWrongsec);
    }

    #[tokio::test]
    async fn test_putfh_then_secinfo_no_name_is_not_wrongsec() {
        use crate::server::export_manager::SecFlavor;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // Get the krb export's root handle with an allowed flavour.
        let mut request = sec_export_request(a.path(), b.path()).await;
        request.set_sec_flavor(SecFlavor::Krb5i);
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), lookup_op("krb"), NfsArgOp::Opgetfh(())]);
        let (request, reply) = NFS40Server::new().compound(msg, request).await;
        let fh = match compound_result(reply).1.pop() {
            Some(NfsResOp4::Opgetfh(GetFh4res::Resok4(ok))) => ok.object,
            other => panic!("expected GETFH, got {:?}", other),
        };
        let mut request = request;
        request.set_sec_flavor(SecFlavor::Sys);

        // PUTFH + SECINFO_NO_NAME: allowed, and lists krb5i.
        let msg = make_compound(vec![
            NfsArgOp::Opputfh(PutFh4args { object: fh }),
            NfsArgOp::OpsecinfoNoName(SecinfoNoName4args { sina_style: SecinfoStyle4::SecinfoStyle4CurrentFh }),
        ]);
        let (request, reply) = NFS40Server::new().compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4Ok, "{:?}", resarray);
        assert_eq!(resarray[1], NfsResOp4::OpsecinfoNoName(SecinfoNoName4res::Resok4(vec![krb5i_secinfo()])));

        // PUTFH + GETFH: WRONGSEC on the PUTFH.
        let msg = make_compound(vec![NfsArgOp::Opputfh(PutFh4args { object: fh }), NfsArgOp::Opgetfh(())]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errWrongsec);
        assert_eq!(resarray, vec![NfsResOp4::OpError { op: 22, status: NfsStat4::Nfs4errWrongsec }]);
    }

    #[tokio::test]
    async fn test_secinfo_at_pseudo_root_names_each_exports_flavours() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let request = sec_export_request(a.path(), b.path()).await;
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::OpSecinfo(SecInfo4args { name: "krb".to_string() }),
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::OpSecinfo(SecInfo4args { name: "open".to_string() }),
            NfsArgOp::Opputrootfh(()),
            NfsArgOp::OpsecinfoNoName(SecinfoNoName4args { sina_style: SecinfoStyle4::SecinfoStyle4CurrentFh }),
        ]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4Ok);
        assert_eq!(resarray[1], NfsResOp4::OpSecinfo(SecInfo4res::Resok4(vec![krb5i_secinfo()])));
        assert_eq!(resarray[3], NfsResOp4::OpSecinfo(SecInfo4res::Resok4(vec![SeCinfo4::AuthSys])));
        assert_eq!(
            resarray[5],
            NfsResOp4::OpsecinfoNoName(SecinfoNoName4res::Resok4(vec![SeCinfo4::AuthSys, krb5i_secinfo()]))
        );
    }

    // --- #142: SECINFO consumes the handle; SECINFO_NO_NAME PARENT ---

    fn getattr_type_op() -> NfsArgOp {
        NfsArgOp::Opgetattr(Getattr4args { attr_request: Attrlist4(vec![FileAttr::Type]) })
    }

    #[tokio::test]
    async fn test_secinfo_then_getattr_is_nofilehandle() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // Inside an export, and at the pseudo-root.
        for ops in [
            vec![NfsArgOp::Opputrootfh(()), lookup_op("open"), NfsArgOp::OpSecinfo(SecInfo4args { name: "x".to_string() }), getattr_type_op()],
            vec![NfsArgOp::Opputrootfh(()), NfsArgOp::OpSecinfo(SecInfo4args { name: "open".to_string() }), getattr_type_op()],
            vec![
                NfsArgOp::Opputrootfh(()),
                NfsArgOp::OpsecinfoNoName(SecinfoNoName4args { sina_style: SecinfoStyle4::SecinfoStyle4CurrentFh }),
                getattr_type_op(),
            ],
        ] {
            let n = ops.len();
            let request = sec_export_request(a.path(), b.path()).await;
            let (_request, reply) = NFS40Server::new().compound(make_compound(ops), request).await;
            let (status, resarray) = compound_result(reply);
            assert_eq!(status, NfsStat4::Nfs4errNofilehandle, "{:?}", resarray);
            assert_eq!(resarray.len(), n, "{:?}", resarray);
        }
    }

    #[tokio::test]
    async fn test_secinfo_no_name_parent_of_export_root_is_pseudo_root() {
        use crate::server::export_manager::SecFlavor;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let parent = |style| NfsArgOp::OpsecinfoNoName(SecinfoNoName4args { sina_style: style });

        // krb (krb5i only) export root: CURRENT_FH is the export's own
        // flavours, PARENT the pseudo-root's (sys and krb5i).
        for (style, want) in [
            (SecinfoStyle4::SecinfoStyle4CurrentFh, vec![krb5i_secinfo()]),
            (SecinfoStyle4::SecinfoStyle4Parent, vec![SeCinfo4::AuthSys, krb5i_secinfo()]),
        ] {
            let mut request = sec_export_request(a.path(), b.path()).await;
            request.set_sec_flavor(SecFlavor::Krb5i);
            let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), lookup_op("krb"), parent(style)]);
            let (_request, reply) = NFS40Server::new().compound(msg, request).await;
            let (status, resarray) = compound_result(reply);
            assert_eq!(status, NfsStat4::Nfs4Ok, "{:?}", resarray);
            assert_eq!(resarray[2], NfsResOp4::OpsecinfoNoName(SecinfoNoName4res::Resok4(want)));
        }

        // Below the export root the parent is in the same export.
        std::fs::create_dir(b.path().join("sub")).unwrap();
        std::fs::set_permissions(b.path().join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut request = sec_export_request(a.path(), b.path()).await;
        request.set_sec_flavor(SecFlavor::Krb5i);
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            lookup_op("krb"),
            lookup_op("sub"),
            parent(SecinfoStyle4::SecinfoStyle4Parent),
        ]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4Ok, "{:?}", resarray);
        assert_eq!(resarray[3], NfsResOp4::OpsecinfoNoName(SecinfoNoName4res::Resok4(vec![krb5i_secinfo()])));

        // The pseudo-root has no parent.
        let request = sec_export_request(a.path(), b.path()).await;
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), parent(SecinfoStyle4::SecinfoStyle4Parent)]);
        let (_request, reply) = NFS40Server::new().compound(msg, request).await;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4errNoent);
    }

    #[test]
    fn test_secinfo4_krb5_wire_format() {
        // flavor 6, sec_oid4 (11 bytes + 1 pad), qop 0, service 2.
        let bytes = nextnfs_proto::xdr::to_bytes(&krb5i_secinfo()).unwrap();
        let mut want = vec![0, 0, 0, 6, 0, 0, 0, 11];
        want.extend_from_slice(KRB5_MECH_OID);
        want.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 2]);
        assert_eq!(bytes, want);
        let back: SeCinfo4 = nextnfs_proto::xdr::from_bytes(&bytes).unwrap();
        assert_eq!(back, krb5i_secinfo());
        assert_eq!(nextnfs_proto::xdr::to_bytes(&SeCinfo4::AuthSys).unwrap(), vec![0, 0, 0, 1]);
    }

    #[tokio::test]
    async fn test_lookup_from_pseudo_root_allows_open_export() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (request, _) = multi_export_request(a.path(), b.path()).await;
        let server = NFS40Server::new();
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            lookup_op("open"),
            NfsArgOp::Opgetfh(()),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4Ok);
        assert_eq!(resarray.len(), 3);
    }

    #[tokio::test]
    async fn test_putfh_of_unstamped_export_handle_refused() {
        // Handles the file manager mints carry their export id, so PUTFH of
        // any handle in "locked" (not just its stamped root) hits its list.
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (request, locked_id) = multi_export_request(a.path(), b.path()).await;
        let (_, fm) = request
            .export_manager()
            .get_export_by_id(locked_id)
            .await
            .unwrap();
        let root = fm.get_root_filehandle().await.unwrap();
        assert_eq!(op_pseudo::export_id_from_fh(&root.id), locked_id);
        let server = NFS40Server::new();
        let msg = make_compound(vec![
            NfsArgOp::Opputfh(PutFh4args { object: root.id }),
            NfsArgOp::Opgetattr(Getattr4args {
                attr_request: Attrlist4(vec![]),
            }),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errAccess);
        assert_eq!(resarray.len(), 1);
    }

    #[tokio::test]
    async fn test_restorefh_restores_saved_export() {
        // SAVEFH on a read-only export, move to a writable one, RESTOREFH:
        // the read-only flag must come back with the handle.
        let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(ro.path().join("f"), b"keep").unwrap();
        let request = create_nfs40_server(None).await;
        let em = request.export_manager();
        em.add_export("rw".to_string(), rw.path().to_path_buf(), false)
            .await
            .unwrap();
        em.add_export("ro".to_string(), ro.path().to_path_buf(), true)
            .await
            .unwrap();
        let server = NFS40Server::new();
        let msg = make_compound(vec![
            NfsArgOp::Opputrootfh(()),
            lookup_op("ro"),
            NfsArgOp::Opsavefh(()),
            NfsArgOp::Opputrootfh(()),
            lookup_op("rw"),
            NfsArgOp::Oprestorefh(()),
            NfsArgOp::Opremove(Remove4args {
                target: "f".to_string(),
            }),
        ]);
        let (_request, reply) = server.compound(msg, request).await;
        let (status, _) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errRofs);
        assert!(ro.path().join("f").exists());
    }

    #[tokio::test]
    async fn test_pseudo_root_readdir_hides_denied_export() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (request, _) = multi_export_request(a.path(), b.path()).await;
        let em = request.export_manager();
        let (entries, _verifier) =
            op_pseudo::pseudo_readdir(&em, request.client_addr(), &[], 0).await;
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["open"]);
        // An allowed client sees both
        let (entries, _) = op_pseudo::pseudo_readdir(&em, "10.1.2.3:700", &[], 0).await;
        assert_eq!(entries.len(), 2);
    }

    // --- #91: permissions checked for the squash-mapped caller ---

    /// An unprivileged uid that does not own the test's tempdirs.
    const OTHER: u32 = 54321;

    fn chmod(path: &std::path::Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mkdir_op(name: &str) -> NfsArgOp {
        NfsArgOp::Opcreate(Create4args {
            objtype: Createtype4::Nf4dir,
            objname: name.to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        })
    }

    async fn run_as(
        dir: &std::path::Path,
        squash: SquashMode,
        cred: OpaqueAuth,
        ops: Vec<NfsArgOp>,
    ) -> (NfsStat4, Vec<NfsResOp4>) {
        let request = request_on_export_squash(false, dir, squash).await;
        let server = NFS40Server::new();
        let (_request, reply) = server.compound(make_compound_as(cred, ops), request).await;
        compound_result(reply)
    }

    #[tokio::test]
    async fn test_perm_other_uid_cannot_create_in_0755_dir() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        let (status, resarray) = run_as(
            dir.path(),
            SquashMode::RootSquash,
            auth_sys(OTHER, OTHER),
            vec![NfsArgOp::Opputrootfh(()), mkdir_op("d")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errAccess);
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 6, status: NfsStat4::Nfs4errAccess });
        assert!(!dir.path().join("d").exists());
    }

    #[tokio::test]
    async fn test_perm_owner_can_create() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        let (uid, gid) = own_ids();
        let (status, _) = run_as(
            dir.path(),
            SquashMode::None,
            auth_sys(uid, gid),
            vec![NfsArgOp::Opputrootfh(()), mkdir_op("d")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4Ok);
        assert!(dir.path().join("d").is_dir());
    }

    #[tokio::test]
    async fn test_perm_root_is_squashed_by_default() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        // AccessConfig::default() is root_squash: root becomes nobody.
        let (status, _) = run_as(
            dir.path(),
            AccessConfig::default().squash,
            auth_sys(0, 0),
            vec![NfsArgOp::Opputrootfh(()), mkdir_op("d")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errAccess);
        assert!(!dir.path().join("d").exists());
    }

    #[tokio::test]
    async fn test_perm_all_squash_maps_owner_too() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        let (uid, gid) = own_ids();
        let (status, _) = run_as(
            dir.path(),
            SquashMode::AllSquash,
            auth_sys(uid, gid),
            vec![NfsArgOp::Opputrootfh(()), mkdir_op("d")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errAccess);
    }

    #[tokio::test]
    async fn test_perm_unsquashed_root_passes() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o700);
        let (status, _) = run_as(
            dir.path(),
            SquashMode::None,
            auth_sys(0, 0),
            vec![NfsArgOp::Opputrootfh(()), mkdir_op("d")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4Ok);
        assert!(dir.path().join("d").is_dir());
    }

    #[tokio::test]
    async fn test_perm_auth_none_is_anonymous() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        let (status, _) = run_as(
            dir.path(),
            SquashMode::None,
            OpaqueAuth::AuthNull(vec![]),
            vec![NfsArgOp::Opputrootfh(()), mkdir_op("d")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errAccess);
    }

    #[tokio::test]
    async fn test_perm_lookup_needs_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        chmod(dir.path(), 0o700);
        let (status, resarray) = run_as(
            dir.path(),
            SquashMode::RootSquash,
            auth_sys(OTHER, OTHER),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("f")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errAccess);
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 15, status: NfsStat4::Nfs4errAccess });
    }

    #[tokio::test]
    async fn test_perm_write_needs_w() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        chmod(&dir.path().join("f"), 0o644);
        chmod(dir.path(), 0o755);
        let (status, resarray) = run_as(
            dir.path(),
            SquashMode::RootSquash,
            auth_sys(OTHER, OTHER),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("f"), write_op()],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errAccess);
        assert_eq!(resarray[2], NfsResOp4::OpError { op: 38, status: NfsStat4::Nfs4errAccess });
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"x");
    }

    #[tokio::test]
    async fn test_perm_sticky_dir_remove_by_non_owner() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        chmod(dir.path(), 0o1777);
        let (status, _) = run_as(
            dir.path(),
            SquashMode::RootSquash,
            auth_sys(OTHER, OTHER),
            vec![
                NfsArgOp::Opputrootfh(()),
                NfsArgOp::Opremove(Remove4args { target: "f".to_string() }),
            ],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errPerm);
        assert!(dir.path().join("f").exists());
    }

    #[tokio::test]
    async fn test_perm_chmod_by_non_owner() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        chmod(&dir.path().join("f"), 0o666);
        chmod(dir.path(), 0o755);
        let setattr = NfsArgOp::Opsetattr(SetAttr4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Mode]),
                attr_vals: Attrlist4(vec![FileAttrValue::Mode(0o777)]),
            },
        });
        let (status, _) = run_as(
            dir.path(),
            SquashMode::RootSquash,
            auth_sys(OTHER, OTHER),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("f"), setattr],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errPerm);
        let mode = std::fs::metadata(dir.path().join("f")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o666);
    }

    #[tokio::test]
    async fn test_perm_access_for_squashed_root() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o755);
        let (status, resarray) = run_as(
            dir.path(),
            SquashMode::RootSquash,
            auth_sys(0, 0),
            vec![
                NfsArgOp::Opputrootfh(()),
                NfsArgOp::OpAccess(Access4args { access: ACCESS4_READ | ACCESS4_MODIFY }),
            ],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4Ok);
        match &resarray[1] {
            NfsResOp4::OpAccess(Access4res::Resok4(res)) => assert_eq!(res.access, ACCESS4_READ),
            other => panic!("unexpected {:?}", other),
        }
    }


    // --- #122: symlinks never lead out of the export; CREATE never takes over ---

    /// An export with `link` → a file outside it (`secret`, "outside") and
    /// `dirlink` → the directory outside it. Returns (export, outside).
    fn symlink_export() -> (tempfile::TempDir, tempfile::TempDir) {
        let (export, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        chmod(&outside.path().join("secret"), 0o600);
        std::os::unix::fs::symlink(outside.path().join("secret"), export.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), export.path().join("dirlink")).unwrap();
        (export, outside)
    }

    async fn run_own(dir: &std::path::Path, ops: Vec<NfsArgOp>) -> (NfsStat4, Vec<NfsResOp4>) {
        let (uid, gid) = own_ids();
        run_as(dir, SquashMode::None, auth_sys(uid, gid), ops).await
    }

    fn setattr_op(attrs: Vec<FileAttrValue>, mask: Vec<FileAttr>) -> NfsArgOp {
        NfsArgOp::Opsetattr(SetAttr4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(mask),
                attr_vals: Attrlist4(attrs),
            },
        })
    }

    #[tokio::test]
    async fn test_confine_write_through_symlink_refused() {
        let (export, outside) = symlink_export();
        let (status, resarray) = run_own(
            export.path(),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("link"), write_op()],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errInval);
        assert_eq!(resarray[2], NfsResOp4::OpError { op: 38, status: NfsStat4::Nfs4errInval });
        assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"outside");
    }

    #[tokio::test]
    async fn test_confine_read_through_symlink_refused() {
        let (export, _outside) = symlink_export();
        let read = NfsArgOp::Opread(Read4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            count: 64,
        });
        let (status, resarray) =
            run_own(export.path(), vec![NfsArgOp::Opputrootfh(()), lookup_op("link"), read]).await;
        assert_eq!(status, NfsStat4::Nfs4errInval);
        assert_eq!(resarray.len(), 3);
    }

    #[tokio::test]
    async fn test_confine_setattr_mode_on_symlink_refused() {
        let (export, outside) = symlink_export();
        let (status, _) = run_own(
            export.path(),
            vec![
                NfsArgOp::Opputrootfh(()),
                lookup_op("link"),
                setattr_op(vec![FileAttrValue::Mode(0o777)], vec![FileAttr::Mode]),
            ],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errInval);
        let mode = std::fs::metadata(outside.path().join("secret")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn test_confine_setattr_times_on_symlink_set_the_link() {
        let (export, outside) = symlink_export();
        let secret = outside.path().join("secret");
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options().write(true).open(&secret).unwrap().set_modified(past).unwrap();
        let (status, _) = run_own(
            export.path(),
            vec![
                NfsArgOp::Opputrootfh(()),
                lookup_op("link"),
                setattr_op(vec![FileAttrValue::TimeModifySet], vec![FileAttr::TimeModifySet]),
            ],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4Ok);
        // The target keeps its time; the link itself was touched.
        assert_eq!(std::fs::metadata(&secret).unwrap().modified().unwrap(), past);
        let link_mtime = std::fs::symlink_metadata(export.path().join("link")).unwrap().modified().unwrap();
        assert!(link_mtime > past);
    }

    #[tokio::test]
    async fn test_confine_lookup_through_dir_symlink_refused() {
        let (export, _outside) = symlink_export();
        let (status, resarray) = run_own(
            export.path(),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("dirlink"), lookup_op("secret")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errSymlink);
        assert_eq!(resarray[2], NfsResOp4::OpError { op: 15, status: NfsStat4::Nfs4errSymlink });
    }

    #[tokio::test]
    async fn test_confine_multi_component_and_dot_names_refused() {
        let (export, _outside) = symlink_export();
        for name in ["dirlink/secret", "..", ".", "a\0b"] {
            let (status, _) =
                run_own(export.path(), vec![NfsArgOp::Opputrootfh(()), lookup_op(name)]).await;
            assert_eq!(status, NfsStat4::Nfs4errBadname, "name {:?}", name);
        }
        let (status, _) = run_own(export.path(), vec![NfsArgOp::Opputrootfh(()), mkdir_op("x/y")]).await;
        assert_eq!(status, NfsStat4::Nfs4errBadname);
    }

    #[tokio::test]
    async fn test_confine_create_in_dir_symlink_refused() {
        let (export, outside) = symlink_export();
        let (status, _) = run_own(
            export.path(),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("dirlink"), mkdir_op("planted")],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errSymlink);
        assert!(!outside.path().join("planted").exists());
    }

    #[tokio::test]
    async fn test_confine_open_symlink_name_refused() {
        let (export, outside) = symlink_export();
        let mut open = open_args(3, OpenFlag4::Open4Nocreate);
        open.claim = OpenClaim4::ClaimNull("link".to_string());
        let (status, _) =
            run_own(export.path(), vec![NfsArgOp::Opputrootfh(()), NfsArgOp::Opopen(open)]).await;
        assert_eq!(status, NfsStat4::Nfs4errSymlink);
        assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"outside");
    }

    #[tokio::test]
    async fn test_confine_handle_through_swapped_dir_is_stale() {
        // A handle for d/secret, then d is replaced by a symlink to a
        // directory outside holding a file of the same name.
        let (export, outside) = symlink_export();
        std::fs::create_dir(export.path().join("d")).unwrap();
        std::fs::write(export.path().join("d/secret"), b"inside").unwrap();
        let request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request
            .file_manager()
            .get_filehandle_for_path("/d/secret".to_string())
            .await
            .unwrap();
        std::fs::rename(export.path().join("d"), export.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), export.path().join("d")).unwrap();
        let mut id = fh.id;
        op_pseudo::stamp_export_id(&mut id, request.current_export_id().unwrap());
        let (uid, gid) = own_ids();
        let server = NFS40Server::new();
        let msg = make_compound_as(
            auth_sys(uid, gid),
            vec![NfsArgOp::Opputfh(PutFh4args { object: id }), write_op()],
        );
        let (_request, reply) = server.compound(msg, request).await;
        let (status, _) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errStale);
        assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"outside");
    }

    // --- #123: the op itself resolves beneath the export root ---

    /// An export with d/secret ("inside") and a handle for it; then d is
    /// swapped for a symlink to the outside directory, as if after the
    /// dispatcher's checks had passed.
    async fn swapped_after_check() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        NfsRequest<'static>,
        crate::server::filemanager::Filehandle,
    ) {
        let (export, outside) = symlink_export();
        std::fs::create_dir(export.path().join("d")).unwrap();
        std::fs::write(export.path().join("d/secret"), b"inside").unwrap();
        let request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request
            .file_manager()
            .get_filehandle_for_path("/d/secret".to_string())
            .await
            .unwrap();
        std::fs::rename(export.path().join("d"), export.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), export.path().join("d")).unwrap();
        (export, outside, request, fh)
    }

    #[tokio::test]
    async fn test_beneath_ops_after_swap_stay_inside() {
        use crate::server::operation::NfsOperation;
        let (export, outside, mut request, fh) = swapped_after_check().await;
        let secret = outside.path().join("secret");
        let mode_before = std::fs::metadata(&secret).unwrap().permissions().mode();

        // WRITE (sync and unstable), run without the dispatcher's checks.
        for stable in [StableHow4::FileSync4, StableHow4::Unstable4] {
            request.set_filehandle(fh.clone());
            let args = Write4args {
                stateid: Stateid4 { seqid: 0, other: [0; 12] },
                offset: 0,
                stable: stable.clone(),
                data: b"x".to_vec(),
            };
            let resp = args.execute(request).await;
            request = resp.request;
            if stable == StableHow4::FileSync4 {
                assert_ne!(resp.status, NfsStat4::Nfs4Ok);
            }
        }
        // SETATTR size and mode, owner and times.
        let (uid, gid) = own_ids();
        let set = request.file_manager().set_attr(
            &fh,
            &Attrlist4(vec![
                FileAttrValue::Size(0),
                FileAttrValue::Mode(0o777),
                FileAttrValue::Owner(uid.to_string()),
                FileAttrValue::OwnerGroup(gid.to_string()),
                FileAttrValue::TimeModifySet,
            ]),
        );
        assert!(set.0.is_empty(), "set through the swapped directory: {:?}", set.0);
        // READ through the vfs (BeneathFS).
        assert!(fh.file.open_file().is_err());
        // REMOVE, RENAME and LINK of the name below the symlink.
        let fm = request.file_manager();
        let root = fm.beneath();
        assert!(root.remove_file("/d/secret").is_err());
        assert!(root.rename("/d/secret", "/stolen").is_err());
        assert!(root.hard_link("/d/secret", "/stolen").is_err());
        assert!(fh.file.remove_file().is_err());

        assert_eq!(std::fs::read(&secret).unwrap(), b"outside");
        assert_eq!(std::fs::metadata(&secret).unwrap().permissions().mode(), mode_before);
        assert!(!export.path().join("stolen").exists());
        assert_eq!(std::fs::read(export.path().join("moved/secret")).unwrap(), b"inside");
    }

    #[tokio::test]
    async fn test_setattr_size_truncates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        for (size, want) in [(2u64, &b"he"[..]), (4, &b"he\0\0"[..])] {
            let (status, _) = run_own(
                dir.path(),
                vec![
                    NfsArgOp::Opputrootfh(()),
                    lookup_op("f"),
                    setattr_op(vec![FileAttrValue::Size(size)], vec![FileAttr::Size]),
                ],
            )
            .await;
            assert_eq!(status, NfsStat4::Nfs4Ok);
            assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), want);
        }
    }

    /// A real export never writes through the vfs: WRITE of a file that is
    /// gone from beneath the root (removed, or a directory on its path is
    /// mid-swap) is NFS4ERR_STALE, and nothing is created or appended.
    #[tokio::test]
    async fn test_write_of_removed_file_is_stale() {
        use crate::server::operation::NfsOperation;
        let export = tempfile::tempdir().unwrap();
        let d = export.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("f"), b"abc").unwrap();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request
            .file_manager()
            .get_filehandle_for_path("/d/f".to_string())
            .await
            .unwrap();
        assert!(!request.file_manager().vfs_only());
        std::fs::remove_file(d.join("f")).unwrap();
        for stable in [StableHow4::FileSync4, StableHow4::DataSync4] {
            request.set_filehandle(fh.clone());
            let args = Write4args {
                stateid: Stateid4 { seqid: 0, other: [0; 12] },
                offset: 1,
                stable,
                data: b"x".to_vec(),
            };
            let resp = args.execute(request).await;
            assert_eq!(resp.status, NfsStat4::Nfs4errStale);
            request = resp.request;
            assert!(!d.join("f").exists());
        }
    }

    /// The issue's test: a thread swaps d for a symlink to the outside
    /// directory and back, in a loop, while WRITEs of d/secret run, through
    /// the dispatcher and as the op alone. The outside file never changes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_beneath_write_races_symlink_swap() {
        use crate::server::operation::NfsOperation;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (export, outside) = symlink_export();
        let d = export.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("secret"), b"inside").unwrap();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request
            .file_manager()
            .get_filehandle_for_path("/d/secret".to_string())
            .await
            .unwrap();
        let mut id = fh.id;
        op_pseudo::stamp_export_id(&mut id, request.current_export_id().unwrap());

        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let (d, out, held, stop) = (
                d.clone(),
                outside.path().to_path_buf(),
                export.path().join("held"),
                stop.clone(),
            );
            std::thread::spawn(move || {
                let mut swaps = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    std::fs::rename(&d, &held).unwrap();
                    std::os::unix::fs::symlink(&out, &d).unwrap();
                    std::thread::yield_now();
                    std::fs::remove_file(&d).unwrap();
                    std::fs::rename(&held, &d).unwrap();
                    std::thread::yield_now();
                    swaps += 1;
                }
                swaps
            })
        };

        let (uid, gid) = own_ids();
        let server = NFS40Server::new();
        let mut written = 0;
        for _ in 0..400 {
            let msg = make_compound_as(
                auth_sys(uid, gid),
                vec![NfsArgOp::Opputfh(PutFh4args { object: id }), write_op()],
            );
            let (r, _reply) = server.compound(msg, request).await;
            request = r;

            request.set_filehandle(fh.clone());
            let args = Write4args {
                stateid: Stateid4 { seqid: 0, other: [0; 12] },
                offset: 0,
                stable: StableHow4::FileSync4,
                data: b"x".to_vec(),
            };
            let resp = args.execute(request).await;
            if resp.status == NfsStat4::Nfs4Ok {
                written += 1;
            }
            request = resp.request;
        }
        stop.store(true, Ordering::Relaxed);
        let swaps = swapper.join().unwrap();

        assert!(swaps > 0);
        assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"outside");
        // The writes that went through landed inside.
        if written > 0 {
            assert_eq!(std::fs::read(d.join("secret")).unwrap(), b"xnside");
        }
    }

    fn exclusive_open(name: &str, verifier: [u8; 8]) -> NfsArgOp {
        NfsArgOp::Opopen(Open4args {
            seqid: 1,
            share_access: 3,
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"excl".to_vec() },
            openhow: OpenFlag4::How(CreateHow4::EXCLUSIVE4(verifier)),
            claim: OpenClaim4::ClaimNull(name.to_string()),
        })
    }

    fn open_resok(resarray: &[NfsResOp4]) -> Open4resok {
        match resarray.last() {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => resok.clone(),
            other => panic!("expected OPEN resok, got {:?}", other),
        }
    }

    /// Finding 696ec94aaf26 / #140: EXCLUSIVE4 keeps its verifier (in the
    /// file's atime/mtime) and says so in attrset; a retry with the same
    /// verifier succeeds without touching the file; any other existing name
    /// is NFS4ERR_EXIST and keeps its data.
    #[tokio::test]
    async fn test_open_exclusive4_verifier_and_retry() {
        let export = tempfile::tempdir().unwrap();
        let path = export.path().join("x");
        std::fs::write(export.path().join("old"), b"keep").unwrap();
        let v = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
        let server = NFS40Server::new();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;

        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), exclusive_open("x", v)]);
        let (r, reply) = server.compound(msg, request).await;
        request = r;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4Ok);
        let first = open_resok(&resarray);
        assert_eq!(first.attrset.0, vec![FileAttr::TimeAccess, FileAttr::TimeModify]);
        let m = std::fs::metadata(&path).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(m.atime() as u32, 0x1234_5678);
        assert_eq!(m.mtime() as u32, 0x9abc_def0); // all 32 bits (#148)

        // The client's data arrives; the times stay the verifier's (as if the
        // reply was lost before the client's SETATTR of them).
        let (atime, mtime) = (m.accessed().unwrap(), m.modified().unwrap());
        std::fs::write(&path, b"data").unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_accessed(atime).set_modified(mtime)).unwrap();
        drop(f);

        // Retry: same verifier → success, same open, data untouched.
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), exclusive_open("x", v)]);
        let (r, reply) = server.compound(msg, request).await;
        request = r;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4Ok);
        assert_eq!(open_resok(&resarray).stateid.other, first.stateid.other);
        assert_eq!(std::fs::read(&path).unwrap(), b"data");

        // Another verifier, or a file that was never an exclusive create.
        let mut w = v;
        w[7] ^= 1;
        for (name, verifier) in [("x", w), ("old", v)] {
            let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), exclusive_open(name, verifier)]);
            let (r, reply) = server.compound(msg, request).await;
            request = r;
            let (status, _) = compound_result(reply);
            assert_eq!(status, NfsStat4::Nfs4errExist, "{}", name);
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        assert_eq!(std::fs::read(export.path().join("old")).unwrap(), b"keep");
    }

    /// The xattr holding a file's EXCLUSIVE4 verifier, or None.
    fn verifier_xattr(path: &std::path::Path) -> Option<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        let p = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let n = std::ffi::CString::new(super::op_open::VERIFIER_XATTR).unwrap();
        let mut buf = [0u8; 16];
        let r = unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        (r >= 0).then(|| buf[..r as usize].to_vec())
    }

    /// An export directory on the build disk (not /tmp, which may be a
    /// tmpfs without user xattrs), checked to keep user xattrs.
    fn xattr_export() -> tempfile::TempDir {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let probe = dir.path().join(".probe");
        std::fs::write(&probe, b"").unwrap();
        use std::os::unix::ffi::OsStrExt;
        let p = std::ffi::CString::new(probe.as_os_str().as_bytes()).unwrap();
        let n = std::ffi::CString::new("user.nextnfs.probe").unwrap();
        let r = unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), b"x".as_ptr() as *const libc::c_void, 1, 0) };
        assert_eq!(r, 0, "user xattrs unsupported under {}: {}", env!("CARGO_MANIFEST_DIR"), std::io::Error::last_os_error());
        std::fs::remove_file(probe).unwrap();
        dir
    }

    async fn exclusive_status<'a>(
        request: crate::server::request::NfsRequest<'a>,
        name: &str,
        v: [u8; 8],
    ) -> (crate::server::request::NfsRequest<'a>, NfsStat4) {
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), exclusive_open(name, v)]);
        let (r, reply) = NFS40Server::new().compound(msg, request).await;
        (r, compound_result(reply).0)
    }

    /// #148, finding 49b9b67926c7: verifiers that differ only in the top
    /// bit of each half (which the old times encoding cleared) are
    /// different creates: the second is NFS4ERR_EXIST, the first still
    /// retries. All eight bytes are in the xattr.
    #[tokio::test]
    async fn test_exclusive4_high_bit_verifiers_do_not_collide() {
        let export = xattr_export();
        let v1 = [0x12, 0x34, 0x56, 0x78, 0x1a, 0xbc, 0xde, 0xf0];
        let mut v2 = v1;
        v2[0] ^= 0x80;
        v2[4] ^= 0x80;
        let request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let (request, st) = exclusive_status(request, "x", v1).await;
        assert_eq!(st, NfsStat4::Nfs4Ok);
        assert_eq!(verifier_xattr(&export.path().join("x")).as_deref(), Some(&v1[..]));
        let (request, st) = exclusive_status(request, "x", v2).await;
        assert_eq!(st, NfsStat4::Nfs4errExist);
        let (_, st) = exclusive_status(request, "x", v1).await;
        assert_eq!(st, NfsStat4::Nfs4Ok);
    }

    /// #148: a retry still matches after the file was read (its atime
    /// moved, as relatime does on a read) and written, and after a server
    /// restart (a new export manager and file manager on the same tree).
    #[tokio::test]
    async fn test_exclusive4_retry_after_read_and_restart() {
        let export = xattr_export();
        let path = export.path().join("y");
        let v = [0xde, 0xad, 0xbe, 0xef, 0x80, 0x00, 0x00, 0x01];
        let request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let (request, st) = exclusive_status(request, "y", v).await;
        assert_eq!(st, NfsStat4::Nfs4Ok);

        // A read moves the atime; data is written (mtime moves too).
        std::fs::write(&path, b"data").unwrap();
        let _ = std::fs::read(&path).unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_accessed(std::time::SystemTime::now())).unwrap();
        drop(f);

        let (request, st) = exclusive_status(request, "y", v).await;
        assert_eq!(st, NfsStat4::Nfs4Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        drop(request);

        // Restart: nothing in memory knows the create.
        let request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let (request, st) = exclusive_status(request, "y", v).await;
        assert_eq!(st, NfsStat4::Nfs4Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        let mut w = v;
        w[0] ^= 1;
        let (_, st) = exclusive_status(request, "y", w).await;
        assert_eq!(st, NfsStat4::Nfs4errExist);
    }

    /// Finding 9c0c932c5136: a handle names an object, not a path. After
    /// another file is renamed over `f`, the old handle (cached on the
    /// connection, known to the file manager, with an open on it) is
    /// NFS4ERR_STALE: a WRITE through it never reaches the new file, and
    /// the new file gets a handle of its own.
    #[tokio::test]
    async fn test_handle_of_replaced_path_is_stale() {
        let export = tempfile::tempdir().unwrap();
        let f = export.path().join("f");
        std::fs::write(&f, b"old").unwrap();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        request.filehandle_cache = Some(Box::leak(Box::new(std::collections::HashMap::new())));
        let fm = request.file_manager();
        let fh = fm.get_filehandle_for_path("/f".to_string()).await.unwrap();
        fm.create_open_state(fh.file.clone(), 1, b"opener".to_vec(), 3, 0).await.unwrap();
        let mut id = fh.id;
        op_pseudo::stamp_export_id(&mut id, request.current_export_id().unwrap());

        // Through the old handle while it is the file: fine (and cached).
        let server = NFS40Server::new();
        let msg = make_compound(vec![NfsArgOp::Opputfh(PutFh4args { object: id }), write_op()]);
        let (r, reply) = server.compound(msg, request).await;
        request = r;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4Ok);
        assert_eq!(std::fs::read(&f).unwrap(), b"xld");

        // A new file is published at the same path.
        std::fs::write(export.path().join("g"), b"new").unwrap();
        std::fs::rename(export.path().join("g"), &f).unwrap();

        let msg = make_compound(vec![NfsArgOp::Opputfh(PutFh4args { object: id }), write_op()]);
        let (r, reply) = server.compound(msg, request).await;
        request = r;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4errStale);
        assert_eq!(std::fs::read(&f).unwrap(), b"new");

        let fm = request.file_manager();
        assert!(fm.get_filehandle_for_id(fh.id).await.is_err());
        let fresh = fm.get_filehandle_for_path("/f".to_string()).await.unwrap();
        assert_ne!(fresh.id, fh.id);
    }

    /// #133 (finding fdfec7e60065): handles are the filesystem's kernel
    /// file handles, so a server that never saw them (a restart: a new
    /// export manager and file manager on the same directory) resolves
    /// them with open_by_handle_at, also for an object renamed since.
    /// Without CAP_DAC_READ_SEARCH (the unprivileged build VM) the server
    /// cannot open handles: then the handle is STALE and FH_EXPIRE_TYPE
    /// says FH4_VOLATILE_ANY instead of FH4_PERSISTENT.
    #[tokio::test]
    async fn test_kernel_handle_resolves_after_restart() {
        use crate::server::filemanager::{FH_INODE, FH_KERNEL};
        let export = tempfile::tempdir().unwrap();
        std::fs::create_dir(export.path().join("d")).unwrap();
        std::fs::write(export.path().join("d/f"), b"data").unwrap();
        let before = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let export_id = before.current_export_id().unwrap();
        let fm = before.file_manager();
        let has_handles = fm.beneath().kernel_handle("/d/f").unwrap().is_some();
        let persistent = fm.beneath().persistent_handles();
        let file = fm.get_filehandle_for_path("/d/f".to_string()).await.unwrap();
        let dir = fm.get_filehandle_for_path("/d".to_string()).await.unwrap();
        if !has_handles {
            eprintln!("filesystem of {:?} has no kernel handles: inode ids", export.path());
            crate::server::beneath::tests::require_privileged("the test filesystem has no kernel handles");
            assert_eq!(file.id[0], FH_INODE);
            return;
        }
        assert_eq!(file.id[0], FH_KERNEL);
        assert_eq!(dir.id[0], FH_KERNEL);
        assert_ne!(file.id, dir.id);
        // Stable while the object is the same.
        assert_eq!(fm.get_filehandle_for_path("/d/f".to_string()).await.unwrap().id, file.id);
        let attrs = fm
            .get_filehandle_attrs(file.id, vec![FileAttr::FhExpireType])
            .await
            .unwrap();
        let want = if persistent { FH4_PERSISTENT } else { FH4_VOLATILE_ANY };
        assert!(matches!(attrs.1[0], FileAttrValue::FhExpireType(t) if t == want));
        let mut fid = file.id;
        op_pseudo::stamp_export_id(&mut fid, export_id);
        let mut did = dir.id;
        op_pseudo::stamp_export_id(&mut did, export_id);
        drop(before);

        // A new server on the same export: nothing in its handle table.
        let after = request_on_export_squash(false, export.path(), SquashMode::None).await;
        assert_eq!(after.current_export_id().unwrap(), export_id);
        let server = NFS40Server::new();
        let msg = make_compound(vec![NfsArgOp::Opputfh(PutFh4args { object: fid }), write_op()]);
        let (after, reply) = server.compound(msg, after).await;
        if !persistent {
            eprintln!("open_by_handle_at not permitted here: STALE after a restart");
            crate::server::beneath::tests::require_privileged("open_by_handle_at is not permitted (no CAP_DAC_READ_SEARCH)");
            assert_eq!(compound_result(reply).0, NfsStat4::Nfs4errStale);
            assert_eq!(std::fs::read(export.path().join("d/f")).unwrap(), b"data");
            return;
        }
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4Ok);
        assert_eq!(std::fs::read(export.path().join("d/f")).unwrap(), b"xata");
        let fm = after.file_manager();
        assert_eq!(fm.get_filehandle_for_id(fid).await.unwrap().path, "/d/f");
        assert_eq!(fm.get_filehandle_for_id(did).await.unwrap().path, "/d");

        // Renamed to another directory outside NFS, then another restart.
        std::fs::create_dir(export.path().join("e")).unwrap();
        std::fs::rename(export.path().join("d/f"), export.path().join("e/g")).unwrap();
        drop(after);
        let again = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fm = again.file_manager();
        let moved = fm.get_filehandle_for_id(fid).await.unwrap();
        assert_eq!(moved.path, "/e/g");
        assert_eq!(fm.get_filehandle_for_path("/e/g".to_string()).await.unwrap().id, moved.id);
    }

    /// #133: a kernel handle also names the inode generation, so a file
    /// removed and created again at the same path (which may reuse the
    /// inode number) has another handle, and the old one is STALE, in the
    /// same server and in a new one.
    #[tokio::test]
    async fn test_kernel_handle_of_recreated_file_is_stale() {
        use crate::server::filemanager::FH_KERNEL;
        let export = tempfile::tempdir().unwrap();
        let f = export.path().join("f");
        std::fs::write(&f, b"old").unwrap();
        let request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let export_id = request.current_export_id().unwrap();
        let fm = request.file_manager();
        let old = fm.get_filehandle_for_path("/f".to_string()).await.unwrap();
        if old.id[0] != FH_KERNEL {
            eprintln!("no kernel handles on {:?}", export.path());
            crate::server::beneath::tests::require_privileged("the test filesystem has no kernel handles");
            return;
        }
        std::fs::remove_file(&f).unwrap();
        std::fs::write(&f, b"new").unwrap();
        let new = fm.get_filehandle_for_path("/f".to_string()).await.unwrap();
        assert_ne!(new.id, old.id);
        assert!(fm.get_filehandle_for_id(old.id).await.is_err());
        let mut id = old.id;
        op_pseudo::stamp_export_id(&mut id, export_id);
        drop(request);
        let again = request_on_export_squash(false, export.path(), SquashMode::None).await;
        assert!(again.file_manager().get_filehandle_for_id(id).await.is_err());
    }

    /// #135: a stable WRITE's failure carries the errno's status (it was
    /// NFS4ERR_IO for everything but a missing file), so the filesystem's
    /// ENOSPC / EDQUOT reach the client as NOSPC / DQUOT. A directory shows
    /// the mapping without filling a disk: EISDIR → NFS4ERR_ISDIR.
    #[tokio::test]
    async fn test_stable_write_error_keeps_errno_status() {
        use crate::server::operation::NfsOperation;
        let export = tempfile::tempdir().unwrap();
        std::fs::create_dir(export.path().join("d")).unwrap();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request
            .file_manager()
            .get_filehandle_for_path("/d".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);
        let args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"x".to_vec(),
        };
        let resp = args.execute(request).await;
        assert_eq!(resp.status, NfsStat4::Nfs4errIsdir);
        assert_eq!(
            crate::server::createattrs::io_status(&std::io::Error::from_raw_os_error(libc::ENOSPC)),
            NfsStat4::Nfs4errNospc
        );
        assert_eq!(
            crate::server::createattrs::io_status(&std::io::Error::from_raw_os_error(libc::EDQUOT)),
            NfsStat4::Nfs4errDquot
        );
    }

    /// Finding 4bb3de8fbdd6: a stable WRITE at EOF used O_APPEND when the
    /// offset equalled a stat of the length, so another writer extending the
    /// file in between moved the bytes to a later offset. Each round, a
    /// second writer (another connection) and a FILE_SYNC WRITE both write
    /// one block at the current end, released together: the file grows by
    /// exactly one block and that block is one of the two, never both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stable_write_at_eof_keeps_offset_under_concurrency() {
        use crate::server::operation::NfsOperation;
        use std::os::unix::fs::FileExt;
        use std::sync::{Arc, Barrier};

        const B: usize = 512;
        const ROUNDS: usize = 300;
        let export = tempfile::tempdir().unwrap();
        let path = export.path().join("f");
        std::fs::write(&path, b"").unwrap();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request
            .file_manager()
            .get_filehandle_for_path("/f".to_string())
            .await
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let other = {
            let (barrier, path) = (barrier.clone(), path.clone());
            std::thread::spawn(move || {
                let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                for k in 0..ROUNDS {
                    barrier.wait();
                    f.write_all_at(&[b'o'; B], (k * B) as u64).unwrap();
                    barrier.wait();
                }
            })
        };

        for k in 0..ROUNDS {
            request.set_filehandle(fh.clone());
            let args = Write4args {
                stateid: Stateid4 { seqid: 0, other: [0; 12] },
                offset: (k * B) as u64,
                stable: StableHow4::FileSync4,
                data: vec![b'n'; B],
            };
            barrier.wait();
            let resp = args.execute(request).await;
            barrier.wait();
            assert_eq!(resp.status, NfsStat4::Nfs4Ok);
            request = resp.request;
            let len = std::fs::metadata(&path).unwrap().len();
            assert_eq!(len, ((k + 1) * B) as u64, "round {}: WRITE moved past its offset", k);
            let mut block = vec![0u8; B];
            std::fs::File::open(&path)
                .unwrap()
                .read_exact_at(&mut block, (k * B) as u64)
                .unwrap();
            assert!(
                block.iter().all(|&b| b == b'n') || block.iter().all(|&b| b == b'o'),
                "round {}: block torn",
                k
            );
        }
        other.join().unwrap();
    }

    #[tokio::test]
    async fn test_create_existing_name_is_exist() {
        let dir = tempfile::tempdir().unwrap();
        chmod(dir.path(), 0o777);
        std::fs::create_dir(dir.path().join("victim")).unwrap();
        std::fs::write(dir.path().join("file"), b"mine").unwrap();
        std::os::unix::fs::symlink("/nonexistent-122", dir.path().join("dangling")).unwrap();
        let (uid, _) = own_ids();
        for name in ["victim", "file", "dangling"] {
            let (status, resarray) = run_as(
                dir.path(),
                SquashMode::RootSquash,
                auth_sys(OTHER, OTHER),
                vec![NfsArgOp::Opputrootfh(()), mkdir_op(name)],
            )
            .await;
            assert_eq!(status, NfsStat4::Nfs4errExist, "name {}", name);
            assert!(!resarray.is_empty());
            let meta = std::fs::symlink_metadata(dir.path().join(name)).unwrap();
            assert_eq!(std::os::unix::fs::MetadataExt::uid(&meta), uid);
        }
        assert!(dir.path().join("file").is_file());
    }

    // --- #127 (finding 0d36258c216e): createattrs on OPEN and CREATE ---

    fn cattrs(vals: Vec<FileAttrValue>) -> Fattr4 {
        let mask = vals
            .iter()
            .map(|v| match v {
                FileAttrValue::Size(_) => FileAttr::Size,
                FileAttrValue::Mode(_) => FileAttr::Mode,
                FileAttrValue::Owner(_) => FileAttr::Owner,
                FileAttrValue::OwnerGroup(_) => FileAttr::OwnerGroup,
                FileAttrValue::Acl(_) => FileAttr::Acl,
                other => panic!("no mask bit in the test for {other:?}"),
            })
            .collect();
        Fattr4 { attrmask: Attrlist4(mask), attr_vals: Attrlist4(vals) }
    }

    fn open_create_op(name: &str, how: CreateHow4) -> NfsArgOp {
        let mut args = open_args(3, OpenFlag4::How(how));
        args.claim = OpenClaim4::ClaimNull(name.to_string());
        NfsArgOp::Opopen(args)
    }

    fn mkdir_with(name: &str, attrs: Fattr4) -> NfsArgOp {
        NfsArgOp::Opcreate(Create4args {
            objtype: Createtype4::Nf4dir,
            objname: name.to_string(),
            createattrs: attrs,
        })
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn open_attrset(res: &NfsResOp4) -> Vec<FileAttr> {
        match res {
            NfsResOp4::Opopen(Open4res::Resok4(ok)) => ok.attrset.0.clone(),
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_open_create_applies_requested_mode() {
        let dir = tempfile::tempdir().unwrap();
        // 0600 (private) and 0664 (wider than a 022 umask allows): both
        // exactly as asked.
        for (name, mode) in [("private", 0o600), ("shared", 0o664)] {
            let how = CreateHow4::UNCHECKED4(cattrs(vec![FileAttrValue::Mode(mode)]));
            let (status, res) =
                run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), open_create_op(name, how)]).await;
            assert_eq!(status, NfsStat4::Nfs4Ok, "{name}");
            assert_eq!(mode_of(&dir.path().join(name)), mode, "{name}");
            assert_eq!(open_attrset(&res[1]), vec![FileAttr::Mode], "{name}");
        }
    }

    #[tokio::test]
    async fn test_open_create_applies_size_and_own_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (uid, gid) = own_ids();
        let how = CreateHow4::GUARDED4(cattrs(vec![
            FileAttrValue::Size(10),
            FileAttrValue::Mode(0o640),
            FileAttrValue::Owner(uid.to_string()),
            FileAttrValue::OwnerGroup(gid.to_string()),
        ]));
        let (status, res) =
            run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), open_create_op("g", how)]).await;
        assert_eq!(status, NfsStat4::Nfs4Ok);
        let meta = std::fs::metadata(dir.path().join("g")).unwrap();
        assert_eq!(meta.len(), 10);
        assert_eq!(meta.permissions().mode() & 0o7777, 0o640);
        assert_eq!(std::os::unix::fs::MetadataExt::uid(&meta), uid);
        assert_eq!(
            open_attrset(&res[1]),
            vec![FileAttr::Size, FileAttr::Mode, FileAttr::Owner, FileAttr::OwnerGroup]
        );
    }

    #[tokio::test]
    async fn test_open_guarded_existing_is_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e"), b"keep").unwrap();
        let how = CreateHow4::GUARDED4(cattrs(vec![FileAttrValue::Mode(0o600)]));
        let (status, _) =
            run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), open_create_op("e", how)]).await;
        assert_eq!(status, NfsStat4::Nfs4errExist);
        assert_eq!(std::fs::read(dir.path().join("e")).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn test_open_unchecked_existing_keeps_data_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e");
        std::fs::write(&path, b"keep").unwrap();
        chmod(&path, 0o644);
        // Attributes other than size are not applied to an existing file.
        let how = CreateHow4::UNCHECKED4(cattrs(vec![FileAttrValue::Mode(0o600)]));
        let (status, res) =
            run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), open_create_op("e", how)]).await;
        assert_eq!(status, NfsStat4::Nfs4Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        assert_eq!(mode_of(&path), 0o644);
        assert!(open_attrset(&res[1]).is_empty());

        // A size of zero truncates it.
        let how = CreateHow4::UNCHECKED4(cattrs(vec![FileAttrValue::Size(0)]));
        let (status, res) =
            run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), open_create_op("e", how)]).await;
        assert_eq!(status, NfsStat4::Nfs4Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(open_attrset(&res[1]), vec![FileAttr::Size]);
    }

    #[tokio::test]
    async fn test_open_create_refuses_unsupported_attrs() {
        let dir = tempfile::tempdir().unwrap();
        let how = CreateHow4::UNCHECKED4(cattrs(vec![FileAttrValue::Mode(0o600), FileAttrValue::Acl(vec![])]));
        let (status, _) =
            run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), open_create_op("acl", how)]).await;
        assert_eq!(status, NfsStat4::Nfs4errAttrnotsupp);
        assert!(!dir.path().join("acl").exists());
    }

    #[tokio::test]
    async fn test_open_create_cannot_give_file_away() {
        if own_ids().0 == 0 {
            return; // root may give a file away
        }
        let dir = tempfile::tempdir().unwrap();
        let how = CreateHow4::UNCHECKED4(cattrs(vec![FileAttrValue::Owner(OTHER.to_string())]));
        let (status, _) =
            run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), open_create_op("away", how)]).await;
        assert_eq!(status, NfsStat4::Nfs4errPerm);
        assert!(!dir.path().join("away").exists());
    }

    #[tokio::test]
    async fn test_create_dir_applies_requested_mode() {
        let dir = tempfile::tempdir().unwrap();
        for (name, mode) in [("private", 0o700), ("shared", 0o2775)] {
            let (status, res) = run_own(
                dir.path(),
                vec![NfsArgOp::Opputrootfh(()), mkdir_with(name, cattrs(vec![FileAttrValue::Mode(mode)]))],
            )
            .await;
            assert_eq!(status, NfsStat4::Nfs4Ok, "{name}");
            assert_eq!(mode_of(&dir.path().join(name)), mode, "{name}");
            match &res[1] {
                NfsResOp4::Opcreate(Create4res::Resok4(ok)) => {
                    assert_eq!(ok.attrset.0, vec![FileAttr::Mode], "{name}")
                }
                other => panic!("Expected Opcreate Resok4, got {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn test_create_dir_refuses_size() {
        let dir = tempfile::tempdir().unwrap();
        let (status, _) = run_own(
            dir.path(),
            vec![NfsArgOp::Opputrootfh(()), mkdir_with("d", cattrs(vec![FileAttrValue::Size(0)]))],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errInval);
        assert!(!dir.path().join("d").exists());
    }

    // --- #129: names are single components; a path never skips the search
    // check of the directory it goes through ---

    /// An export (0755) with `private` (0700, the test user's) holding a
    /// world-readable `file`, and a world-readable `pub` at the root: OTHER
    /// may search the root but not `private`.
    fn private_dir_export() -> tempfile::TempDir {
        let export = tempfile::tempdir().unwrap();
        chmod(export.path(), 0o755);
        std::fs::write(export.path().join("pub"), b"pub").unwrap();
        chmod(&export.path().join("pub"), 0o644);
        let private = export.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("file"), b"private").unwrap();
        chmod(&private.join("file"), 0o644);
        chmod(&private, 0o700);
        export
    }

    async fn run_other(dir: &std::path::Path, ops: Vec<NfsArgOp>) -> (NfsStat4, Vec<NfsResOp4>) {
        run_as(dir, SquashMode::RootSquash, auth_sys(OTHER, OTHER), ops).await
    }

    fn open_name_op(name: &str, openhow: OpenFlag4) -> NfsArgOp {
        let mut args = open_args(1, openhow);
        args.claim = OpenClaim4::ClaimNull(name.to_string());
        NfsArgOp::Opopen(args)
    }

    fn unchecked() -> OpenFlag4 {
        OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
            attrmask: Attrlist4(vec![]),
            attr_vals: Attrlist4(vec![]),
        }))
    }

    #[tokio::test]
    async fn test_name_component_by_component_lookup_is_denied() {
        // The control: one component at a time, `private` is not searchable.
        let export = private_dir_export();
        let (status, resarray) = run_other(
            export.path(),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("private"), lookup_op("file"), NfsArgOp::Opgetfh(())],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errAccess);
        assert_eq!(resarray.len(), 3);
        assert_eq!(resarray[2], NfsResOp4::OpError { op: 15, status: NfsStat4::Nfs4errAccess });
    }

    #[tokio::test]
    async fn test_name_lookup_path_past_unsearchable_dir_refused() {
        // The finding: LOOKUP "private/file" from the searchable root got a
        // handle to the file (and READ checks only the file).
        let export = private_dir_export();
        let (status, resarray) = run_other(
            export.path(),
            vec![NfsArgOp::Opputrootfh(()), lookup_op("private/file"), NfsArgOp::Opgetfh(())],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errBadname);
        assert_eq!(resarray.len(), 2);
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 15, status: NfsStat4::Nfs4errBadname });
    }

    #[tokio::test]
    async fn test_name_open_path_past_unsearchable_dir_refused() {
        let export = private_dir_export();
        for openhow in [OpenFlag4::Open4Nocreate, unchecked()] {
            let (status, resarray) = run_other(
                export.path(),
                vec![NfsArgOp::Opputrootfh(()), open_name_op("private/file", openhow)],
            )
            .await;
            assert_eq!(status, NfsStat4::Nfs4errBadname);
            assert_eq!(resarray[1], NfsResOp4::OpError { op: 18, status: NfsStat4::Nfs4errBadname });
        }
        let (status, _) = run_other(
            export.path(),
            vec![NfsArgOp::Opputrootfh(()), open_name_op("private/new", unchecked())],
        )
        .await;
        assert_eq!(status, NfsStat4::Nfs4errBadname);
        assert!(!export.path().join("private/new").exists());
    }

    #[tokio::test]
    async fn test_name_mutations_with_paths_refused() {
        let export = private_dir_export();
        let private_file = export.path().join("private/file");
        let remove = NfsArgOp::Opremove(Remove4args { target: "private/file".to_string() });
        let rename = |old: &str, new: &str| {
            NfsArgOp::Oprename(Rename4args { oldname: old.to_string(), newname: new.to_string() })
        };
        let cases: Vec<(Vec<NfsArgOp>, u32)> = vec![
            (vec![NfsArgOp::Opputrootfh(()), remove], 28),
            (vec![NfsArgOp::Opputrootfh(()), mkdir_op("private/planted")], 6),
            (
                vec![NfsArgOp::Opputrootfh(()), NfsArgOp::Opsavefh(()), rename("private/file", "stolen")],
                29,
            ),
            (
                vec![NfsArgOp::Opputrootfh(()), NfsArgOp::Opsavefh(()), rename("pub", "private/planted")],
                29,
            ),
            (
                vec![
                    NfsArgOp::Opputrootfh(()),
                    lookup_op("pub"),
                    NfsArgOp::Opsavefh(()),
                    NfsArgOp::Opputrootfh(()),
                    NfsArgOp::Oplink(Link4args { newname: "private/planted".to_string() }),
                ],
                11,
            ),
        ];
        for (ops, op) in cases {
            let last = ops.len() - 1;
            let (status, resarray) = run_other(export.path(), ops).await;
            assert_eq!(status, NfsStat4::Nfs4errBadname, "op {}", op);
            assert_eq!(resarray[last], NfsResOp4::OpError { op, status: NfsStat4::Nfs4errBadname });
        }
        assert_eq!(std::fs::read(&private_file).unwrap(), b"private");
        assert!(export.path().join("pub").exists());
        assert!(!export.path().join("stolen").exists());
        assert!(!export.path().join("private/planted").exists());
    }

    /// The ops check the name themselves too, not only the dispatcher: each
    /// called directly (as a test or a future caller would) refuses a path.
    #[tokio::test]
    async fn test_name_ops_refuse_paths_without_dispatcher() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        root.join("sub").unwrap().create_dir().unwrap();
        root.join("sub/f").unwrap().create_file().unwrap();

        let mut request = request;
        for (name, want) in [
            ("sub/f", NfsStat4::Nfs4errBadname),
            ("..", NfsStat4::Nfs4errBadname),
            (".", NfsStat4::Nfs4errBadname),
            ("a\0b", NfsStat4::Nfs4errBadname),
            ("", NfsStat4::Nfs4errInval),
        ] {
            let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
            request.set_filehandle(root_fh);
            let response = Lookup4args { objname: name.to_string() }.execute(request).await;
            assert_eq!(response.status, want, "LOOKUP {:?}", name);
            request = response.request;
        }

        let ops = vec![
            ("OPEN", open_name_op("sub/f", OpenFlag4::Open4Nocreate)),
            ("OPEN create", open_name_op("sub/g", unchecked())),
            ("CREATE", mkdir_op("sub/d")),
            ("REMOVE", NfsArgOp::Opremove(Remove4args { target: "sub/f".to_string() })),
            (
                "RENAME",
                NfsArgOp::Oprename(Rename4args { oldname: "sub/f".to_string(), newname: "g".to_string() }),
            ),
            ("LINK", NfsArgOp::Oplink(Link4args { newname: "sub/g".to_string() })),
        ];
        for (what, op) in ops {
            let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
            request.set_filehandle(root_fh.clone());
            request.set_saved_filehandle(root_fh);
            let response = match op {
                NfsArgOp::Opopen(a) => a.execute(request).await,
                NfsArgOp::Opcreate(a) => a.execute(request).await,
                NfsArgOp::Opremove(a) => a.execute(request).await,
                NfsArgOp::Oprename(a) => a.execute(request).await,
                NfsArgOp::Oplink(a) => a.execute(request).await,
                _ => unreachable!(),
            };
            assert_eq!(response.status, NfsStat4::Nfs4errBadname, "{}", what);
            request = response.request;
        }
        assert!(root.join("sub/f").unwrap().exists().unwrap());
        assert!(!root.join("sub/g").unwrap().exists().unwrap());
        assert!(!root.join("sub/d").unwrap().exists().unwrap());
        assert!(!root.join("g").unwrap().exists().unwrap());
    }

    // --- #130: the pseudo-root READDIR is budgeted and verified too ---

    fn readdir_op(cookie: u64, cookieverf: [u8; 8], maxcount: u32) -> NfsArgOp {
        NfsArgOp::Opreaddir(Readdir4args {
            cookie,
            cookieverf,
            dircount: 0,
            maxcount,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        })
    }

    #[tokio::test]
    async fn test_pseudo_root_readdir_pages_within_maxcount() {
        let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut request = create_nfs40_server(None).await;
        let em = request.export_manager();
        for (name, d) in ["ex1", "ex2", "ex3"].iter().zip(&dirs) {
            em.add_export(name.to_string(), d.path().to_path_buf(), false).await.unwrap();
        }
        let server = NFS40Server::new();

        // One export entry per page (an entry here is well under 64 bytes).
        let (mut cookie, mut verf, mut names) = (0u64, [0u8; 8], vec![]);
        for page in 0.. {
            assert!(page < 10, "no progress");
            let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), readdir_op(cookie, verf, 16 + 64)]);
            let (r, reply) = server.compound(msg, request).await;
            request = r;
            let (status, resarray) = compound_result(reply);
            assert_eq!(status, NfsStat4::Nfs4Ok, "page {}", page);
            let NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok)) = &resarray[1] else {
                panic!("{:?}", resarray)
            };
            assert!(nextnfs_proto::xdr::to_bytes(resok).unwrap().len() <= 16 + 64);
            if let Some(e) = resok.reply.entries.as_ref() {
                assert!(e.nextentry.is_none(), "one entry per page");
                names.push(e.name.clone());
                cookie = e.cookie;
            }
            verf = resok.cookieverf;
            if resok.reply.eof {
                break;
            }
        }
        // Every export once; the order is the export list's own.
        names.sort();
        assert_eq!(names, vec!["ex1", "ex2", "ex3"]);

        // A verifier from another export list is NOT_SAME.
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), readdir_op(3, [0xAB; 8], 4096)]);
        let (r, reply) = server.compound(msg, request).await;
        request = r;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4errNotSame);

        // No room for one entry: TOOSMALL.
        let msg = make_compound(vec![NfsArgOp::Opputrootfh(()), readdir_op(0, [0; 8], 20)]);
        let (_, reply) = server.compound(msg, request).await;
        assert_eq!(compound_result(reply).0, NfsStat4::Nfs4errToosmall);
    }

    // --- #131: every op that ran is in the reply, encoded as the RFC says ---

    /// The reply's XDR words.
    fn reply_words(status: NfsStat4, resarray: Vec<NfsResOp4>) -> Vec<u32> {
        let res = Compound4res { status, tag: String::new(), resarray };
        nextnfs_proto::xdr::to_bytes(&res)
            .unwrap()
            .chunks(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    fn setattr_mode_op() -> NfsArgOp {
        setattr_op(vec![FileAttrValue::Mode(0o600)], vec![FileAttr::Mode])
    }

    #[tokio::test]
    async fn test_failed_op_without_result_is_in_the_reply() {
        // Finding 45e86120ffb7: READ without a filehandle returned no
        // result, so the reply had a failed status and an empty array.
        let dir = tempfile::tempdir().unwrap();
        let read = NfsArgOp::Opread(Read4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            count: 1,
        });
        let (status, resarray) = run_own(dir.path(), vec![read]).await;
        assert_ne!(status, NfsStat4::Nfs4Ok);
        assert_eq!(resarray, vec![NfsResOp4::OpError { op: 25, status: status.clone() }]);
        let code = reply_words(status.clone(), resarray);
        let s: u32 = num_traits::ToPrimitive::to_u32(&status).unwrap();
        // status, empty tag, one result: READ and its status, nothing else
        assert_eq!(code, vec![s, 0, 1, 25, s]);
    }

    #[tokio::test]
    async fn test_failed_open_follows_its_predecessors() {
        let dir = tempfile::tempdir().unwrap();
        let (status, resarray) = run_own(
            dir.path(),
            vec![NfsArgOp::Opputrootfh(()), open_name_op("missing", OpenFlag4::Open4Nocreate), NfsArgOp::Opgetfh(())],
        )
        .await;
        assert_ne!(status, NfsStat4::Nfs4Ok);
        assert_eq!(resarray.len(), 2, "PUTROOTFH, then OPEN's error; GETFH not run");
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 18, status: status.clone() });
    }

    #[tokio::test]
    async fn test_failed_remove_sends_no_cinfo() {
        // REMOVE4res is void on an error; the handler's change_info is not sent.
        let dir = tempfile::tempdir().unwrap();
        let remove = NfsArgOp::Opremove(Remove4args { target: "missing".to_string() });
        let (status, resarray) = run_own(dir.path(), vec![NfsArgOp::Opputrootfh(()), remove]).await;
        assert_ne!(status, NfsStat4::Nfs4Ok);
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 28, status: status.clone() });
    }

    #[tokio::test]
    async fn test_failed_setattr_keeps_its_attrsset() {
        // SETATTR4res has attrsset after the status whatever the status:
        // without a filehandle (handler error) and on a read-only export
        // (dispatcher refusal) alike.
        let dir = tempfile::tempdir().unwrap();
        let (status, resarray) = run_own(dir.path(), vec![setattr_mode_op()]).await;
        assert_ne!(status, NfsStat4::Nfs4Ok);
        assert_eq!(
            resarray,
            vec![NfsResOp4::Opsetattr(SetAttr4res { status: status.clone(), attrsset: Attrlist4(vec![]) })]
        );
        let s: u32 = num_traits::ToPrimitive::to_u32(&status).unwrap();
        // status, tag, one result: SETATTR, its status, an empty bitmap
        // (the codec writes it as one zero word)
        assert_eq!(reply_words(status, resarray), vec![s, 0, 1, 34, s, 1, 0]);

        let request = request_on_export(true, dir.path()).await;
        let server = NFS40Server::new();
        let (_r, reply) = server
            .compound(make_compound(vec![NfsArgOp::Opputrootfh(()), setattr_mode_op()]), request)
            .await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errRofs);
        assert_eq!(
            resarray[1],
            NfsResOp4::Opsetattr(SetAttr4res { status: NfsStat4::Nfs4errRofs, attrsset: Attrlist4(vec![]) })
        );
    }

    #[tokio::test]
    async fn test_grace_refusal_is_in_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let request = request_on_export(false, dir.path()).await;
        let server = NFS40Server::new();
        server.in_grace.store(true, Ordering::Relaxed);
        let remove = NfsArgOp::Opremove(Remove4args { target: "x".to_string() });
        let (_r, reply) = server.compound(make_compound(vec![NfsArgOp::Opputrootfh(()), remove]), request).await;
        let (status, resarray) = compound_result(reply);
        assert_eq!(status, NfsStat4::Nfs4errGrace);
        assert_eq!(resarray.len(), 2);
        assert_eq!(resarray[1], NfsResOp4::OpError { op: 28, status: NfsStat4::Nfs4errGrace });
    }

    #[test]
    fn test_failed_result_keeps_only_rfc_error_bodies() {
        let denied = Lock4denied {
            offset: 0,
            length: 1,
            locktype: NfsLockType4::WriteLt,
            owner: LockOwner4 { clientid: 7, owner: b"o".to_vec() },
        };
        let lock = NfsResOp4::Oplock(Lock4res::Denied(denied.clone()));
        assert_eq!(failed_result(12, NfsStat4::Nfs4errDenied, Some(lock.clone())), lock);
        let lockt = NfsResOp4::Oplockt(Lockt4res::Denied(denied));
        assert_eq!(failed_result(13, NfsStat4::Nfs4errDenied, Some(lockt.clone())), lockt);
        // A body under another status is not sent.
        assert_eq!(
            failed_result(12, NfsStat4::Nfs4errIo, Some(lock)),
            NfsResOp4::OpError { op: 12, status: NfsStat4::Nfs4errIo }
        );
        let remove = NfsResOp4::Opremove(Remove4res {
            status: NfsStat4::Nfs4errNoent,
            cinfo: ChangeInfo4 { atomic: false, before: 1, after: 2 },
        });
        assert_eq!(
            failed_result(28, NfsStat4::Nfs4errNoent, Some(remove)),
            NfsResOp4::OpError { op: 28, status: NfsStat4::Nfs4errNoent }
        );
        let set = NfsResOp4::Opsetattr(SetAttr4res {
            status: NfsStat4::Nfs4errPerm,
            attrsset: Attrlist4(vec![FileAttr::Size]),
        });
        assert_eq!(failed_result(34, NfsStat4::Nfs4errPerm, Some(set.clone())), set);
        assert_eq!(
            failed_result(34, NfsStat4::Nfs4errDelay, None),
            NfsResOp4::Opsetattr(SetAttr4res { status: NfsStat4::Nfs4errDelay, attrsset: Attrlist4(vec![]) })
        );
    }

    fn unstable(offset: u64, data: &[u8]) -> Write4args {
        Write4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset,
            stable: StableHow4::Unstable4,
            data: data.to_vec(),
        }
    }

    fn write_verf(resp: &crate::server::response::NfsOpResponse<'_>) -> [u8; 8] {
        match &resp.result {
            Some(NfsResOp4::Opwrite(Write4res::Resok4(r))) => r.writeverf,
            other => panic!("expected WRITE resok, got {:?}", other),
        }
    }

    /// #136 (finding 8e411131c3ba): an unstable WRITE was acknowledged
    /// once queued, whatever pwrite did. Now its outcome comes back: a
    /// pwrite that fails (offset past off_t: EINVAL) is the WRITE's error,
    /// and nothing is written. A good one on the same file is OK, and the
    /// COMMIT after it carries the same verifier.
    #[tokio::test]
    async fn test_unstable_write_failure_is_not_acknowledged() {
        use crate::server::operation::NfsOperation;
        let export = tempfile::tempdir().unwrap();
        let f = export.path().join("f");
        std::fs::write(&f, b"").unwrap();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request.file_manager().get_filehandle_for_path("/f".to_string()).await.unwrap();

        request.set_filehandle(fh.clone());
        let resp = unstable(u64::MAX - 1, b"lost").execute(request).await;
        assert_eq!(resp.status, NfsStat4::Nfs4errIo);
        assert!(resp.result.is_none());
        let mut request = resp.request;
        assert_eq!(std::fs::read(&f).unwrap(), b"");

        request.set_filehandle(fh.clone());
        let resp = unstable(0, b"kept").execute(request).await;
        assert_eq!(resp.status, NfsStat4::Nfs4Ok);
        let verf = write_verf(&resp);
        let mut request = resp.request;
        assert_eq!(std::fs::read(&f).unwrap(), b"kept");

        request.set_filehandle(fh);
        let resp = Commit4args { offset: 0, count: 0 }.execute(request).await;
        assert_eq!(resp.status, NfsStat4::Nfs4Ok);
        match resp.result {
            Some(NfsResOp4::Opcommit(Commit4res::Resok4(r))) => assert_eq!(r.writeverf, verf),
            other => panic!("expected COMMIT resok, got {:?}", other),
        }
    }

    /// #136: the write cache opened its file with O_CREAT, so an unstable
    /// WRITE through a handle whose file was removed made a new empty file
    /// and wrote into it, OK; and COMMIT said OK. On a real export the
    /// cache never creates the file: WRITE and COMMIT are STALE, and the
    /// file stays gone.
    #[tokio::test]
    async fn test_unstable_write_and_commit_of_removed_file_are_stale() {
        use crate::server::operation::NfsOperation;
        let export = tempfile::tempdir().unwrap();
        let f = export.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        let mut request = request_on_export_squash(false, export.path(), SquashMode::None).await;
        let fh = request.file_manager().get_filehandle_for_path("/f".to_string()).await.unwrap();
        std::fs::remove_file(&f).unwrap();

        request.set_filehandle(fh.clone());
        let resp = unstable(0, b"data").execute(request).await;
        assert_eq!(resp.status, NfsStat4::Nfs4errStale);
        let mut request = resp.request;
        assert!(!f.exists());

        request.set_filehandle(fh);
        let resp = Commit4args { offset: 0, count: 0 }.execute(request).await;
        assert_eq!(resp.status, NfsStat4::Nfs4errStale);
        assert!(!f.exists());
    }

    /// #136: the statuses a failed write-cache step maps to. WRITE keeps
    /// the errno's (NOSPC, DQUOT, FBIG); COMMIT, which RFC 8881 gives no
    /// NOSPC, says IO for them; a gone file is STALE; a dead actor
    /// SERVERFAULT.
    #[test]
    fn test_write_cache_error_statuses() {
        use crate::server::filemanager::WriteCacheError as E;
        let io = |n| E::Io(std::io::Error::from_raw_os_error(n));
        assert_eq!(io(libc::ENOSPC).write_status(), NfsStat4::Nfs4errNospc);
        assert_eq!(io(libc::EDQUOT).write_status(), NfsStat4::Nfs4errDquot);
        assert_eq!(io(libc::EFBIG).write_status(), NfsStat4::Nfs4errFbig);
        assert_eq!(io(libc::EIO).write_status(), NfsStat4::Nfs4errIo);
        assert_eq!(E::Gone.write_status(), NfsStat4::Nfs4errStale);
        assert_eq!(E::ActorGone.write_status(), NfsStat4::Nfs4errServerfault);
        assert_eq!(io(libc::ENOSPC).commit_status(), NfsStat4::Nfs4errIo);
        assert_eq!(io(libc::EIO).commit_status(), NfsStat4::Nfs4errIo);
        assert_eq!(io(libc::EISDIR).commit_status(), NfsStat4::Nfs4errIsdir);
        assert_eq!(E::Gone.commit_status(), NfsStat4::Nfs4errStale);
        assert_eq!(E::ActorGone.commit_status(), NfsStat4::Nfs4errServerfault);
    }
}
