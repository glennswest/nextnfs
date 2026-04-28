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

        // Perform the rename via direct filesystem call (preserves inodes).
        //
        // We bypass VfsPath::move_file() because the vfs crate's AltrootFS
        // doesn't implement FileSystem::move_file(), causing it to fall through
        // to a copy-and-delete path that creates a NEW inode. The Linux kernel
        // then detects "fileid changed" and reports ESTALE.
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
        let real_src = request.file_manager().real_path(&src_path);
        let real_dst = request.file_manager().real_path(&dst_path);
        let result = if std::fs::rename(&real_src, &real_dst).is_ok() {
            Ok(())
        } else {
            // Fall back to VFS move for non-physical filesystems (e.g., MemoryFS in tests)
            if is_dir {
                src_vfs.move_dir(&dst_vfs)
            } else {
                src_vfs.move_file(&dst_vfs)
            }
        };

        match result {
            Ok(_) => {
                // Update filehandle database with new path
                request
                    .file_manager()
                    .rename_path(src_path.clone(), dst_path.clone(), dst_vfs.clone())
                    .await;

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

    #[tokio::test]
    #[traced_test]
    async fn test_rename_file() {
        use std::io::Write;
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a file via VFS
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("origfile.txt").unwrap().create_file().unwrap();
        {
            let mut f = root_file.join("origfile.txt").unwrap().append_file().unwrap();
            f.write_all(b"data").unwrap();
        }

        let mut request = request;
        request.save_filehandle(); // save root as source dir
        // current fh is also root (target dir)

        let args = Rename4args {
            oldname: "origfile.txt".to_string(),
            newname: "renamed.txt".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_rename_no_current_fh() {
        use crate::test_utils::create_nfs40_server;
        // No filehandles at all — saved_fh check fails first
        let request = create_nfs40_server(None).await;
        let args = Rename4args {
            oldname: "a".to_string(),
            newname: "b".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_rename_cross_directory() {
        // Rename a dir from root into a subdirectory
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create target directory
        let create_target = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "targetdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_target.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root, create source
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let create_source = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "moveme".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_source.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Save root as source dir, set target as current dir
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);
        request.save_filehandle(); // saved = root (source)

        let target_fh = request.file_manager()
            .get_filehandle_for_path("targetdir".to_string())
            .await.unwrap();
        request.set_filehandle(target_fh); // current = targetdir

        let args = Rename4args {
            oldname: "moveme".to_string(),
            newname: "moved".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_rename_returns_change_info() {
        use crate::server::nfs40::{NfsResOp4, Rename4res};
        let request = create_nfs40_server_with_root_fh(None).await;

        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "ci_old".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);
        request.save_filehandle();

        let args = Rename4args {
            oldname: "ci_old".to_string(),
            newname: "ci_new".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Oprename(Rename4res::Resok4(resok))) => {
                assert!(resok.source_cinfo.atomic);
                assert!(resok.target_cinfo.atomic);
            }
            other => panic!("Expected Oprename Resok4, got {:?}", other),
        }
    }
}
