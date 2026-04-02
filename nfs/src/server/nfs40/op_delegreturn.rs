use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{DelegReturn4args, DelegReturn4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for DelegReturn4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 8: DELEGRETURN - Return Delegation {:?}",
            self.deleg_stateid
        );

        let filehandle = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("DELEGRETURN: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opdelegreturn(DelegReturn4res {
                        status: NfsStat4::Nfs4errFhexpired,
                    })),
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        let fh_id = filehandle.id;
        let status = request
            .file_manager()
            .return_delegation(fh_id, self.deleg_stateid.clone())
            .await;

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opdelegreturn(DelegReturn4res {
                status: status.clone(),
            })),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{
                Attrlist4, Create4args, Createtype4, DelegReturn4args, Fattr4, NfsResOp4,
                NfsStat4, Open4args, Open4res, Open4resok, OpenClaim4, OpenDelegation4,
                OpenFlag4, Stateid4,
            },
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use nextnfs_proto::nfs4_proto::OpenOwner4;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_delegreturn_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = DelegReturn4args {
            deleg_stateid: Stateid4 {
                seqid: 1,
                other: [0; 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_delegreturn_bad_stateid() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = DelegReturn4args {
            deleg_stateid: Stateid4 {
                seqid: 999,
                other: [0xFF; 12],
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadStateid);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_open_grants_delegation_then_return() {
        let request = create_nfs40_server_with_root_fh(None).await;

        // Create a file to open
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "testfile".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Reset to root for OPEN
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // OPEN for reading — should get a delegation
        let open_args = Open4args {
            seqid: 1,
            share_access: 1, // OPEN4_SHARE_ACCESS_READ
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: vec![1, 2, 3],
            },
            openhow: OpenFlag4::Open4Nocreate,
            claim: OpenClaim4::ClaimNull("testfile".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Extract the delegation stateid
        let deleg_stateid = if let Some(NfsResOp4::Opopen(Open4res::Resok4(Open4resok {
            delegation: OpenDelegation4::Read(ref deleg),
            ..
        }))) = response.result
        {
            deleg.stateid.clone()
        } else {
            panic!("Expected read delegation from OPEN");
        };

        // DELEGRETURN with the correct stateid
        let request = response.request;
        let delegreturn_args = DelegReturn4args {
            deleg_stateid,
        };
        let response = delegreturn_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
