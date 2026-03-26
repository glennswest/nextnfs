use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    clientmanager::ClientCallback, operation::NfsOperation, request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    NfsResOp4, NfsStat4, SetClientId4args, SetClientId4res, SetClientId4resok,
};

#[async_trait]
impl NfsOperation for SetClientId4args {
    /// The client uses the SETCLIENTID operation to notify the server of its
    /// intention to use a particular client identifier, callback, and
    /// callback_ident for subsequent requests that entail creating lock,
    /// share reservation, and delegation state on the server.
    ///
    /// Please read: [RFC 7530](https://datatracker.ietf.org/doc/html/rfc7530#section-16.33)
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 35: SETCLIENTID - Negotiate Client ID {:?}, with request {:?}",
            self, request
        );
        let callback = ClientCallback {
            program: self.callback.cb_program,
            rnetid: self.callback.cb_location.rnetid.clone(),
            raddr: self.callback.cb_location.raddr.clone(),
            callback_ident: self.callback_ident,
        };

        let res = request
            .client_manager()
            .upsert_client(self.client.verifier, self.client.id.clone(), callback, None)
            .await;
        match res {
            Ok(client) => NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opsetclientid(SetClientId4res::Resok4(
                    SetClientId4resok {
                        clientid: client.clientid,
                        setclientid_confirm: client.setclientid_confirm,
                    },
                ))),
                status: NfsStat4::Nfs4Ok,
            },
            Err(e) => {
                error!(
                    client_id = %self.client.id,
                    verifier = ?self.client.verifier,
                    error = %e,
                    "SETCLIENTID failed"
                );
                NfsOpResponse {
                    request,
                    result: None,
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
            nfs40::{NfsResOp4, NfsStat4, SetClientId4res},
            operation::NfsOperation,
        },
        test_utils::{create_client, create_nfs40_server},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_setup_new_client() {
        let request = create_nfs40_server(None).await;

        let client1 = create_client(
            [23, 213, 67, 174, 197, 95, 35, 119],
            "Linux NFSv4.0 LAPTOP/127.0.0.1".to_string(),
        );
        let client1_dup = create_client(
            [45, 5, 67, 56, 197, 6, 35, 119],
            "Linux NFSv4.0 LAPTOP/127.0.0.1".to_string(),
        );

        // run client1
        let response = client1.execute(request).await;
        let result = response.result.unwrap();
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        match result {
            NfsResOp4::Opsetclientid(res) => match res {
                SetClientId4res::Resok4(resok) => {
                    assert_eq!(resok.clientid, 1);
                    assert_eq!(resok.setclientid_confirm.len(), 8);
                }
                _ => panic!("Expected Resok4"),
            },
            _ => panic!("Expected Opsetclientid"),
        }

        // run client1_dup (same NfsClientId4.id — should return same client_id)
        let response = client1_dup.execute(response.request).await;
        let result = response.result.unwrap();
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        match result {
            NfsResOp4::Opsetclientid(res) => match res {
                SetClientId4res::Resok4(resok) => {
                    assert_eq!(resok.clientid, 1);
                    assert_eq!(resok.setclientid_confirm.len(), 8);
                }
                _ => panic!("Expected Resok4"),
            },
            _ => panic!("Expected Opsetclientid"),
        }

        let client2 = create_client(
            [123, 213, 2, 174, 3, 95, 5, 119],
            "Linux NFSv4.0 LAPTOP-1/127.0.0.1".to_string(),
        );

        // run client2 (different id — should get new client_id)
        let response = client2.execute(response.request).await;
        let result = response.result.unwrap();
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        match result {
            NfsResOp4::Opsetclientid(res) => match res {
                SetClientId4res::Resok4(resok) => {
                    assert_eq!(resok.clientid, 2);
                    assert_eq!(resok.setclientid_confirm.len(), 8);
                }
                _ => panic!("Expected Resok4"),
            },
            _ => panic!("Expected Opsetclientid"),
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_setclientid_different_verifier_same_id() {
        // Same client id string but different verifier should still return same client_id
        // (updated verifier for an existing client)
        let request = create_nfs40_server(None).await;
        let client_a = create_client(
            [1, 2, 3, 4, 5, 6, 7, 8],
            "Linux VERF_TEST/127.0.0.1".to_string(),
        );
        let client_b = create_client(
            [10, 20, 30, 40, 50, 60, 70, 80],
            "Linux VERF_TEST/127.0.0.1".to_string(),
        );

        let res_a = client_a.execute(request).await;
        assert_eq!(res_a.status, NfsStat4::Nfs4Ok);
        let id_a = match res_a.result.unwrap() {
            NfsResOp4::Opsetclientid(SetClientId4res::Resok4(r)) => r.clientid,
            _ => panic!("Expected Resok4"),
        };

        let res_b = client_b.execute(res_a.request).await;
        assert_eq!(res_b.status, NfsStat4::Nfs4Ok);
        let id_b = match res_b.result.unwrap() {
            NfsResOp4::Opsetclientid(SetClientId4res::Resok4(r)) => r.clientid,
            _ => panic!("Expected Resok4"),
        };

        // Same client identity string -> same client_id
        assert_eq!(id_a, id_b);
    }
}
