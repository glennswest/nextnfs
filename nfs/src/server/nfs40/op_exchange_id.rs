use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{
    ExchangeId4args, ExchangeId4res, ExchangeId4resok, NfsImplId4, NfsResOp4, NfsStat4,
    Nfstime4, ServerOwner4, StateProtect4r,
};

/// EXCHGID4_FLAG_USE_NON_PNFS | EXCHGID4_FLAG_SUPP_MOVED_REFER
const EXCHANGE_FLAGS: u32 = 0x00010000 | 0x00000008;

#[async_trait]
impl NfsOperation for ExchangeId4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 42: EXCHANGE_ID flags=0x{:08x} owner={:?}",
            self.eia_flags, self.eia_clientowner.co_ownerid
        );

        let sm = match request.session_manager() {
            Some(sm) => sm,
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errServerfault,
                };
            }
        };

        let client_id = sm.allocate_client_id().await;

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpexchangeId(ExchangeId4res::Resok4(
                ExchangeId4resok {
                    eir_clientid: client_id,
                    eir_sequenceid: 1,
                    eir_flags: EXCHANGE_FLAGS,
                    eir_state_protect: StateProtect4r::SpNone,
                    eir_server_owner: ServerOwner4 {
                        so_minor_id: 0,
                        so_major_id: b"nextnfs".to_vec(),
                    },
                    eir_server_scope: b"nextnfs.local".to_vec(),
                    eir_server_impl_id: vec![NfsImplId4 {
                        nii_domain: "nextnfs.dev".to_string(),
                        nii_name: "nextnfs".to_string(),
                        nii_date: Nfstime4 {
                            seconds: 0,
                            nseconds: 0,
                        },
                    }],
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
        ClientOwner4, ExchangeId4args, ExchangeId4res, StateProtect4a, NFS4_VERIFIER_SIZE,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_exchange_id_basic() {
        let request = create_nfs40_server(None).await;
        let args = ExchangeId4args {
            eia_clientowner: ClientOwner4 {
                co_verifier: [0u8; NFS4_VERIFIER_SIZE],
                co_ownerid: b"test-client".to_vec(),
            },
            eia_flags: 0,
            eia_state_protect: StateProtect4a::SpNone,
            eia_client_impl_id: vec![],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        if let Some(NfsResOp4::OpexchangeId(ExchangeId4res::Resok4(res))) = &response.result {
            assert!(res.eir_clientid > 0);
            assert_eq!(res.eir_sequenceid, 1);
            assert_eq!(res.eir_server_owner.so_major_id, b"nextnfs");
        } else {
            panic!("Expected ExchangeId4resok");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_exchange_id_unique_client_ids() {
        let request = create_nfs40_server(None).await;
        let args = ExchangeId4args {
            eia_clientowner: ClientOwner4 {
                co_verifier: [0u8; NFS4_VERIFIER_SIZE],
                co_ownerid: b"client-1".to_vec(),
            },
            eia_flags: 0,
            eia_state_protect: StateProtect4a::SpNone,
            eia_client_impl_id: vec![],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let id1 = if let Some(NfsResOp4::OpexchangeId(ExchangeId4res::Resok4(res))) =
            &response.result
        {
            res.eir_clientid
        } else {
            panic!("Expected ExchangeId4resok");
        };

        let request = response.request;
        let args2 = ExchangeId4args {
            eia_clientowner: ClientOwner4 {
                co_verifier: [0u8; NFS4_VERIFIER_SIZE],
                co_ownerid: b"client-2".to_vec(),
            },
            eia_flags: 0,
            eia_state_protect: StateProtect4a::SpNone,
            eia_client_impl_id: vec![],
        };
        let response = args2.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        let id2 = if let Some(NfsResOp4::OpexchangeId(ExchangeId4res::Resok4(res))) =
            &response.result
        {
            res.eir_clientid
        } else {
            panic!("Expected ExchangeId4resok");
        };

        assert_ne!(id1, id2);
    }
}
