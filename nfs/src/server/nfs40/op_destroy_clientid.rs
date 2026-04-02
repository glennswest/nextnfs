use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{DestroyClientId4args, DestroyClientId4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for DestroyClientId4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 57: DESTROY_CLIENTID clientid={}",
            self.dca_clientid
        );

        let sm = match request.session_manager() {
            Some(sm) => sm.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::OpdestroyClientid(DestroyClientId4res {
                        dcr_status: NfsStat4::Nfs4errServerfault,
                    })),
                    status: NfsStat4::Nfs4errServerfault,
                };
            }
        };

        // Destroy all sessions for this client
        let _destroyed = sm.destroy_client(self.dca_clientid).await;

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpdestroyClientid(DestroyClientId4res {
                dcr_status: NfsStat4::Nfs4Ok,
            })),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{nfs40::NfsStat4, operation::NfsOperation},
        test_utils::create_nfs40_server,
    };
    use nextnfs_proto::nfs4_proto::DestroyClientId4args;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_destroy_clientid() {
        let request = create_nfs40_server(None).await;
        let args = DestroyClientId4args { dca_clientid: 999 };
        let response = args.execute(request).await;
        // Always succeeds (even for unknown client IDs — idempotent)
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
