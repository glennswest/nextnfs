use nextnfs_proto::nfs4_proto::{NfsResOp4, NfsStat4};

use super::request::NfsRequest;

#[derive(Debug)]
pub struct NfsOpResponse<'a> {
    pub request: NfsRequest<'a>,
    // result of this operation
    pub result: Option<NfsResOp4>,
    // status of this operation, err or ok
    pub status: NfsStat4,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_nfs40_server;

    #[tokio::test]
    async fn test_response_ok_no_result() {
        let request = create_nfs40_server(None).await;
        let response = NfsOpResponse {
            request,
            result: None,
            status: NfsStat4::Nfs4Ok,
        };
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        assert!(response.result.is_none());
    }

    #[tokio::test]
    async fn test_response_error_status() {
        let request = create_nfs40_server(None).await;
        let response = NfsOpResponse {
            request,
            result: None,
            status: NfsStat4::Nfs4errStale,
        };
        assert_eq!(response.status, NfsStat4::Nfs4errStale);
    }
}
