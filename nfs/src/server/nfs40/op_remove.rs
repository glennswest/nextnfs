use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::{ChangeInfo4, NfsStat4},
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{NfsResOp4, Remove4args, Remove4res};

#[async_trait]
impl NfsOperation for Remove4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 28: REMOVE - Remove File System Object {:?}, with request {:?}",
            self, request
        );
        let filehandle = request.current_filehandle();
        match filehandle {
            None => {
                error!("None filehandle");
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opremove(Remove4res {
                        status: NfsStat4::Nfs4errStale,
                        cinfo: ChangeInfo4 {
                            atomic: false,
                            before: 0,
                            after: 0,
                        },
                    })),
                    status: NfsStat4::Nfs4errStale,
                };
            }
            Some(filehandle) => {
                let path = match filehandle.file.join(self.target.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("REMOVE: invalid path join: {:?}", e);
                        return NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Opremove(Remove4res {
                                status: NfsStat4::Nfs4errInval,
                                cinfo: ChangeInfo4 {
                                    atomic: false,
                                    before: 0,
                                    after: 0,
                                },
                            })),
                            status: NfsStat4::Nfs4errInval,
                        };
                    }
                };
                let res = request.file_manager().remove_file(path).await;
                match res {
                    Ok(_) => NfsOpResponse {
                        request,
                        result: Some(NfsResOp4::Opremove(Remove4res {
                            status: NfsStat4::Nfs4Ok,
                            cinfo: ChangeInfo4 {
                                atomic: false,
                                before: 0,
                                after: 0,
                            },
                        })),
                        status: NfsStat4::Nfs4Ok,
                    },
                    Err(e) => {
                        error!("REMOVE failed: {:?}", e);
                        NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Opremove(Remove4res {
                                status: NfsStat4::Nfs4errIo,
                                cinfo: ChangeInfo4 {
                                    atomic: false,
                                    before: 0,
                                    after: 0,
                                },
                            })),
                            status: NfsStat4::Nfs4errIo,
                        }
                    }
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
                Attrlist4, Create4args, Createtype4, Fattr4, NfsStat4, Remove4args,
            },
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_remove_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Remove4args {
            target: "anything".to_string(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errStale);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_remove_nonexistent() {
        // MemoryFS remove on nonexistent path succeeds silently;
        // the filemanager actor handles the removal, so this returns Ok.
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Remove4args {
            target: "nosuchdir".to_string(),
        };
        let response = args.execute(request).await;
        // Verify the operation completes (either Ok or specific error)
        assert!(
            response.status == NfsStat4::Nfs4Ok || response.status == NfsStat4::Nfs4errIo
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_remove_directory() {
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a directory first
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "rmdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root and remove
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let remove_args = Remove4args {
            target: "rmdir".to_string(),
        };
        let response = remove_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_remove_file() {
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create file via VFS
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("removeme.txt").unwrap().create_file().unwrap();

        let remove_args = Remove4args {
            target: "removeme.txt".to_string(),
        };
        let response = remove_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
