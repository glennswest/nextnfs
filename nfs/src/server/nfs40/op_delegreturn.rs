use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{DelegReturn4args, DelegReturn4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for DelegReturn4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 8: DELEGRETURN - Return Delegation {:?}",
            self.deleg_stateid
        );

        let filehandle = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("DELEGRETURN: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opdelegreturn(DelegReturn4res {
                        status: NfsStat4::Nfs4errFhexpired,
                    })),
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        let fh_id = filehandle.id;
        let status = request
            .file_manager()
            .return_delegation(fh_id, self.deleg_stateid.clone())
            .await;

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opdelegreturn(DelegReturn4res {
                status: status.clone(),
            })),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{DelegReturn4args, NfsStat4, Stateid4},
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_delegreturn_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = DelegReturn4args {
            deleg_stateid: Stateid4 {
                seqid: 1,
                other: [0; 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_delegreturn_bad_stateid() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = DelegReturn4args {
            deleg_stateid: Stateid4 {
                seqid: 999,
                other: [0xFF; 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadStateid);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_grant_delegation_then_return() {
        let request = create_nfs40_server_with_root_fh(None).await;

        // Grant a delegation directly via the file manager
        let fh = request.current_filehandle().unwrap().clone();
        let deleg_stateid = request
            .file_manager()
            .grant_delegation(fh.id, 1, false)
            .await
            .expect("should grant delegation");

        // DELEGRETURN with the correct stateid
        let delegreturn_args = DelegReturn4args {
            deleg_stateid,
        };
        let response = delegreturn_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
