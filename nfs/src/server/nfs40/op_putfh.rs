use async_trait::async_trait;
use tracing::debug;

use crate::server::{
    nfs40::op_pseudo, operation::NfsOperation, request::NfsRequest, response::NfsOpResponse,
};
use nextnfs_proto::nfs4_proto::{NfsResOp4, NfsStat4, PutFh4args, PutFh4res};

#[async_trait]
impl NfsOperation for PutFh4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 22: PUTFH - Set Current Filehandle {:?}, with request {:?}",
            self, request
        );

        // Check if this is a pseudo-root filehandle
        if op_pseudo::is_pseudo_root(&self.object) {
            request.set_export(op_pseudo::PSEUDO_ROOT_EXPORT_ID).await;
            let pseudo_fh =
                crate::server::filemanager::Filehandle::pseudo_root(self.object);
            request.set_filehandle(pseudo_fh);
            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opputfh(PutFh4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            };
        }

        // Extract export_id from the filehandle and switch if needed
        let export_id = op_pseudo::export_id_from_fh(&self.object);
        if request.current_export_id() != Some(export_id) {
            request.set_export(export_id).await;
        }

        if let Some(fh) = request.get_filehandle_from_cache(self.object) {
            request.set_filehandle(fh);
            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opputfh(PutFh4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            };
        }

        match request.set_filehandle_id(self.object).await {
            Ok(fh) => {
                request.cache_filehandle(fh);
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opputfh(PutFh4res {
                        status: NfsStat4::Nfs4Ok,
                    })),
                    status: NfsStat4::Nfs4Ok,
                };
            }
            Err(e) => {
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opputfh(PutFh4res { status: e.clone() })),
                    status: e,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_putfh_pseudo_root() {
        let request = create_nfs40_server(None).await;
        let pseudo_fh = op_pseudo::pseudo_root_fh();
        let args = PutFh4args { object: pseudo_fh };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_putfh_invalid_handle() {
        let request = create_nfs40_server(None).await;
        let args = PutFh4args {
            object: [0xFF; 26],
        };
        let response = args.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_putfh_valid_root_handle() {
        // Get the root fh ID, then use PUTFH to set it
        let mut request = create_nfs40_server(None).await;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        let root_id = root_fh.id;
        // Need to cache the filehandle so putfh can find it
        request.set_filehandle(root_fh);

        let args = PutFh4args { object: root_id };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
