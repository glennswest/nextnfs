use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{
    ChangeInfo4, Link4args, Link4res, Link4resok, NfsResOp4, NfsStat4,
};

#[async_trait]
impl NfsOperation for Link4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 11: LINK {:?}", self);

        // SAVED_FH is the source object, CURRENT_FH is the target directory
        let saved_fh = match request.saved_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errRestorefh,
                };
            }
        };

        let target_dir = match request.current_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // Create hard link: target_dir/newname -> saved_fh
        let target_path = target_dir.file.join(&self.newname);
        let target_path = match target_path {
            Ok(p) => p,
            Err(_) => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errInval,
                };
            }
        };

        // Use std::fs::hard_link with real filesystem paths via export_root
        let source_vfs_path = saved_fh.file.as_str();
        if source_vfs_path.is_empty() || source_vfs_path == "/" {
            // Can't hard-link the root
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errInval,
            };
        }

        let source_real = request.file_manager().real_path(source_vfs_path);
        let target_real = request.file_manager().real_path(target_path.as_str());

        match std::fs::hard_link(&source_real, &target_real) {
            Ok(_) => {
                let change_before = target_dir.attr_change;
                let change_after = change_before + 1;

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplink(Link4res::Resok4(Link4resok {
                        cinfo: ChangeInfo4 {
                            atomic: true,
                            before: change_before,
                            after: change_after,
                        },
                    }))),
                    status: NfsStat4::Nfs4Ok,
                }
            }
            Err(e) => {
                debug!("LINK failed: {:?}", e);
                let status = match e.kind() {
                    std::io::ErrorKind::PermissionDenied => NfsStat4::Nfs4errAccess,
                    std::io::ErrorKind::AlreadyExists => NfsStat4::Nfs4errExist,
                    _ => NfsStat4::Nfs4errIo,
                };
                NfsOpResponse {
                    request,
                    result: None,
                    status,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_link_no_saved_fh() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Link4args {
            newname: "hardlink".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errRestorefh);
    }

    #[tokio::test]
    async fn test_link_no_current_fh() {
        let request = create_nfs40_server(None).await;
        let args = Link4args {
            newname: "hardlink".to_string(),
        };
        let response = args.execute(request).await;
        // No saved fh either, so should get Nfs4errRestorefh first
        assert_eq!(response.status, NfsStat4::Nfs4errRestorefh);
    }

    #[tokio::test]
    async fn test_link_root_source_rejected() {
        // Saving the root fh then trying to hard-link it should fail with Nfs4errInval
        let mut request = create_nfs40_server_with_root_fh(None).await;
        request.save_filehandle();
        let args = Link4args {
            newname: "root_link".to_string(),
        };
        let response = args.execute(request).await;
        // Root source path is "/" — cannot hard-link root
        assert_eq!(response.status, NfsStat4::Nfs4errInval);
    }

    #[tokio::test]
    async fn test_link_source_file_on_memoryfs() {
        use crate::server::operation::NfsOperation as _;
        use crate::server::nfs40::{Create4args, Createtype4, Fattr4, Attrlist4};
        // Create a file so we have a non-root saved fh
        let request = create_nfs40_server_with_root_fh(None).await;
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "srcdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        // srcdir is now current fh — save it
        let mut request = response.request;
        request.save_filehandle();
        // Reset to root as target directory
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let args = Link4args {
            newname: "linked_dir".to_string(),
        };
        let response = args.execute(request).await;
        // MemoryFS paths don't map to real filesystem — hard_link will fail with Io error
        assert!(
            response.status == NfsStat4::Nfs4errIo
            || response.status == NfsStat4::Nfs4errAccess
            || response.status == NfsStat4::Nfs4errExist
        );
    }
}
