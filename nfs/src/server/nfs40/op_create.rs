use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    createattrs::{self, CreateAttrs, DEFAULT_DIR_MODE},
    operation::NfsOperation,
    perm,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    ChangeInfo4, Create4args, Create4res, Create4resok, Createtype4, NfsResOp4, NfsStat4,
};

#[async_trait]
impl NfsOperation for Create4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 6: CREATE - Create a Non-regular File Object {:?}, with request {:?}",
            self, request
        );

        // createattrs are validated before anything is created (#127). A
        // size is for regular files, which CREATE never makes.
        let caller = request.caller();
        let attrs = match CreateAttrs::parse(&self.createattrs)
            .and_then(|a| a.check(&caller).map(|_| a))
        {
            Ok(a) if a.size.is_some() => Err(NfsStat4::Nfs4errInval),
            other => other,
        };
        let attrs = match attrs {
            Ok(a) => a,
            Err(status) => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status,
                };
            }
        };

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

        // A zero-length objname is NFS4ERR_INVAL; `.`, `..`, `/` and NUL are
        // NFS4ERR_BADNAME (RFC 7530 §12.7): the new object's parent is the
        // current directory, the one whose permissions were checked (#129).
        if let Err(status) = perm::check_name(&self.objname) {
            return NfsOpResponse {
                request,
                result: None,
                status,
            };
        }

        let (cinfo, attrset) = match self.objtype {
            Createtype4::Nf4lnk(ref linkdata) => {
                // Symlink creation: linkdata is the target path
                debug!("CREATE NF4LNK: name={:?} target={:?}", self.objname, linkdata);
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
                let parent_vfs_str = current_dir.as_str().to_string();
                let before_change = request.file_manager().dir_change_attr(&parent_vfs_str);
                // symlinkat in the parent resolved beneath the export root
                // (#123); the target is stored as given, never resolved.
                let made = request
                    .file_manager()
                    .beneath()
                    .symlink(std::ffi::OsStr::new(linkdata.as_str()), &link_vfs_str);
                match made {
                    Ok(_) => {
                        request.file_manager().touch_file(filehandle.id).await;
                        let after_change = request.file_manager().dir_change_attr(&parent_vfs_str);
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
                        // Owner and group (a symlink has no settable mode).
                        let attrset =
                            createattrs::finish_new_object(&mut request, &caller, &attrs).await;
                        (
                            ChangeInfo4 {
                                atomic: false,
                                before: before_change,
                                after: after_change,
                            },
                            attrset,
                        )
                    }
                    Err(e) => {
                        error!("CREATE symlink failed: {:?}", e);
                        // errno mapped as for every create (EDQUOT → DQUOT, EXDEV → XDEV, #146).
                let status = crate::server::createattrs::io_status(&e);
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
                let parent_vfs_str = current_dir.as_str().to_string();
                let before_change = request.file_manager().dir_change_attr(&parent_vfs_str);
                // An existing name (file, directory, or a symlink, dangling
                // or not) is NFS4ERR_EXIST: a CREATE that "succeeded" on it
                // would hand the existing object to the caller (#122).
                let exists = new_dir.exists().unwrap_or(false)
                    || request.file_manager().beneath().exists(new_dir.as_str());
                let fm = request.file_manager();
                let created = if exists {
                    Err(NfsStat4::Nfs4errExist)
                } else if !fm.vfs_only() {
                    // mkdirat with the requested permission bits from the
                    // start (#127), in the parent resolved beneath the
                    // export root (#123).
                    fm.beneath()
                        .create_dir(new_dir.as_str(), attrs.initial_mode(DEFAULT_DIR_MODE))
                        .map_err(|e| createattrs::io_status(&e))
                } else {
                    new_dir.create_dir().map_err(|e| match e.kind() {
                        vfs::error::VfsErrorKind::DirectoryExists
                        | vfs::error::VfsErrorKind::FileExists => NfsStat4::Nfs4errExist,
                        vfs::error::VfsErrorKind::FileNotFound => NfsStat4::Nfs4errNoent,
                        vfs::error::VfsErrorKind::IoError(io) => match io.kind() {
                            std::io::ErrorKind::PermissionDenied => NfsStat4::Nfs4errAccess,
                            std::io::ErrorKind::AlreadyExists => NfsStat4::Nfs4errExist,
                            _ => NfsStat4::Nfs4errIo,
                        },
                        _ => NfsStat4::Nfs4errIo,
                    })
                };
                if let Err(status) = created {
                    debug!("CREATE dir {:?}: {:?}", new_dir.as_str(), status);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status,
                    };
                }

                request.file_manager().touch_file(filehandle.id).await;
                let after_change = request.file_manager().dir_change_attr(&parent_vfs_str);

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
                // Owner, then the exact mode (set-id/sticky bits included).
                let attrset = createattrs::finish_new_object(&mut request, &caller, &attrs).await;

                (
                    ChangeInfo4 {
                        atomic: false,
                        before: before_change,
                        after: after_change,
                    },
                    attrset,
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
        if let Some(NfsResOp4::Opcreate(Create4res::Resok4(_resok))) = response.result {
            // cinfo before/after are read from the underlying filesystem;
            // on MemoryFS they're zero, but the response shape is what
            // matters here.
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
}
