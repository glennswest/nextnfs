use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{
    NfsResOp4, ReleaseLockowner4args, ReleaseLockowner4res,
};

#[async_trait]
impl NfsOperation for ReleaseLockowner4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 39: RELEASE_LOCKOWNER {:?}", self);

        let status = request
            .file_manager()
            .release_lock_owner(self.lock_owner.clientid, self.lock_owner.owner.clone())
            .await;

        let result_status = status.clone();
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpreleaseLockOwner(ReleaseLockowner4res {
                status: result_status,
            })),
            status,
        }
    }
}
