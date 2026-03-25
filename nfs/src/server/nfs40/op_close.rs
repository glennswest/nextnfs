use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{Close4args, Close4res, NfsResOp4, NfsStat4, Stateid4};

#[async_trait]
impl NfsOperation for Close4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 4: CLOSE - Close File {:?}, with request {:?}",
            self, request
        );

        let current_filehandle = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("CLOSE: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };
        request.drop_filehandle_from_cache(current_filehandle.id);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opclose(Close4res::OpenStateid(Stateid4 {
                seqid: self.seqid,
                other: self.open_stateid.other,
            }))),
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
    async fn test_close_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Close4args {
            seqid: 1,
            open_stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_close_with_filehandle() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Close4args {
            seqid: 1,
            open_stateid: Stateid4 {
                seqid: 0,
                other: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opclose(Close4res::OpenStateid(stateid))) => {
                assert_eq!(stateid.seqid, 1);
                assert_eq!(stateid.other, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
            }
            other => panic!("Expected Opclose, got {:?}", other),
        }
    }
}
