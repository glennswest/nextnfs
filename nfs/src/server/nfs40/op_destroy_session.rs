use async_trait::async_trait;
use tracing::{debug, error, warn};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{DestroySession4args, DestroySession4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for DestroySession4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 44: DESTROY_SESSION session={:02x?}",
            &self.dsa_sessionid[..4]
        );

        let sm = match request.session_manager() {
            Some(sm) => sm.clone(),
            None => {
                error!("DESTROY_SESSION: no session manager");
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::OpdestroySession(DestroySession4res {
                        dsr_status: NfsStat4::Nfs4errServerfault,
                    })),
                    status: NfsStat4::Nfs4errServerfault,
                };
            }
        };

        let destroyed = sm.destroy_session(&self.dsa_sessionid).await;

        if destroyed {
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::OpdestroySession(DestroySession4res {
                    dsr_status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            }
        } else {
            warn!("DESTROY_SESSION: session not found");
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::OpdestroySession(DestroySession4res {
                    dsr_status: NfsStat4::Nfs4errBadSession,
                })),
                status: NfsStat4::Nfs4errBadSession,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{NfsResOp4, NfsStat4},
            operation::NfsOperation,
        },
        test_utils::create_nfs40_server,
    };
    use nextnfs_proto::nfs4_proto::{
        CallbackSecParms4, ChannelAttrs4, ClientOwner4, CreateSession4args, CreateSession4res,
        DestroySession4args, ExchangeId4args, ExchangeId4res,
        StateProtect4a, NFS4_VERIFIER_SIZE,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_destroy_session_ok() {
        let request = create_nfs40_server(None).await;

        // EXCHANGE_ID + CREATE_SESSION
        let exchange_args = ExchangeId4args {
            eia_clientowner: ClientOwner4 {
                co_verifier: [0u8; NFS4_VERIFIER_SIZE],
                co_ownerid: b"ds-test".to_vec(),
            },
            eia_flags: 0,
            eia_state_protect: StateProtect4a::SpNone,
            eia_client_impl_id: vec![],
        };
        let response = exchange_args.execute(request).await;
        let client_id = if let Some(NfsResOp4::OpexchangeId(ExchangeId4res::Resok4(res))) =
            &response.result
        {
            res.eir_clientid
        } else {
            panic!("Expected ExchangeId4resok");
        };

        let cs_args = CreateSession4args {
            csa_clientid: client_id,
            csa_sequence: 1,
            csa_flags: 0,
            csa_fore_chan_attrs: ChannelAttrs4 {
                ca_headerpadsize: 0,
                ca_maxrequestsize: 1_048_576,
                ca_maxresponsesize: 1_048_576,
                ca_maxresponsesize_cached: 65536,
                ca_maxoperations: 64,
                ca_maxrequests: 16,
                ca_rdma_ird: vec![],
            },
            csa_back_chan_attrs: ChannelAttrs4 {
                ca_headerpadsize: 0,
                ca_maxrequestsize: 4096,
                ca_maxresponsesize: 4096,
                ca_maxresponsesize_cached: 0,
                ca_maxoperations: 2,
                ca_maxrequests: 1,
                ca_rdma_ird: vec![],
            },
            csa_cb_program: 0,
            csa_sec_parms: vec![CallbackSecParms4::AuthNone],
        };
        let response = cs_args.execute(response.request).await;
        let session_id = if let Some(NfsResOp4::OpcreateSession(CreateSession4res::Resok4(res))) =
            &response.result
        {
            res.csr_sessionid
        } else {
            panic!("Expected CreateSession4resok");
        };

        // DESTROY_SESSION
        let ds_args = DestroySession4args {
            dsa_sessionid: session_id,
        };
        let response = ds_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_destroy_session_not_found() {
        let request = create_nfs40_server(None).await;
        let args = DestroySession4args {
            dsa_sessionid: [0xFF; 16],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadSession);
    }
}
