use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{FreeStateid4args, FreeStateid4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for FreeStateid4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 45: FREE_STATEID seqid={}",
            self.fsa_stateid.seqid
        );

        // FREE_STATEID releases a stateid that is no longer needed.
        // Accept unconditionally — the stateid may have already been freed.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpfreeStateid(FreeStateid4res {
                fsr_status: NfsStat4::Nfs4Ok,
            })),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{nfs40::{NfsStat4, Stateid4}, operation::NfsOperation},
        test_utils::create_nfs40_server,
    };
    use nextnfs_proto::nfs4_proto::FreeStateid4args;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_free_stateid() {
        let request = create_nfs40_server(None).await;
        let args = FreeStateid4args {
            fsa_stateid: Stateid4 { seqid: 1, other: [0; 12] },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
