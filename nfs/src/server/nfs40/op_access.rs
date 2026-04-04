use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{
    Access4args, Access4res, Access4resok, NfsFtype4, NfsResOp4, NfsStat4, ACCESS4_DELETE,
    ACCESS4_EXECUTE, ACCESS4_EXTEND, ACCESS4_LOOKUP, ACCESS4_MODIFY, ACCESS4_READ,
};

/// Check POSIX permissions and return the subset of requested NFS access flags
/// that the caller is allowed.
fn check_access(
    requested: u32,
    mode: u32,
    file_uid: u32,
    file_gid: u32,
    caller_uid: u32,
    caller_gid: u32,
    is_dir: bool,
) -> u32 {
    // Root gets everything
    if caller_uid == 0 {
        return requested;
    }

    // Determine which POSIX permission bits apply
    let bits = if caller_uid == file_uid {
        (mode >> 6) & 7 // owner bits
    } else if caller_gid == file_gid {
        (mode >> 3) & 7 // group bits
    } else {
        mode & 7 // other bits
    };

    let has_read = bits & 4 != 0;
    let has_write = bits & 2 != 0;
    let has_exec = bits & 1 != 0;

    let mut granted = 0u32;

    if has_read && (requested & ACCESS4_READ != 0) {
        granted |= ACCESS4_READ;
    }
    if has_exec && (requested & ACCESS4_EXECUTE != 0) {
        granted |= ACCESS4_EXECUTE;
    }
    if is_dir && has_exec && (requested & ACCESS4_LOOKUP != 0) {
        granted |= ACCESS4_LOOKUP;
    }
    if !is_dir && has_read && (requested & ACCESS4_LOOKUP != 0) {
        // LOOKUP on non-dir is meaningless but harmless — grant if readable
        granted |= ACCESS4_LOOKUP;
    }
    if has_write && (requested & ACCESS4_MODIFY != 0) {
        granted |= ACCESS4_MODIFY;
    }
    if has_write && (requested & ACCESS4_EXTEND != 0) {
        granted |= ACCESS4_EXTEND;
    }
    if has_write && (requested & ACCESS4_DELETE != 0) {
        granted |= ACCESS4_DELETE;
    }

    granted
}

#[async_trait]
impl NfsOperation for Access4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 3: ACCESS - Check Access Rights {:?}, with request {:?}",
            self, request
        );

        let supported = ACCESS4_READ
            | ACCESS4_LOOKUP
            | ACCESS4_MODIFY
            | ACCESS4_EXTEND
            | ACCESS4_DELETE
            | ACCESS4_EXECUTE;

        // If we have a current filehandle, check real permissions
        let access = if let Some(fh) = request.current_filehandle() {
            let file_uid = fh.attr_owner.parse::<u32>().unwrap_or(0);
            let file_gid = fh.attr_owner_group.parse::<u32>().unwrap_or(0);
            let is_dir = fh.attr_type == NfsFtype4::Nf4dir;
            check_access(
                self.access,
                fh.attr_mode,
                file_uid,
                file_gid,
                request.auth_uid(),
                request.auth_gid(),
                is_dir,
            )
        } else {
            // No filehandle — grant what was requested (best effort)
            self.access
        };

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpAccess(Access4res::Resok4(Access4resok {
                supported,
                access,
            }))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use crate::{
        server::{
            nfs40::{
                Access4args, Access4res, NfsResOp4, NfsStat4, ACCESS4_DELETE, ACCESS4_EXECUTE,
                ACCESS4_EXTEND, ACCESS4_LOOKUP, ACCESS4_MODIFY, ACCESS4_READ,
            },
            operation::NfsOperation,
        },
        test_utils::create_nfs40_server,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_access_single_read_flag() {
        let request = create_nfs40_server(None).await;
        let args = Access4args {
            access: ACCESS4_READ,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::OpAccess(Access4res::Resok4(res))) = response.result {
            assert_eq!(res.access, ACCESS4_READ);
        } else {
            panic!("Unexpected response");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_access_execute_flag() {
        let request = create_nfs40_server(None).await;
        let args = Access4args {
            access: ACCESS4_EXECUTE,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::OpAccess(Access4res::Resok4(res))) = response.result {
            assert_eq!(res.access, ACCESS4_EXECUTE);
        } else {
            panic!("Unexpected response");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_access_zero_flags() {
        let request = create_nfs40_server(None).await;
        let args = Access4args { access: 0 };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::OpAccess(Access4res::Resok4(res))) = response.result {
            assert_eq!(res.access, 0);
        } else {
            panic!("Unexpected response");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_access_all_flags() {
        let request = create_nfs40_server(None).await;
        let all = ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY
            | ACCESS4_EXTEND | ACCESS4_DELETE | ACCESS4_EXECUTE;
        let args = Access4args { access: all };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::OpAccess(Access4res::Resok4(res))) = response.result {
            assert_eq!(res.access, all);
            assert_eq!(res.supported, all);
        } else {
            panic!("Unexpected response");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_check_access() {
        let request = create_nfs40_server(None).await;
        let args = Access4args {
            access: ACCESS4_READ
                | ACCESS4_LOOKUP
                | ACCESS4_MODIFY
                | ACCESS4_EXTEND
                | ACCESS4_DELETE,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::OpAccess(Access4res::Resok4(res))) = response.result {
            assert_eq!(
                res.supported,
                ACCESS4_READ
                    | ACCESS4_LOOKUP
                    | ACCESS4_MODIFY
                    | ACCESS4_EXTEND
                    | ACCESS4_DELETE
                    | ACCESS4_EXECUTE
            );
            assert_eq!(
                res.access,
                ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY | ACCESS4_EXTEND | ACCESS4_DELETE
            );
        } else {
            panic!("Unexpected response: {:?}", response);
        }
    }
}
