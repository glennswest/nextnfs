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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;
    use nextnfs_proto::nfs4_proto::NfsLockType4;

    #[tokio::test]
    async fn test_lockt_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Lockt4args {
            locktype: NfsLockType4::ReadLt,
            offset: 0,
            length: 100,
            owner: LockOwner4 {
                clientid: 1,
                owner: b"test".to_vec(),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_lockt_no_conflict() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Lockt4args {
            locktype: NfsLockType4::ReadLt,
            offset: 0,
            length: 100,
            owner: LockOwner4 {
                clientid: 1,
                owner: b"test".to_vec(),
            },
        };
        let response = args.execute(request).await;
        // No existing locks, so test should succeed (Nfs4Ok)
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_lockt_detects_conflict() {
        use nextnfs_proto::nfs4_proto::{Lock4args, Locker4, OpenToLockOwner4, Stateid4};

        let request = create_nfs40_server_with_root_fh(None).await;
        // Acquire a write lock via LOCK
        let lock_args = Lock4args {
            locktype: NfsLockType4::WriteLt,
            reclaim: false,
            offset: 0,
            length: 100,
            locker: Locker4::OpenOwner(OpenToLockOwner4 {
                open_seqid: 1,
                open_stateid: Stateid4 { seqid: 0, other: [0; 12] },
                lock_seqid: 1,
                lock_owner: LockOwner4 { clientid: 1, owner: b"owner1".to_vec() },
            }),
        };
        let lock_resp = lock_args.execute(request).await;
        assert_eq!(lock_resp.status, NfsStat4::Nfs4Ok);

        // Test lock from different client — should detect conflict
        let test_args = Lockt4args {
            locktype: NfsLockType4::ReadLt,
            offset: 50,
            length: 50,
            owner: LockOwner4 { clientid: 2, owner: b"owner2".to_vec() },
        };
        let test_resp = test_args.execute(lock_resp.request).await;
        assert_eq!(test_resp.status, NfsStat4::Nfs4errDenied);
        if let Some(NfsResOp4::Oplockt(Lockt4res::Denied(denied))) = test_resp.result {
            assert_eq!(denied.owner.clientid, 1);
        } else {
            panic!("Expected Lockt4res::Denied");
        }
    }
}
