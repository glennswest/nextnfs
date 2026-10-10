use async_trait::async_trait;
use tracing::debug;

use crate::server::{
    filemanager::RealMeta, operation::NfsOperation, perm::Caller, request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    Access4args, Access4res, Access4resok, NfsFtype4, NfsResOp4, NfsStat4, ACCESS4_DELETE,
    ACCESS4_EXECUTE, ACCESS4_EXTEND, ACCESS4_LOOKUP, ACCESS4_MODIFY, ACCESS4_READ,
};

/// Check POSIX permissions and return the subset of requested NFS access flags
/// that the caller (already squash-mapped, #91) is allowed.
fn check_access(requested: u32, meta: &RealMeta, caller: &Caller, is_dir: bool) -> u32 {
    // Root gets everything
    if caller.is_root() {
        return requested;
    }

    // Owner, group (primary or supplementary) or other bits
    let bits = caller.mode_bits(meta);

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
        // AUTH_NONE and other uid-less flavours are the anonymous user,
        // not root (#101).
        let access = if let Some(fh) = request.current_filehandle() {
            let is_dir = fh.attr_type == NfsFtype4::Nf4dir;
            // Fresh stat (the cached handle's attrs may be stale); the
            // handle's attrs when the object cannot be stat()ed.
            let meta = request
                .file_manager_opt()
                .and_then(|fm| RealMeta::from_path(&fm.real_path(&fh.path)))
                .unwrap_or_else(|| RealMeta {
                    ino: fh.attr_fileid,
                    dev: 0,
                    mode: fh.attr_mode,
                    nlink: fh.attr_nlink as u64,
                    uid: fh.attr_owner.parse::<u32>().unwrap_or(0),
                    gid: fh.attr_owner_group.parse::<u32>().unwrap_or(0),
                    size: fh.attr_size,
                    blocks: 0,
                    atime: 0,
                    atime_nsec: 0,
                    mtime: 0,
                    mtime_nsec: 0,
                    ctime: 0,
                    ctime_nsec: 0,
                });
            check_access(self.access, &meta, &request.caller(), is_dir)
        } else {
            // No filehandle — grant what was requested (best effort)
            self.access
        };
        // A read-only export never grants write access (issue #93)
        let access = if request.is_read_only() {
            access & !(ACCESS4_MODIFY | ACCESS4_EXTEND | ACCESS4_DELETE)
        } else {
            access
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
