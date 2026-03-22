use async_trait::async_trait;
use tracing::debug;

use crate::server::{
    filemanager::UnlockResult, operation::NfsOperation, request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Locku4args, Locku4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for Locku4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 14: LOCKU {:?}", self);

        let result = request
            .file_manager()
            .unlock_file(self.lock_stateid.other, self.offset, self.length)
            .await;

        match result {
            UnlockResult::Ok(stateid) => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oplocku(Locku4res::LockStateid(stateid))),
                status: NfsStat4::Nfs4Ok,
            },
            UnlockResult::Error(status) => NfsOpResponse {
                request,
                result: None,
                status,
            },
        }
    }
}
