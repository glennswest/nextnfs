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

        // Check that the current filehandle has locks (open state)
        if filehandle.locks.is_empty() {
            error!("OPEN_DOWNGRADE: no open state for filehandle");
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errBadStateid,
            };
        }

        // Extract lock state before moving request
        let lock_stateid = filehandle.locks[0].stateid;
        let new_seqid = self.seqid.wrapping_add(1);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpopenDowngrade(OpenDowngrade4res::Resok4(
                OpenDowngrade4resok {
                    open_stateid: Stateid4 {
                        seqid: new_seqid,
                        other: lock_stateid,
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

        // OPEN sets the current fh to the opened file, which has locks.
        // The filehandle should already have locks from the OPEN create.
        // Now downgrade from BOTH to READ.
        let downgrade_args = make_downgrade_args(2, 1, 0); // share_access=1 (READ)
        let response = downgrade_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::OpopenDowngrade(OpenDowngrade4res::Resok4(resok))) => {
                assert_eq!(resok.open_stateid.seqid, 3); // seqid incremented
            }
            _ => panic!("Expected OPEN_DOWNGRADE Resok4"),
        }
    }
}
