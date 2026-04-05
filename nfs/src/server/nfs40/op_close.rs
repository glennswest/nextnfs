use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{Close4args, Close4res, NfsResOp4, NfsStat4, Stateid4};

#[async_trait]
impl NfsOperation for Close4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 4: CLOSE - Close File {:?}, with request {:?}",
            self, request
        );

        let current_filehandle = match request.current_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                error!("CLOSE: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // Flush write cache before closing to ensure data durability
        if current_filehandle.write_cache.is_some() {
            if let Ok(wc) = request.file_manager().get_write_cache_handle(current_filehandle.clone()).await {
                wc.commit().await;
            }
        }

        // Release the open stateid from lockdb to prevent resource exhaustion
        request.file_manager().close_file(self.open_stateid.other).await;

        request.drop_filehandle_from_cache(current_filehandle.id);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opclose(Close4res::OpenStateid(Stateid4 {
                seqid: self.seqid,
                other: self.open_stateid.other,
            }))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_close_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Close4args {
            seqid: 1,
            open_stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_close_with_filehandle() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Close4args {
            seqid: 1,
            open_stateid: Stateid4 {
                seqid: 0,
                other: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opclose(Close4res::OpenStateid(stateid))) => {
                assert_eq!(stateid.seqid, 1);
                assert_eq!(stateid.other, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
            }
            other => panic!("Expected Opclose, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_close_uses_args_seqid() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Close4args {
            seqid: 42,
            open_stateid: Stateid4 {
                seqid: 7,
                other: [10; 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::Opclose(Close4res::OpenStateid(stateid))) => {
                assert_eq!(stateid.seqid, 42);
                assert_eq!(stateid.other, [10; 12]);
            }
            other => panic!("Expected Opclose, got {:?}", other),
        }
    }

    /// Regression test for #34: CLOSE then re-OPEN+OPEN_CONFIRM must succeed.
    /// The CLOSE stateid cleanup must not break subsequent opens on new files.
    #[tokio::test]
    async fn test_close_then_reopen_confirm_succeeds() {
        use nextnfs_proto::nfs4_proto::{
            Attrlist4, CreateHow4, Fattr4, Open4args, Open4res, OpenClaim4,
            OpenConfirm4args, OpenConfirm4res, OpenFlag4, OpenOwner4,
        };

        let request = create_nfs40_server_with_root_fh(None).await;

        // OPEN (create) first file
        let open1 = Open4args {
            seqid: 1,
            share_access: 2,
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"close_reopen".to_vec() },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("file1.txt".to_string()),
        };
        let response = open1.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let stateid1 = match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(r))) => r.stateid.clone(),
            other => panic!("Expected Opopen, got {:?}", other),
        };

        // CLOSE file1
        let close_args = Close4args { seqid: 1, open_stateid: stateid1 };
        let response = close_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root fh for second OPEN
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // OPEN (create) second file
        let open2 = Open4args {
            seqid: 2,
            share_access: 2,
            share_deny: 0,
            owner: OpenOwner4 { clientid: 1, owner: b"close_reopen".to_vec() },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("file2.txt".to_string()),
        };
        let response = open2.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let stateid2 = match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(r))) => r.stateid.clone(),
            other => panic!("Expected Opopen, got {:?}", other),
        };

        // OPEN_CONFIRM on file2 must succeed despite file1 having been CLOSEd
        let confirm = OpenConfirm4args { open_stateid: stateid2, seqid: 2 };
        let response = confirm.execute(response.request).await;
        assert_eq!(
            response.status, NfsStat4::Nfs4Ok,
            "OPEN_CONFIRM after CLOSE+reOPEN must succeed (regression #34)"
        );
        match &response.result {
            Some(NfsResOp4::OpopenConfirm(OpenConfirm4res::Resok4(_))) => {}
            other => panic!("Expected OpopenConfirm Resok4, got {:?}", other),
        }
    }
}
