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
                crate::server::filemanager::Filehandle::pseudo_root(self.object.clone());
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

        match request.get_filehandle_from_cache(self.object.clone()) {
            Some(fh) => {
                request.set_filehandle(fh);
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opputfh(PutFh4res {
                        status: NfsStat4::Nfs4Ok,
                    })),
                    status: NfsStat4::Nfs4Ok,
                };
            }
            None => {}
        }

        match request.set_filehandle_id(self.object.clone()).await {
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
