use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{DelegPurge4args, DelegPurge4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for DelegPurge4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 7: DELEGPURGE - Purge Delegations Awaiting Recovery, clientid={}",
            self.clientid
        );

        // DELEGPURGE informs the server that the client will not reclaim any more
        // delegations. This is a no-op for our implementation — we don't hold
        // unclaimed delegations after grace period anyway.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opdelegpurge(DelegPurge4res {
                status: NfsStat4::Nfs4Ok,
            })),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{DelegPurge4args, NfsStat4},
            operation::NfsOperation,
        },
        test_utils::create_nfs40_server,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_delegpurge() {
        let request = create_nfs40_server(None).await;
        let args = DelegPurge4args { clientid: 12345 };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
