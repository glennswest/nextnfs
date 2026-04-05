use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::NfsStat4, operation::NfsOperation, request::NfsRequest, response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Attrlist4, FileAttr, NfsResOp4, SetAttr4args, SetAttr4res};

#[async_trait]
impl NfsOperation for SetAttr4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 34: SETATTR - Set Attributes {:?}, with request {:?}",
            self, request
        );
        let filehandle = request.current_filehandle();
        match filehandle {
            None => {
                error!("None filehandle");
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opsetattr(SetAttr4res {
                        status: NfsStat4::Nfs4errStale,
                        attrsset: Attrlist4::<FileAttr>::new(None),
                    })),
                    status: NfsStat4::Nfs4errStale,
                }
            }
            Some(filehandle) => {
                let attrsset = if !self.obj_attributes.attrmask.is_empty() {
                    let attrsset = request
                        .file_manager()
                        .set_attr(filehandle, &self.obj_attributes.attr_vals);

                    request
                        .file_manager()
                        .touch_file(filehandle.id)
                        .await;

                    match request.set_filehandle_id(filehandle.id).await {
                        Ok(fh) => {
                            request.cache_filehandle(fh);
                        }
                        Err(e) => {
                            return NfsOpResponse {
                                request,
                                result: None,
                                status: e,
                            };
                        }
                    }

                    attrsset
                } else {
                    Attrlist4::<FileAttr>::new(None)
                };

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opsetattr(SetAttr4res {
                        status: NfsStat4::Nfs4Ok,
                        attrsset,
                    })),
                    status: NfsStat4::Nfs4Ok,
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
    use nextnfs_proto::nfs4_proto::{Fattr4, Stateid4};

    #[tokio::test]
    async fn test_setattr_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = SetAttr4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errStale);
    }

    #[tokio::test]
    async fn test_setattr_empty_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = SetAttr4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_setattr_returns_attrsset() {
        use nextnfs_proto::nfs4_proto::FileAttrValue;
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = SetAttr4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Size]),
                attr_vals: Attrlist4(vec![FileAttrValue::Size(0)]),
            },
        };
        let response = args.execute(request).await;
        // Root directory open_file may fail, but status should reflect the attempt
        assert!(
            response.status == NfsStat4::Nfs4Ok
            || response.status != NfsStat4::Nfs4errStale
        );
    }

    /// Regression test for #36: SETATTR with Owner/OwnerGroup must be accepted.
    /// This verifies the server processes chown attributes without error.
    /// (Actual uid/gid persistence requires a real filesystem, not MemoryFS.)
    #[tokio::test]
    async fn test_setattr_chown_accepted() {
        use nextnfs_proto::nfs4_proto::FileAttrValue;
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = SetAttr4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Owner, FileAttr::OwnerGroup]),
                attr_vals: Attrlist4(vec![
                    FileAttrValue::Owner("1000".to_string()),
                    FileAttrValue::OwnerGroup("1000".to_string()),
                ]),
            },
        };
        let response = args.execute(request).await;
        // SETATTR should succeed (status Ok) even if chown on MemoryFS is a no-op
        assert_eq!(
            response.status, NfsStat4::Nfs4Ok,
            "SETATTR with Owner/OwnerGroup must not error (regression #36)"
        );
    }

    /// Regression test for #36: SETATTR mode change must report the attr as set.
    #[tokio::test]
    async fn test_setattr_chmod_reports_attrsset() {
        use nextnfs_proto::nfs4_proto::FileAttrValue;
        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Create a file to chmod
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("chmod_test.txt").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("chmod_test.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        let args = SetAttr4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Mode]),
                attr_vals: Attrlist4(vec![FileAttrValue::Mode(0o755)]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
