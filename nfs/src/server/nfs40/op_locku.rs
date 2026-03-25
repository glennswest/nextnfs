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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;
    use nextnfs_proto::nfs4_proto::{NfsLockType4, Stateid4};

    #[tokio::test]
    async fn test_locku_nonexistent_lock() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Locku4args {
            locktype: NfsLockType4::ReadLt,
            seqid: 1,
            lock_stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            offset: 0,
            length: 100,
        };
        let response = args.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_locku_valid_unlock() {
        use crate::server::filemanager::LockResult;
        let request = create_nfs40_server_with_root_fh(None).await;
        let root_fh_id = request.current_filehandle().unwrap().id;

        // Acquire a lock first
        let lock_result = request.file_manager()
            .lock_file(root_fh_id, 1, b"owner1".to_vec(), NfsLockType4::WriteLt, 0, 100)
            .await;
        let stateid = match lock_result {
            LockResult::Ok(s) => s,
            other => panic!("Expected LockResult::Ok, got {:?}", other),
        };

        let args = Locku4args {
            locktype: NfsLockType4::WriteLt,
            seqid: stateid.seqid,
            lock_stateid: stateid.clone(),
            offset: 0,
            length: 100,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Oplocku(Locku4res::LockStateid(sid))) => {
                assert_eq!(sid.seqid, stateid.seqid + 1);
            }
            other => panic!("Expected Oplocku LockStateid, got {:?}", other),
        }
    }
}
