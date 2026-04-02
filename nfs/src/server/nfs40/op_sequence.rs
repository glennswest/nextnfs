use async_trait::async_trait;
use tracing::{debug, error, warn};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{NfsResOp4, NfsStat4, Sequence4args, Sequence4res, Sequence4resok};

#[async_trait]
impl NfsOperation for Sequence4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 53: SEQUENCE session={:02x?} slot={} seq={}",
            &self.sa_sessionid[..4],
            self.sa_slotid,
            self.sa_sequenceid
        );

        let sm = match request.session_manager() {
            Some(sm) => sm.clone(),
            None => {
                error!("SEQUENCE: no session manager");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errServerfault,
                };
            }
        };

        let session = match sm.get_session(&self.sa_sessionid).await {
            Some(s) => s,
            None => {
                warn!("SEQUENCE: unknown session");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errBadSession,
                };
            }
        };

        // Validate slot ID
        if self.sa_slotid as usize >= session.slots.len() {
            warn!(
                "SEQUENCE: slot {} out of range (max {})",
                self.sa_slotid,
                session.slots.len() - 1
            );
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errBadSlot,
            };
        }

        // Store session context in request for subsequent operations
        request.session_id = Some(self.sa_sessionid);
        request.sequence_slotid = Some(self.sa_slotid);

        let highest_slot = (session.slots.len() - 1) as u32;

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opsequence(Sequence4res::Resok4(
                Sequence4resok {
                    sr_sessionid: self.sa_sessionid,
                    sr_sequenceid: self.sa_sequenceid,
                    sr_slotid: self.sa_slotid,
                    sr_highest_slotid: highest_slot,
                    sr_target_highest_slotid: highest_slot,
                    sr_status_flags: 0,
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
        ExchangeId4args, ExchangeId4res, Sequence4args, Sequence4res, StateProtect4a,
        NFS4_VERIFIER_SIZE,
    };
    use tracing_test::traced_test;

    async fn setup_session(
        request: crate::server::request::NfsRequest<'_>,
    ) -> (crate::server::request::NfsRequest<'_>, [u8; 16]) {
        let exchange_args = ExchangeId4args {
            eia_clientowner: ClientOwner4 {
                co_verifier: [0u8; NFS4_VERIFIER_SIZE],
                co_ownerid: b"seq-test".to_vec(),
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

        (response.request, session_id)
    }

    #[tokio::test]
    #[traced_test]
    async fn test_sequence_basic() {
        let request = create_nfs40_server(None).await;
        let (request, session_id) = setup_session(request).await;

        let args = Sequence4args {
            sa_sessionid: session_id,
            sa_sequenceid: 1,
            sa_slotid: 0,
            sa_highest_slotid: 0,
            sa_cachethis: false,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        if let Some(NfsResOp4::Opsequence(Sequence4res::Resok4(res))) = &response.result {
            assert_eq!(res.sr_sessionid, session_id);
            assert_eq!(res.sr_sequenceid, 1);
            assert_eq!(res.sr_slotid, 0);
        } else {
            panic!("Expected Sequence4resok");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_sequence_bad_session() {
        let request = create_nfs40_server(None).await;
        let args = Sequence4args {
            sa_sessionid: [0xFF; 16],
            sa_sequenceid: 1,
            sa_slotid: 0,
            sa_highest_slotid: 0,
            sa_cachethis: false,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadSession);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_sequence_bad_slot() {
        let request = create_nfs40_server(None).await;
        let (request, session_id) = setup_session(request).await;

        let args = Sequence4args {
            sa_sessionid: session_id,
            sa_sequenceid: 1,
            sa_slotid: 999, // way out of range
            sa_highest_slotid: 0,
            sa_cachethis: false,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadSlot);
    }
}
