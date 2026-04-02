use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{LayoutCommit4args, LayoutCommit4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for LayoutCommit4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 49: LAYOUTCOMMIT offset={} length={} reclaim={}",
            self.loca_offset, self.loca_length, self.loca_reclaim
        );

        // No layouts are ever issued, so LAYOUTCOMMIT has nothing to commit.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OplayoutCommit(LayoutCommit4res::Err(
                NfsStat4::Nfs4errNomatchingLayout,
            ))),
            status: NfsStat4::Nfs4errNomatchingLayout,
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
        LayoutCommit4args, LayoutType4, Stateid4,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_layoutcommit_no_layout() {
        let request = create_nfs40_server(None).await;
        let args = LayoutCommit4args {
            loca_offset: 0,
            loca_length: 4096,
            loca_reclaim: false,
            loca_stateid: Stateid4 {
                seqid: 0,
                other: [0u8; 12],
            },
            loca_last_write_offset: false,
            loca_layout_type: LayoutType4::LayoutNfsv4Files,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNomatchingLayout);
    }
}
