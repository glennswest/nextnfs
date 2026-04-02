use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{NfsResOp4, NfsStat4, ReclaimComplete4args, ReclaimComplete4res};

#[async_trait]
impl NfsOperation for ReclaimComplete4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 58: RECLAIM_COMPLETE one_fs={}",
            self.rca_one_fs
        );

        // RECLAIM_COMPLETE signals the client has finished reclaiming state.
        // Since we support near-zero grace period via state recovery, this is a no-op.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpreclaimComplete(ReclaimComplete4res {
                rcr_status: NfsStat4::Nfs4Ok,
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
    use nextnfs_proto::nfs4_proto::ReclaimComplete4args;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_reclaim_complete() {
        let request = create_nfs40_server(None).await;
        let args = ReclaimComplete4args { rca_one_fs: false };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
