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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_openconfirm_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = OpenConfirm4args {
            open_stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            seqid: 1,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_openconfirm_no_locks() {
        // Root filehandle has no locks, so OPEN_CONFIRM should fail
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = OpenConfirm4args {
            open_stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            seqid: 1,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadStateid);
    }
}
