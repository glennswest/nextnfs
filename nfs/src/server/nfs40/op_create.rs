use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{
    Attrlist4, ChangeInfo4, Create4args, Create4res, Create4resok, Createtype4, FileAttr,
    NfsResOp4, NfsStat4,
};

#[async_trait]
impl NfsOperation for Create4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 6: CREATE - Create a Non-regular File Object {:?}, with request {:?}",
            self, request
        );

        let current_filehandle = request.current_filehandle();
        let filehandle = match current_filehandle {
            Some(filehandle) => filehandle,
            None => {
                error!("None filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        if self.objname.is_empty() {
            // If the objname is of zero length, NFS4ERR_INVAL will be returned.
            // The objname is also subject to the normal UTF-8, character support,
            // and name checks.  See Section 12.7 for further discussion.
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errInval,
            };
        }

        // Quota enforcement: reject creation if hard limit already exceeded
        if let Some(qm) = request.quota_manager() {
            if !qm.check_write(0) {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errDquot,
                };
            }
        }

        let (cinfo, attrset) = match self.objtype {
            Createtype4::Nf4lnk(ref linkdata) => {
                // Symlink creation: linkdata is the target path
                let current_dir = if filehandle.file.is_file().unwrap_or(false) {
                    &filehandle.file.parent()
                } else {
                    &filehandle.file
                };
                let link_path = match current_dir.join(self.objname.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("CREATE symlink: invalid path join: {:?}", e);
                        return NfsOpResponse {
                            request,
                            result: None,
                            status: NfsStat4::Nfs4errInval,
                        };
                    }
                };

                let link_vfs_str = link_path.as_str().to_string();
                let link_real = request.file_manager().real_path(&link_vfs_str);
                match std::os::unix::fs::symlink(linkdata, &link_real) {
                    Ok(_) => {
                        request.file_manager().touch_file(filehandle.id).await;
                        let resp = request
                            .file_manager()
                            .get_filehandle_for_path(link_vfs_str)
                            .await;
                        let new_fh = match resp {
                            Ok(fh) => fh,
                            Err(e) => {
                                debug!("FileManagerError {:?}", e);
                                request.unset_filehandle();
                                return NfsOpResponse {
                                    request,
                                    result: None,
                                    status: e.nfs_error,
                                };
                            }
                        };
                        request.set_filehandle(new_fh.clone());
                        (
                            ChangeInfo4 {
                                atomic: true,
                                before: new_fh.attr_change,
                                after: new_fh.attr_change,
                            },
                            Attrlist4::<FileAttr>::new(None),
                        )
                    }
                    Err(e) => {
                        error!("CREATE symlink failed: {:?}", e);
                        let status = match e.kind() {
                            std::io::ErrorKind::PermissionDenied => NfsStat4::Nfs4errAccess,
                            std::io::ErrorKind::AlreadyExists => NfsStat4::Nfs4errExist,
                            _ => NfsStat4::Nfs4errIo,
                        };
                        return NfsOpResponse {
                            request,
                            result: None,
                            status,
                        };
                    }
                }
            }
            Createtype4::Nf4dir => {
                let current_dir = if filehandle.file.is_file().unwrap_or(false) {
                    &filehandle.file.parent()
                } else {
                    &filehandle.file
                };
                let new_dir = match current_dir.join(self.objname.clone()) {
                    Ok(d) => d,
                    Err(e) => {
                        error!("CREATE: invalid path join: {:?}", e);
                        return NfsOpResponse {
                            request,
                            result: None,
                            status: NfsStat4::Nfs4errInval,
                        };
                    }
                };
                let _ = new_dir.create_dir();

                request.file_manager().touch_file(filehandle.id).await;

                let resp = request
                    .file_manager()
                    .get_filehandle_for_path(new_dir.as_str().to_string())
                    .await;
                let filehandle = match resp {
                    Ok(filehandle) => filehandle,
                    Err(e) => {
                        debug!("FileManagerError {:?}", e);
                        request.unset_filehandle();
                        return NfsOpResponse {
                            request,
                            result: None,
                            status: e.nfs_error,
                        };
                    }
                };
                request.set_filehandle(filehandle.clone());

                (
                    ChangeInfo4 {
                        atomic: true,
                        before: filehandle.attr_change,
                        after: filehandle.attr_change,
                    },
                    Attrlist4::<FileAttr>::new(None),
                )
            }
            _ => {
                // https://datatracker.ietf.org/doc/html/rfc7530#section-16.4.2
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errBadtype,
                };
            }
        };

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opcreate(Create4res::Resok4(Create4resok {
                cinfo,
                attrset,
            }))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            export_manager::{QuotaConfig, QuotaManager},
            nfs40::{
                Attrlist4, Create4args, Create4res, Createtype4, Fattr4, NfsResOp4,
                NfsStat4,
            },
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_create_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "testdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_empty_name() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errInval);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_directory() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "newdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opcreate(Create4res::Resok4(resok))) = response.result {
            assert!(resok.cinfo.atomic);
        } else {
            panic!("Expected Opcreate Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_unsupported_type() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Create4args {
            objtype: Createtype4::Nf4sock,
            objname: "testsock".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadtype);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_nested_directory() {
        let request = create_nfs40_server_with_root_fh(None).await;
        // Create parent dir
        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "parent".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Current fh is now "parent" — create child inside it
        let request = response.request;
        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "child".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_dot_hidden_dir() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: ".hidden".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_dir_with_spaces() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "dir with spaces".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_dir_with_special_chars() {
        let names = ["file-with-dashes", "file_with_underscores", "file.with.dots", "UPPERCASE"];
        let request = create_nfs40_server_with_root_fh(None).await;
        let mut req = request;
        for name in &names {
            let args = Create4args {
                objtype: Createtype4::Nf4dir,
                objname: name.to_string(),
                createattrs: Fattr4 {
                    attrmask: Attrlist4(vec![]),
                    attr_vals: Attrlist4(vec![]),
                },
            };
            let response = args.execute(req).await;
            assert_eq!(response.status, NfsStat4::Nfs4Ok, "failed to create dir: {}", name);
            // Reset to root for next iteration
            req = response.request;
            let root_fh = req.file_manager().get_root_filehandle().await.unwrap();
            req.set_filehandle(root_fh);
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_quota_exceeded() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Set hard quota already exceeded
        let qm = QuotaManager::new(QuotaConfig {
            hard_limit_bytes: 100,
            soft_limit_bytes: 50,
        });
        qm.record_write(200); // over hard limit
        request.set_quota_manager(qm);

        let args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "quotadir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errDquot);
    }
}
