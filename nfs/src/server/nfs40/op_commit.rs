use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{Commit4args, Commit4res, Commit4resok, NfsResOp4, NfsStat4};

fn verifier_from_boot(boot_time: &u64) -> [u8; 8] {
    let mut verifier = [0; 8];
    verifier.copy_from_slice(boot_time.to_be_bytes().as_ref());
    verifier
}

#[async_trait]
impl NfsOperation for Commit4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 5: COMMIT - Commit Cached Data {:?}, with request {:?}",
            self, request
        );
        let current_filehandle = request.current_filehandle();
        let filehandle = match current_filehandle {
            Some(filehandle) => filehandle,
            None => {
                error!("None filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        // unlock write cache & write file

        let write_cache = match request
            .file_manager()
            .get_write_cache_handle(filehandle.clone())
            .await
        {
            Ok(wc) => wc,
            Err(e) => {
                error!("COMMIT: failed to get write cache: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errServerfault,
                };
            }
        };
        write_cache.commit().await;

        request
            .file_manager()
            .touch_file(filehandle.id)
            .await;

        request.drop_filehandle_from_cache(filehandle.id);
        let boot_time = request.boot_time;
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opcommit(Commit4res::Resok4(Commit4resok {
                writeverf: verifier_from_boot(&boot_time),
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
    async fn test_commit_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Commit4args {
            offset: 0,
            count: 0,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[test]
    fn test_verifier_from_boot() {
        let boot_time: u64 = 0x0102030405060708;
        let verifier = verifier_from_boot(&boot_time);
        assert_eq!(verifier, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    }
}
