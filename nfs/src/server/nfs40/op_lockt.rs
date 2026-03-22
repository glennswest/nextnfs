use async_trait::async_trait;
use tracing::debug;

use crate::server::{
    filemanager::TestLockResult, operation::NfsOperation, request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    Lock4denied, Lockt4args, Lockt4res, LockOwner4, NfsResOp4, NfsStat4,
};

#[async_trait]
impl NfsOperation for Lockt4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 13: LOCKT {:?}", self);

        let fh = match request.current_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        let result = request
            .file_manager()
            .test_lock(
                fh.id,
                self.owner.clientid,
                self.owner.owner.clone(),
                self.locktype.clone(),
                self.offset,
                self.length,
            )
            .await;

        match result {
            TestLockResult::Ok => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oplockt(Lockt4res::Denied(Lock4denied {
                    offset: 0,
                    length: 0,
                    locktype: self.locktype.clone(),
                    owner: LockOwner4 {
                        clientid: self.owner.clientid,
                        owner: self.owner.owner.clone(),
                    },
                }))),
                status: NfsStat4::Nfs4Ok,
            },
            TestLockResult::Denied {
                offset,
                length,
                lock_type,
                owner_clientid,
                owner,
            } => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oplockt(Lockt4res::Denied(Lock4denied {
                    offset,
                    length,
                    locktype: lock_type,
                    owner: LockOwner4 {
                        clientid: owner_clientid,
                        owner,
                    },
                }))),
                status: NfsStat4::Nfs4errDenied,
            },
        }
    }
}
