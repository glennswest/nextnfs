use async_trait::async_trait;
use tracing::{debug, warn};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{
    BindConnToSession4args, BindConnToSession4res, BindConnToSession4resok,
    ChannelDirFromServer4, NfsResOp4, NfsStat4,
};

#[async_trait]
impl NfsOperation for BindConnToSession4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 41: BIND_CONN_TO_SESSION session={:02x?}",
            &self.bctsa_sessid[..4]
        );

        // Verify session exists
        let sm = match request.session_manager() {
            Some(sm) => sm.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errServerfault,
                };
            }
        };

        let session = match sm.get_session(&self.bctsa_sessid).await {
            Some(s) => s,
            None => {
                warn!("BIND_CONN_TO_SESSION: session not found");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errBadSession,
                };
            }
        };

        // Accept the binding — fore channel only (no callback channel yet)
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpbindConnToSession(
                BindConnToSession4res::Resok4(BindConnToSession4resok {
                    bctsr_sessid: session.id,
                    bctsr_dir: ChannelDirFromServer4::Fore,
                    bctsr_use_conn_in_rdma_mode: false,
                }),
            )),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{nfs40::NfsStat4, operation::NfsOperation},
        test_utils::create_nfs40_server,
    };
    use nextnfs_proto::nfs4_proto::{
        BindConnToSession4args, ChannelDirFromClient4,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_bind_conn_bad_session() {
        let request = create_nfs40_server(None).await;
        let args = BindConnToSession4args {
            bctsa_sessid: [0xFF; 16],
            bctsa_dir: ChannelDirFromClient4::ForeOrBoth,
            bctsa_use_conn_in_rdma_mode: false,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadSession);
    }
}
