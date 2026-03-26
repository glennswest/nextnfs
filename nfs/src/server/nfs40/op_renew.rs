use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{NfsResOp4, NfsStat4, Renew4args, Renew4res};

#[async_trait]
impl NfsOperation for Renew4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 30: RENEW - Renew a Lease {:?}, with request {:?}",
            self, request
        );
        let res = request.client_manager().renew_leases(self.clientid).await;
        match res {
            Ok(_) => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Oprenew(Renew4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            },
            Err(e) => {
                error!("Renew err {:?}", e);
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oprenew(Renew4res {
                        status: e.nfs_error.clone(),
                    })),
                    status: e.nfs_error,
                }
            }
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use crate::{
        server::{
            nfs40::{NfsResOp4, NfsStat4, Renew4args, Renew4res, SetClientId4res, SetClientIdConfirm4args},
            operation::NfsOperation,
        },
        test_utils::{create_client, create_nfs40_server},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_renew_unknown_client() {
        // Renewing a client that was never registered returns stale clientid
        let request = create_nfs40_server(None).await;
        let args = Renew4args { clientid: 999 };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errStaleClientid);
        match &response.result {
            Some(NfsResOp4::Oprenew(Renew4res { status })) => {
                assert_eq!(*status, NfsStat4::Nfs4errStaleClientid);
            }
            other => panic!("Expected Oprenew with error status, got {:?}", other),
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_renew_leases() {
        let request = create_nfs40_server(None).await;

        let client1 = create_client(
            [23, 213, 67, 174, 197, 95, 35, 119],
            "Linux NFSv4.0 LAPTOP/127.0.0.1".to_string(),
        );

        // setup client
        let res_client1 = client1.execute(request).await;
        let (client1_id, client1_confirm) = match res_client1.result.unwrap() {
            NfsResOp4::Opsetclientid(SetClientId4res::Resok4(resok)) => {
                (resok.clientid, resok.setclientid_confirm)
            }
            _ => panic!("Unexpected response"),
        };

        // confirm client1
        let conf_client1 = SetClientIdConfirm4args {
            clientid: client1_id,
            setclientid_confirm: client1_confirm,
        };
        let res_confirm = conf_client1.execute(res_client1.request).await;
        assert_eq!(res_confirm.status, NfsStat4::Nfs4Ok);

        // renew client1 — should succeed
        let renew_client1 = Renew4args {
            clientid: client1_id,
        };
        let res_renew = renew_client1.execute(res_confirm.request).await;
        assert_eq!(res_renew.status, NfsStat4::Nfs4Ok);

        // renew stale client — should fail
        let renew_stale = Renew4args { clientid: 50 };
        let res_renew_stale = renew_stale.execute(res_renew.request).await;
        assert_eq!(res_renew_stale.status, NfsStat4::Nfs4errStaleClientid);
    }
}
