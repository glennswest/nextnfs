use async_trait::async_trait;
use tracing::debug;

use crate::server::{
    filemanager::LockResult, operation::NfsOperation, request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    Lock4args, Lock4denied, Lock4res, Lock4resok, Locker4, LockOwner4, NfsResOp4, NfsStat4,
};

#[async_trait]
impl NfsOperation for Lock4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 12: LOCK {:?}", self);

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

        // Extract client_id and owner from the locker
        let (client_id, owner) = match &self.locker {
            Locker4::OpenOwner(otl) => (otl.lock_owner.clientid, otl.lock_owner.owner.clone()),
            Locker4::LockOwner(el) => {
                // For existing lock owner, we need to find the owner info from the stateid.
                // Use client_id 0 as placeholder — the lock manager will match by stateid.
                (0u64, el.lock_stateid.other.to_vec())
            }
        };

        let result = request
            .file_manager()
            .lock_file(
                fh.id,
                client_id,
                owner,
                self.locktype.clone(),
                self.offset,
                self.length,
            )
            .await;

        match result {
            LockResult::Ok(stateid) => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oplock(Lock4res::Resok4(Lock4resok {
                    lock_stateid: stateid,
                }))),
                status: NfsStat4::Nfs4Ok,
            },
            LockResult::Denied {
                offset,
                length,
                lock_type,
                owner_clientid,
                owner,
            } => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oplock(Lock4res::Denied(Lock4denied {
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
            LockResult::Error(status) => NfsOpResponse {
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
    use nextnfs_proto::nfs4_proto::{NfsLockType4, OpenToLockOwner4, Stateid4};

    #[tokio::test]
    async fn test_lock_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Lock4args {
            locktype: NfsLockType4::WriteLt,
            reclaim: false,
            offset: 0,
            length: 100,
            locker: Locker4::OpenOwner(OpenToLockOwner4 {
                open_seqid: 1,
                open_stateid: Stateid4 {
                    seqid: 0,
                    other: [0; 12],
                },
                lock_seqid: 1,
                lock_owner: LockOwner4 {
                    clientid: 1,
                    owner: b"test".to_vec(),
                },
            }),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_lock_on_root_directory() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Lock4args {
            locktype: NfsLockType4::ReadLt,
            reclaim: false,
            offset: 0,
            length: 0xFFFFFFFFFFFFFFFF,
            locker: Locker4::OpenOwner(OpenToLockOwner4 {
                open_seqid: 1,
                open_stateid: Stateid4 {
                    seqid: 0,
                    other: [0; 12],
                },
                lock_seqid: 1,
                lock_owner: LockOwner4 {
                    clientid: 1,
                    owner: b"test".to_vec(),
                },
            }),
        };
        let response = args.execute(request).await;
        // Lock on root should succeed (lock manager accepts any inode)
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
