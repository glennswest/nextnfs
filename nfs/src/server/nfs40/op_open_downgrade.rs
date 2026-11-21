//! OPEN_DOWNGRADE operation — reduce open share access/deny modes.
//!
//! RFC 7530 Section 16.19: Reduces the access rights for an open file.
//! The client can use this to downgrade share_access or share_deny
//! without closing the file. Required for correct open state management.

use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::operation::NfsOperation;
use crate::server::request::NfsRequest;
use crate::server::response::NfsOpResponse;
use nextnfs_proto::nfs4_proto::*;

#[async_trait]
impl NfsOperation for OpenDowngrade4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "OPEN_DOWNGRADE stateid={:?} seqid={} share_access={} share_deny={}",
            self.open_stateid, self.seqid, self.share_access, self.share_deny
        );

        let filehandle = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("OPEN_DOWNGRADE: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // Validate share_access is valid (1=READ, 2=WRITE, 3=BOTH)
        if self.share_access == 0 || self.share_access > 3 {
            error!("OPEN_DOWNGRADE: invalid share_access {}", self.share_access);
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errInval,
            };
        }

        // The reservation itself is narrowed (finding 60ad262caa93): the
        // deny modes other clients are checked against change with it.
        let fh_id = filehandle.id;
        let lock = match request
            .file_manager()
            .downgrade_open(fh_id, self.open_stateid.other, self.share_access, self.share_deny)
            .await
        {
            Ok(lock) => lock,
            Err(e) => {
                error!("OPEN_DOWNGRADE: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: e.nfs_error,
                };
            }
        };
        let mut request = request;
        // Cached copies of the filehandle carry the old reservation.
        request.drop_filehandle_from_cache(fh_id);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpopenDowngrade(OpenDowngrade4res::Resok4(
                OpenDowngrade4resok {
                    open_stateid: Stateid4 {
                        seqid: lock.seqid,
                        other: lock.stateid,
                    },
                },
            ))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    fn make_downgrade_args(seqid: u32, share_access: u32, share_deny: u32) -> OpenDowngrade4args {
        OpenDowngrade4args {
            open_stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            seqid,
            share_access,
            share_deny,
        }
    }

    #[tokio::test]
    async fn test_open_downgrade_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = make_downgrade_args(1, 1, 0);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_open_downgrade_invalid_share_access_zero() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_downgrade_args(1, 0, 0); // share_access=0 is invalid
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errInval);
    }

    #[tokio::test]
    async fn test_open_downgrade_invalid_share_access_too_high() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_downgrade_args(1, 4, 0); // share_access > 3 is invalid
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errInval);
    }

    #[tokio::test]
    async fn test_open_downgrade_no_open_state() {
        // Root dir has no locks/open state
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_downgrade_args(1, 1, 0);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadStateid);
    }

    #[tokio::test]
    async fn test_open_downgrade_success() {
        // Create and open a file to get open state with locks
        let request = create_nfs40_server_with_root_fh(None).await;

        // Open a file for writing (creates locks via UNCHECKED4 create)
        let open_args = Open4args {
            seqid: 1,
            share_access: 3, // OPEN4_SHARE_ACCESS_BOTH
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: vec![1, 2, 3, 4],
            },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("downgrade_test.txt".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let opened = open_stateid(&response);

        // Downgrade from BOTH to READ: the stateid's seqid goes up by one.
        let mut downgrade_args = make_downgrade_args(2, 1, 0);
        downgrade_args.open_stateid = opened.clone();
        let response = downgrade_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let downgraded = match response.result {
            Some(NfsResOp4::OpopenDowngrade(OpenDowngrade4res::Resok4(ref resok))) => resok.open_stateid.clone(),
            _ => panic!("Expected OPEN_DOWNGRADE Resok4"),
        };
        assert_eq!(downgraded.other, opened.other);
        assert_eq!(downgraded.seqid, opened.seqid + 1);

        // Back up to WRITE is not a downgrade.
        let mut up = make_downgrade_args(3, 2, 0);
        up.open_stateid = downgraded;
        let response = up.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errInval);
    }

    fn open_stateid(response: &crate::server::response::NfsOpResponse<'_>) -> Stateid4 {
        match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => resok.stateid.clone(),
            other => panic!("Expected OPEN Resok4, got {:?}", other),
        }
    }

    /// Finding 60ad262caa93: after a downgrade from deny WRITE to deny NONE,
    /// another open-owner may open for writing; before it, it may not.
    #[tokio::test]
    async fn test_open_downgrade_releases_deny() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().clone();
        let open = |owner: &[u8], access: u32, deny: u32| Open4args {
            seqid: 1,
            share_access: access,
            share_deny: deny,
            owner: OpenOwner4 { clientid: 1, owner: owner.to_vec() },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("dg_deny.txt".to_string()),
        };
        let response = open(b"a", 1, 2).execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let a = open_stateid(&response);
        let mut request = response.request;

        request.set_filehandle(root.clone());
        let response = open(b"b", 2, 0).execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errShareDenied);
        let mut request = response.request;

        // Back on the file: downgrade a's deny WRITE to NONE.
        let fh = request
            .file_manager()
            .get_filehandle_for_path("/dg_deny.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);
        let mut dg = make_downgrade_args(2, 1, 0);
        dg.open_stateid = a;
        let response = dg.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let mut request = response.request;

        request.set_filehandle(root);
        let response = open(b"b", 2, 0).execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
