use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::{op_pseudo, Lookup4res, NfsResOp4},
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Lookup4args, NfsStat4};

#[async_trait]
impl NfsOperation for Lookup4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 15: LOOKUP - Look Up Filename {:?}, with request {:?}",
            self, request
        );

        // If on pseudo-root, resolve export name
        if request.is_pseudo_root() {
            let em = request.export_manager();
            if let Some((info, _fm)) = em.get_export_by_name(&self.objname).await {
                // Switch to this export
                request.set_export(info.export_id).await;
                match request.file_manager().get_root_filehandle().await {
                    Ok(mut root_fh) => {
                        // Stamp export_id into the filehandle
                        op_pseudo::stamp_export_id(&mut root_fh.id, info.export_id);
                        request.set_filehandle(root_fh);
                        return NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Oplookup(Lookup4res {
                                status: NfsStat4::Nfs4Ok,
                            })),
                            status: NfsStat4::Nfs4Ok,
                        };
                    }
                    Err(e) => {
                        error!("Failed to get export root: {:?}", e);
                        return NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Oplookup(Lookup4res {
                                status: NfsStat4::Nfs4errServerfault,
                            })),
                            status: NfsStat4::Nfs4errServerfault,
                        };
                    }
                }
            } else {
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplookup(Lookup4res {
                        status: NfsStat4::Nfs4errNoent,
                    })),
                    status: NfsStat4::Nfs4errNoent,
                };
            }
        }

        let current_fh = request.current_filehandle();
        let filehandle = match current_fh {
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

        let mut path = filehandle.path.clone();
        if path == "/" {
            path.push_str(self.objname.as_str());
        } else {
            path.push('/');
            path.push_str(self.objname.as_str());
        }

        debug!("lookup {:?}", path);

        let resp = request.file_manager().get_filehandle_for_path(path).await;
        let filehandle = match resp {
            Ok(filehandle) => filehandle,
            Err(e) => {
                // a missing file during lookup is not an error
                debug!("FileManagerError {:?}", e);
                request.unset_filehandle();
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplookup(Lookup4res {
                        status: e.nfs_error.clone(),
                    })),
                    status: e.nfs_error,
                };
            }
        };

        // lookup sets the current filehandle to the looked up filehandle
        request.set_filehandle(filehandle);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Oplookup(Lookup4res {
                status: NfsStat4::Nfs4Ok,
            })),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{
                Attrlist4, Create4args, Createtype4, Fattr4, Lookup4args, Lookup4res,
                NfsResOp4, NfsStat4,
            },
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_lookup_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Lookup4args {
            objname: "anything".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_lookup_nonexistent() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Lookup4args {
            objname: "nosuchfile".to_string(),
        };
        let response = args.execute(request).await;
        // Should fail — file doesn't exist
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_lookup_after_create() {
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a directory first
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "lookupdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Chain the same request — CREATE set fh to the new dir,
        // so we need to reset to root for the lookup.
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let lookup_args = Lookup4args {
            objname: "lookupdir".to_string(),
        };
        let response = lookup_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Oplookup(Lookup4res { status })) = response.result {
            assert_eq!(status, NfsStat4::Nfs4Ok);
        } else {
            panic!("Expected Oplookup result");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_lookup_in_subdirectory() {
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create parent dir
        let create_parent = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "parent".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_parent.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Current fh is now "parent" — create child inside it
        let request = response.request;
        let create_child = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "child".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_child.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to parent for lookup
        let mut request = response.request;
        let parent_fh = request.file_manager()
            .get_filehandle_for_path("parent".to_string())
            .await.unwrap();
        request.set_filehandle(parent_fh);

        let lookup_args = Lookup4args {
            objname: "child".to_string(),
        };
        let response = lookup_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_lookup_miss_unsets_filehandle() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let lookup_args = Lookup4args {
            objname: "missing_file".to_string(),
        };
        let response = lookup_args.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
        // After a failed lookup, current filehandle should be unset
        assert!(response.request.current_filehandle().is_none());
    }
}
