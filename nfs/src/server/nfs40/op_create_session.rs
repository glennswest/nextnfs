use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{
    ChannelAttrs4, CreateSession4args, CreateSession4res, CreateSession4resok, NfsResOp4,
    NfsStat4,
};

#[async_trait]
impl NfsOperation for CreateSession4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 43: CREATE_SESSION clientid={} sequence={}",
            self.csa_clientid, self.csa_sequence
        );

        let sm = match request.session_manager() {
            Some(sm) => sm.clone(),
            None => {
                error!("CREATE_SESSION: no session manager");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errServerfault,
                };
            }
        };

        // Use the client-requested max_requests as slot count (capped at 64)
        let max_slots = self.csa_fore_chan_attrs.ca_maxrequests.min(64);
        let session = sm.create_session(self.csa_clientid, max_slots).await;

        // Negotiate channel attributes — server may lower client's requests
        let fore_chan = ChannelAttrs4 {
            ca_headerpadsize: 0,
            ca_maxrequestsize: self.csa_fore_chan_attrs.ca_maxrequestsize.min(1_048_576),
            ca_maxresponsesize: self.csa_fore_chan_attrs.ca_maxresponsesize.min(1_048_576),
            ca_maxresponsesize_cached: self
                .csa_fore_chan_attrs
                .ca_maxresponsesize_cached
                .min(1_048_576),
            ca_maxoperations: self.csa_fore_chan_attrs.ca_maxoperations.min(64),
            ca_maxrequests: max_slots,
            ca_rdma_ird: vec![],
        };

        // Back channel — minimal support (no callbacks yet)
        let back_chan = ChannelAttrs4 {
            ca_headerpadsize: 0,
            ca_maxrequestsize: 4096,
            ca_maxresponsesize: 4096,
            ca_maxresponsesize_cached: 0,
            ca_maxoperations: 2,
            ca_maxrequests: 1,
            ca_rdma_ird: vec![],
        };

        // Store session_id in request for subsequent operations
        request.session_id = Some(session.id);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpcreateSession(CreateSession4res::Resok4(
                CreateSession4resok {
                    csr_sessionid: session.id,
                    csr_sequenceid: self.csa_sequence,
                    csr_flags: self.csa_flags,
                    csr_fore_chan_attrs: fore_chan,
                    csr_back_chan_attrs: back_chan,
                },
            ))),
            status: NfsStat4::Nfs4Ok,
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
        ExchangeId4args, ExchangeId4res, StateProtect4a, NFS4_VERIFIER_SIZE,
    };
    use tracing_test::traced_test;

    fn default_channel_attrs() -> ChannelAttrs4 {
        ChannelAttrs4 {
            ca_headerpadsize: 0,
            ca_maxrequestsize: 1_048_576,
            ca_maxresponsesize: 1_048_576,
            ca_maxresponsesize_cached: 65536,
            ca_maxoperations: 64,
            ca_maxrequests: 32,
            ca_rdma_ird: vec![],
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_create_session_basic() {
        let request = create_nfs40_server(None).await;

        // First get a client ID via EXCHANGE_ID
        let exchange_args = ExchangeId4args {
            eia_clientowner: ClientOwner4 {
                co_verifier: [0u8; NFS4_VERIFIER_SIZE],
                co_ownerid: b"test-client".to_vec(),
            },
            eia_flags: 0,
            eia_state_protect: StateProtect4a::SpNone,
            eia_client_impl_id: vec![],
        };
        let response = exchange_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let client_id = if let Some(NfsResOp4::OpexchangeId(ExchangeId4res::Resok4(res))) =
            &response.result
        {
            res.eir_clientid
        } else {
            panic!("Expected ExchangeId4resok");
        };

        // CREATE_SESSION
        let cs_args = CreateSession4args {
            csa_clientid: client_id,
            csa_sequence: 1,
            csa_flags: 0,
            csa_fore_chan_attrs: default_channel_attrs(),
            csa_back_chan_attrs: default_channel_attrs(),
            csa_cb_program: 0,
            csa_sec_parms: vec![CallbackSecParms4::AuthNone],
        };
        let response = cs_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        if let Some(NfsResOp4::OpcreateSession(CreateSession4res::Resok4(res))) = &response.result
        {
            assert_eq!(res.csr_sequenceid, 1);
            assert!(res.csr_sessionid != [0u8; 16]);
            assert_eq!(res.csr_fore_chan_attrs.ca_maxrequests, 32);
        } else {
            panic!("Expected CreateSession4resok");
        }
    }
}
