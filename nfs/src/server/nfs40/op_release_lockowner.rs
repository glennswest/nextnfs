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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;
    use nextnfs_proto::nfs4_proto::{LockOwner4, NfsStat4};

    #[tokio::test]
    async fn test_release_lockowner_no_locks() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = ReleaseLockowner4args {
            lock_owner: LockOwner4 {
                clientid: 1,
                owner: b"test_owner".to_vec(),
            },
        };
        let response = args.execute(request).await;
        // Releasing a nonexistent lock owner should succeed (no-op)
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
