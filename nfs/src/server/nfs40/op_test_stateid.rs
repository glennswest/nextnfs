use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{
    NfsResOp4, NfsStat4, TestStateid4args, TestStateid4res, TestStateid4resok,
};

#[async_trait]
impl NfsOperation for TestStateid4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 55: TEST_STATEID count={}",
            self.ts_stateids.len()
        );

        // Return NFS4_OK for all stateids — we don't track v4.1 stateids separately yet.
        // Clients use this to validate stateids are still valid.
        let status_codes: Vec<u32> = self
            .ts_stateids
            .iter()
            .map(|_| NfsStat4::Nfs4Ok as u32)
            .collect();

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OptestStateid(TestStateid4res::Resok4(
                TestStateid4resok { tsr_status_codes: status_codes },
            ))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{NfsResOp4, NfsStat4, Stateid4},
            operation::NfsOperation,
        },
        test_utils::create_nfs40_server,
    };
    use nextnfs_proto::nfs4_proto::{TestStateid4args, TestStateid4res};
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_test_stateid_empty() {
        let request = create_nfs40_server(None).await;
        let args = TestStateid4args {
            ts_stateids: vec![],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::OptestStateid(TestStateid4res::Resok4(res))) = &response.result {
            assert!(res.tsr_status_codes.is_empty());
        } else {
            panic!("Expected TestStateid4resok");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_test_stateid_multiple() {
        let request = create_nfs40_server(None).await;
        let args = TestStateid4args {
            ts_stateids: vec![
                Stateid4 { seqid: 1, other: [0; 12] },
                Stateid4 { seqid: 2, other: [1; 12] },
            ],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::OptestStateid(TestStateid4res::Resok4(res))) = &response.result {
            assert_eq!(res.tsr_status_codes.len(), 2);
            assert_eq!(res.tsr_status_codes[0], NfsStat4::Nfs4Ok as u32);
            assert_eq!(res.tsr_status_codes[1], NfsStat4::Nfs4Ok as u32);
        } else {
            panic!("Expected TestStateid4resok");
        }
    }
}
