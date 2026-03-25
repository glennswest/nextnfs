use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{
    NfsResOp4, NfsStat4, OpenConfirm4args, OpenConfirm4res, OpenConfirm4resok, Stateid4,
};

#[async_trait]
impl NfsOperation for OpenConfirm4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 20: OPEN_CONFIRM - Confirm Open {:?}, with request {:?}",
            self, request
        );

        let fh = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("OPEN_CONFIRM: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        let lock = match fh.locks.first() {
            Some(lock) => lock.clone(),
            None => {
                error!("OPEN_CONFIRM: no locks on filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errBadStateid,
                };
            }
        };

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpopenConfirm(OpenConfirm4res::Resok4(
                OpenConfirm4resok {
                    open_stateid: Stateid4 {
                        seqid: lock.seqid,
                        other: lock.stateid,
                    },
                },
            ))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}
