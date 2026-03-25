//! RENAME operation — rename/move a file or directory.
//!
//! Uses SAVED_FH as source directory and CURRENT_FH as target directory.

use async_trait::async_trait;

use crate::server::operation::NfsOperation;
use crate::server::request::NfsRequest;
use crate::server::response::NfsOpResponse;
use nextnfs_proto::nfs4_proto::*;
use tracing::{debug, error};

#[async_trait]
impl NfsOperation for Rename4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("RENAME: {} -> {}", self.oldname, self.newname);

        // Get saved filehandle (source directory)
        let saved_fh = match request.saved_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                error!("RENAME: no saved filehandle (source directory)");
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oprename(Rename4res::Resok4(Rename4resok {
                        source_cinfo: ChangeInfo4 {
                            atomic: false,
                            before: 0,
                            after: 0,
                        },
                        target_cinfo: ChangeInfo4 {
                            atomic: false,
                            before: 0,
                            after: 0,
                        },
                    }))),
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // Get current filehandle (target directory)
        let current_fh = match request.current_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                error!("RENAME: no current filehandle (target directory)");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // Construct source and destination paths
        let src_path = format!("{}/{}", saved_fh.path.trim_end_matches('/'), self.oldname);
        let dst_path = format!(
            "{}/{}",
            current_fh.path.trim_end_matches('/'),
            self.newname
        );

        debug!("RENAME: {} -> {}", src_path, dst_path);

        // Check source exists
        let src_vfs = match saved_fh.file.join(&self.oldname) {
            Ok(p) => p,
            Err(_) => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNoent,
                };
            }
        };

        if !src_vfs.exists().unwrap_or(false) {
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errNoent,
            };
        }

        // Perform the rename via VFS
        let dst_vfs = match current_fh.file.join(&self.newname) {
            Ok(p) => p,
            Err(_) => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errInval,
                };
            }
        };
        let is_dir = src_vfs.is_dir().unwrap_or(false);
        let result = if is_dir {
            src_vfs.move_dir(&dst_vfs)
        } else {
            src_vfs.move_file(&dst_vfs)
        };

        match result {
            Ok(_) => {
                // Touch both parent directories
                request
                    .file_manager()
                    .touch_file(saved_fh.id)
                    .await;
                request
                    .file_manager()
                    .touch_file(current_fh.id)
                    .await;

                let change = ChangeInfo4 {
                    atomic: true,
                    before: saved_fh.attr_change,
                    after: saved_fh.attr_change + 1,
                };

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oprename(Rename4res::Resok4(Rename4resok {
                        source_cinfo: change.clone(),
                        target_cinfo: ChangeInfo4 {
                            atomic: true,
                            before: current_fh.attr_change,
                            after: current_fh.attr_change + 1,
                        },
                    }))),
                    status: NfsStat4::Nfs4Ok,
                }
            }
            Err(e) => {
                error!("RENAME failed: {:?}", e);
                NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errServerfault,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{
                Attrlist4, Create4args, Createtype4, Fattr4, NfsStat4, Rename4args,
            },
            operation::NfsOperation,
        },
        test_utils::create_nfs40_server_with_root_fh,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_rename_no_saved_fh() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Rename4args {
            oldname: "a".to_string(),
            newname: "b".to_string(),
        };
        let response = args.execute(request).await;
        // No saved filehandle set → Nfs4errNofilehandle
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_rename_nonexistent_source() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Save the root as the source directory
        request.save_filehandle();

        let args = Rename4args {
            oldname: "nonexistent".to_string(),
            newname: "target".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNoent);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_rename_directory() {
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a directory
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "oldname".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root, save as source, set as current (target)
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);
        request.save_filehandle();

        let args = Rename4args {
            oldname: "oldname".to_string(),
            newname: "newname".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
