use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs41::{ChannelAttrs, MAX_CACHED_REPLY_SIZE},
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};
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
        // (at least one slot: a session with none could not run SEQUENCE)
        let max_slots = self.csa_fore_chan_attrs.ca_maxrequests.clamp(1, 64);

        // Negotiate channel attributes — server may lower client's requests.
        // The reply cache keeps at most MAX_CACHED_REPLY_SIZE per slot (#118).
        let fore_chan = ChannelAttrs4 {
            ca_headerpadsize: 0,
            ca_maxrequestsize: self.csa_fore_chan_attrs.ca_maxrequestsize.min(1_048_576),
            ca_maxresponsesize: self.csa_fore_chan_attrs.ca_maxresponsesize.min(1_048_576),
            ca_maxresponsesize_cached: self
                .csa_fore_chan_attrs
                .ca_maxresponsesize_cached
                .min(MAX_CACHED_REPLY_SIZE),
            ca_maxoperations: self.csa_fore_chan_attrs.ca_maxoperations.min(64),
            ca_maxrequests: max_slots,
            ca_rdma_ird: vec![],
        };
        let attrs = ChannelAttrs {
            max_request_size: fore_chan.ca_maxrequestsize,
            max_response_size: fore_chan.ca_maxresponsesize,
            max_response_size_cached: fore_chan.ca_maxresponsesize_cached,
            max_ops: fore_chan.ca_maxoperations,
            max_requests: max_slots,
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

        // The client record checks csa_sequence (RFC 8881 §18.36.4, #128):
        // a retry gets the reply of the CREATE_SESSION it repeats.
        let res = sm
            .create_session_for_client(
                self.csa_clientid,
                self.csa_sequence,
                max_slots,
                attrs,
                |session| CreateSession4resok {
                    csr_sessionid: session.id,
                    csr_sequenceid: self.csa_sequence,
                    csr_flags: self.csa_flags,
                    csr_fore_chan_attrs: fore_chan,
                    csr_back_chan_attrs: back_chan,
                },
            )
            .await;
        let res = match res {
            Ok(res) => res,
            Err(status) => {
                debug!(
                    "CREATE_SESSION: clientid={} sequence={} refused: {:?}",
                    self.csa_clientid, self.csa_sequence, status
                );
                return NfsOpResponse { request, result: None, status };
            }
        };

        // Store session_id in request for subsequent operations
        request.session_id = Some(res.csr_sessionid);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpcreateSession(CreateSession4res::Resok4(res))),
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

    fn create_args(client_id: u64, seq: u32) -> CreateSession4args {
        CreateSession4args {
            csa_clientid: client_id,
            csa_sequence: seq,
            csa_flags: 0,
            csa_fore_chan_attrs: default_channel_attrs(),
            csa_back_chan_attrs: default_channel_attrs(),
            csa_cb_program: 0,
            csa_sec_parms: vec![CallbackSecParms4::AuthNone],
        }
    }

    fn session_of(result: &Option<NfsResOp4>) -> [u8; 16] {
        match result {
            Some(NfsResOp4::OpcreateSession(CreateSession4res::Resok4(res))) => res.csr_sessionid,
            other => panic!("Expected CreateSession4resok, got {other:?}"),
        }
    }

    /// #128: a CREATE_SESSION retry gets the same session back; a skipped
    /// sequence is SEQ_MISORDERED; a client id never given out is
    /// STALE_CLIENTID.
    #[tokio::test]
    #[traced_test]
    async fn test_create_session_replay_and_errors() {
        let request = create_nfs40_server(None).await;
        let exchange_args = ExchangeId4args {
            eia_clientowner: ClientOwner4 {
                co_verifier: [0u8; NFS4_VERIFIER_SIZE],
                co_ownerid: b"replay-client".to_vec(),
            },
            eia_flags: 0,
            eia_state_protect: StateProtect4a::SpNone,
            eia_client_impl_id: vec![],
        };
        let response = exchange_args.execute(request).await;
        let (client_id, seq) = match &response.result {
            Some(NfsResOp4::OpexchangeId(ExchangeId4res::Resok4(res))) => {
                (res.eir_clientid, res.eir_sequenceid)
            }
            _ => panic!("Expected ExchangeId4resok"),
        };

        let response = create_args(client_id, seq).execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let first = session_of(&response.result);

        let response = create_args(client_id, seq).execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        assert_eq!(session_of(&response.result), first, "retry made a new session");

        let response = create_args(client_id, seq + 2).execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errSeqMisordered);
        assert!(response.result.is_none());

        let response = create_args(client_id, seq + 1).execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        assert_ne!(session_of(&response.result), first);

        let response = create_args(client_id ^ 0xffff, 1).execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errStaleClientid);
    }
}
