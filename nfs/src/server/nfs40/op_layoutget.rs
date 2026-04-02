use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{LayoutGet4args, LayoutGet4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for LayoutGet4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 50: LAYOUTGET type={:?} iomode={:?} offset={} length={}",
            self.loga_layout_type, self.loga_iomode, self.loga_offset, self.loga_length
        );

        // Single-server implementation — no pNFS data servers.
        // Return LAYOUTUNAVAILABLE so clients use normal READ/WRITE I/O.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OplayoutGet(LayoutGet4res::Err(
                NfsStat4::Nfs4errLayoutUnavailable,
            ))),
            status: NfsStat4::Nfs4errLayoutUnavailable,
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
        LayoutGet4args, LayoutIomode4, LayoutType4, Stateid4,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_layoutget_unavailable() {
        let request = create_nfs40_server(None).await;
        let args = LayoutGet4args {
            loga_signal_layout_avail: false,
            loga_layout_type: LayoutType4::LayoutNfsv4Files,
            loga_iomode: LayoutIomode4::LayoutiomodeRead,
            loga_offset: 0,
            loga_length: 0xFFFFFFFFFFFFFFFF,
            loga_minlength: 0,
            loga_stateid: Stateid4 {
                seqid: 0,
                other: [0u8; 12],
            },
            loga_maxcount: 65536,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errLayoutUnavailable);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_layoutget_flex_files_unavailable() {
        let request = create_nfs40_server(None).await;
        let args = LayoutGet4args {
            loga_signal_layout_avail: true,
            loga_layout_type: LayoutType4::LayoutFlexFiles,
            loga_iomode: LayoutIomode4::LayoutiomodeRw,
            loga_offset: 0,
            loga_length: 4096,
            loga_minlength: 4096,
            loga_stateid: Stateid4 {
                seqid: 1,
                other: [0u8; 12],
            },
            loga_maxcount: 65536,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errLayoutUnavailable);
    }
}
