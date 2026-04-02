use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::{op_pseudo, NfsStat4},
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Fattr4, Getattr4args, Getattr4resok, NfsResOp4};

use crate::server::filemanager::QuotaInfo;

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
                let quota_info = request.quota_manager().map(|qm| {
                    // 1TB default filesystem size
                    const DEFAULT_SPACE_TOTAL: u64 = 1_099_511_627_776;
                    let used = qm.bytes_used();
                    QuotaInfo {
                        quota_avail_hard: qm.quota_avail_hard(),
                        quota_avail_soft: qm.quota_avail_soft(),
                        quota_used: used,
                        space_total: DEFAULT_SPACE_TOTAL,
                        space_free: DEFAULT_SPACE_TOTAL.saturating_sub(used),
                        space_avail: DEFAULT_SPACE_TOTAL.saturating_sub(used),
                    }
                });
                let resp = request
                    .file_manager()
                    .filehandle_attrs(&self.attr_request, filehandle, quota_info.as_ref());

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
            nfs40::{
                Attrlist4, FileAttr, FileAttrValue, Getattr4args, Getattr4resok, NfsResOp4,
                NfsStat4,
            },
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

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_quota_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![
                FileAttr::QuotaAvailHard,
                FileAttr::QuotaAvailSoft,
                FileAttr::QuotaUsed,
            ]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            assert_eq!(fattr.attrmask.len(), 3);
            assert_eq!(fattr.attr_vals.len(), 3);
            // Default QuotaManager has 0 limits (unlimited) so avail=0, used=0
            assert!(matches!(fattr.attr_vals.0[0], FileAttrValue::QuotaAvailHard(0)));
            assert!(matches!(fattr.attr_vals.0[1], FileAttrValue::QuotaAvailSoft(0)));
            assert!(matches!(fattr.attr_vals.0[2], FileAttrValue::QuotaUsed(0)));
        } else {
            panic!("Expected Opgetattr with quota attributes");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_space_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![
                FileAttr::SpaceAvail,
                FileAttr::SpaceFree,
                FileAttr::SpaceTotal,
            ]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            assert_eq!(fattr.attrmask.len(), 3);
            assert_eq!(fattr.attr_vals.len(), 3);
            // No QuotaManager in test helper, so values default to 0
            assert!(matches!(fattr.attr_vals.0[0], FileAttrValue::SpaceAvail(0)));
            assert!(matches!(fattr.attr_vals.0[1], FileAttrValue::SpaceFree(0)));
            assert!(matches!(fattr.attr_vals.0[2], FileAttrValue::SpaceTotal(0)));
        } else {
            panic!("Expected Opgetattr with space attributes");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_supported_includes_quota() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::SupportedAttrs]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            if let FileAttrValue::SupportedAttrs(supported) = &fattr.attr_vals.0[0] {
                assert!(supported.0.contains(&FileAttr::QuotaAvailHard));
                assert!(supported.0.contains(&FileAttr::QuotaAvailSoft));
                assert!(supported.0.contains(&FileAttr::QuotaUsed));
                assert!(supported.0.contains(&FileAttr::SpaceAvail));
                assert!(supported.0.contains(&FileAttr::SpaceFree));
                assert!(supported.0.contains(&FileAttr::SpaceTotal));
            } else {
                panic!("Expected SupportedAttrs value");
            }
        } else {
            panic!("Expected Opgetattr with SupportedAttrs");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_files_avail_free_total() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![
                FileAttr::FilesAvail,
                FileAttr::FilesFree,
                FileAttr::FilesTotal,
            ]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            assert_eq!(fattr.attrmask.len(), 3);
        } else {
            panic!("Expected Opgetattr with 3 file count attributes");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_time_delta() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::TimeDelta]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            assert_eq!(fattr.attrmask.len(), 1);
            if let FileAttrValue::TimeDelta(td) = &fattr.attr_vals.0[0] {
                assert_eq!(td.seconds, 0);
                assert_eq!(td.nseconds, 1);
            } else {
                panic!("Expected TimeDelta value");
            }
        } else {
            panic!("Expected Opgetattr with TimeDelta");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_case_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![
                FileAttr::CaseInsensitive,
                FileAttr::CasePreserving,
            ]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            assert_eq!(fattr.attrmask.len(), 2);
            if let FileAttrValue::CaseInsensitive(ci) = &fattr.attr_vals.0[0] {
                assert!(!ci); // POSIX is case-sensitive
            } else {
                panic!("Expected CaseInsensitive value");
            }
            if let FileAttrValue::CasePreserving(cp) = &fattr.attr_vals.0[1] {
                assert!(cp); // POSIX preserves case
            } else {
                panic!("Expected CasePreserving value");
            }
        } else {
            panic!("Expected Opgetattr with case attributes");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_mounted_on_fileid() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::MountedOnFileid]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            assert_eq!(fattr.attrmask.len(), 1);
        } else {
            panic!("Expected Opgetattr with MountedOnFileid");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_time_create() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::TimeCreate]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            assert_eq!(fattr.attrmask.len(), 1);
        } else {
            panic!("Expected Opgetattr with TimeCreate");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_getattr_supported_includes_new_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Getattr4args {
            attr_request: Attrlist4(vec![FileAttr::SupportedAttrs]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opgetattr(Getattr4resok {
            obj_attributes: Some(fattr), ..
        })) = response.result
        {
            if let FileAttrValue::SupportedAttrs(supported) = &fattr.attr_vals.0[0] {
                assert!(supported.0.contains(&FileAttr::FilesAvail));
                assert!(supported.0.contains(&FileAttr::FilesFree));
                assert!(supported.0.contains(&FileAttr::FilesTotal));
                assert!(supported.0.contains(&FileAttr::TimeDelta));
                assert!(supported.0.contains(&FileAttr::TimeCreate));
                assert!(supported.0.contains(&FileAttr::MountedOnFileid));
                assert!(supported.0.contains(&FileAttr::CaseInsensitive));
                assert!(supported.0.contains(&FileAttr::CasePreserving));
            } else {
                panic!("Expected SupportedAttrs value");
            }
        } else {
            panic!("Expected Opgetattr with SupportedAttrs");
        }
    }
}
