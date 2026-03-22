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
        let is_dir = src_vfs.is_dir().unwrap_or(false);
        let result = if is_dir {
            src_vfs.move_dir(&current_fh.file.join(&self.newname).unwrap())
        } else {
            src_vfs.move_file(&current_fh.file.join(&self.newname).unwrap())
        };

        match result {
            Ok(_) => {
                // Touch both parent directories
                request
                    .file_manager()
                    .touch_file(saved_fh.id.clone())
                    .await;
                request
                    .file_manager()
                    .touch_file(current_fh.id.clone())
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
