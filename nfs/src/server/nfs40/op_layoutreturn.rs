use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{LayoutReturn4args, LayoutReturn4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for LayoutReturn4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 51: LAYOUTRETURN type={:?} iomode={:?} return_type={:?} reclaim={}",
            self.lora_layout_type, self.lora_iomode, self.lora_return_type, self.lora_reclaim
        );

        // No layouts are ever issued, so nothing to return.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OplayoutReturn(LayoutReturn4res::Err(
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
        LayoutIomode4, LayoutReturn4args, LayoutReturnType4, LayoutType4,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_layoutreturn_no_layout() {
        let request = create_nfs40_server(None).await;
        let args = LayoutReturn4args {
            lora_reclaim: false,
            lora_layout_type: LayoutType4::LayoutNfsv4Files,
            lora_iomode: LayoutIomode4::LayoutiomodeRead,
            lora_return_type: LayoutReturnType4::LayoutreturnAll,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNomatchingLayout);
    }
}
