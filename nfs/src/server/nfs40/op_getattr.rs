use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::{op_pseudo, NfsStat4},
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Fattr4, Getattr4args, Getattr4resok, NfsResOp4};

#[async_trait]
impl NfsOperation for Getattr4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 9: GETATTR - Get Attributes {:?}, with request {:?}",
            self, request
        );

        // If on pseudo-root, return synthetic attrs
        if request.is_pseudo_root() {
            let (answer_attrs, attrs) =
                op_pseudo::pseudo_root_getattr(&self.attr_request);
            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                    status: NfsStat4::Nfs4Ok,
                    obj_attributes: Some(Fattr4 {
                        attrmask: answer_attrs,
                        attr_vals: attrs,
                    }),
                })),
                status: NfsStat4::Nfs4Ok,
            };
        }

        let filehandle = request.current_filehandle();
        match filehandle {
            None => {
                error!("None filehandle");
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                        obj_attributes: None,
                        status: NfsStat4::Nfs4errStale,
                    })),
                    status: NfsStat4::Nfs4errStale,
                }
            }
            Some(filehandle) => {
                let resp = request
                    .file_manager()
                    .filehandle_attrs(&self.attr_request, filehandle);

                let (answer_attrs, attrs) = match resp {
                    Some(inner) => inner,
                    None => {
                        return NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                                obj_attributes: None,
                                status: NfsStat4::Nfs4errServerfault,
                            })),
                            status: NfsStat4::Nfs4errServerfault,
                        };
                    }
                };

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                        status: NfsStat4::Nfs4Ok,
                        obj_attributes: Some(Fattr4 {
                            attrmask: answer_attrs,
                            attr_vals: attrs,
                        }),
                    })),
                    status: NfsStat4::Nfs4Ok,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{Attrlist4, FileAttr, Getattr4args, Getattr4resok, NfsResOp4, NfsStat4},
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errStale);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_root_type() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok { obj_attributes: Some(fattr), .. })) = response.result {
            assert!(!fattr.attrmask.is_empty());
        } else {
            panic!("Expected Opgetattr with attributes");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_multiple_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![
                FileAttr::Type,
                FileAttr::Size,
                FileAttr::Fsid,
                FileAttr::Fileid,
                FileAttr::Mode,
            ]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok { obj_attributes: Some(fattr), .. })) = response.result {
            assert_eq!(fattr.attrmask.len(), 5);
            assert_eq!(fattr.attr_vals.len(), 5);
        } else {
            panic!("Expected Opgetattr with 5 attributes");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_supported_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::SupportedAttrs]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok { obj_attributes: Some(fattr), .. })) = response.result {
            assert_eq!(fattr.attrmask.len(), 1);
            assert_eq!(fattr.attrmask.0[0], FileAttr::SupportedAttrs);
        } else {
            panic!("Expected Opgetattr with SupportedAttrs");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_lease_time() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::LeaseTime]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_on_created_file() {
        let request = create_nfs40_server_with_root_fh(None).await;
        // Create a file first
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("getattr_file").unwrap().create_file().unwrap();
        {
            use std::io::Write;
            let mut f = root_file.join("getattr_file").unwrap().append_file().unwrap();
            f.write_all(b"content").unwrap();
        }
        let mut request = request;
        let fh = request.file_manager()
            .get_filehandle_for_path("getattr_file".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::Size, FileAttr::Type]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok { obj_attributes: Some(fattr), .. })) = response.result {
            assert_eq!(fattr.attrmask.len(), 2);
        } else {
            panic!("Expected Opgetattr with attributes");
        }
    }
}
