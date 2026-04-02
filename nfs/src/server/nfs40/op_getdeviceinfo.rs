use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{GetDeviceInfo4args, GetDeviceInfo4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for GetDeviceInfo4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 47: GETDEVICEINFO device={:02x?} type={:?}",
            &self.gdia_device_id[..4],
            self.gdia_layout_type
        );

        // No pNFS data servers — no devices to report.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpgetdeviceInfo(GetDeviceInfo4res::Err(
                NfsStat4::Nfs4errNotsupp,
            ))),
            status: NfsStat4::Nfs4errNotsupp,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{nfs40::NfsStat4, operation::NfsOperation},
        test_utils::create_nfs40_server,
    };
    use nextnfs_proto::nfs4_proto::{GetDeviceInfo4args, LayoutType4};
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_getdeviceinfo_not_supported() {
        let request = create_nfs40_server(None).await;
        let args = GetDeviceInfo4args {
            gdia_device_id: [0u8; 16],
            gdia_layout_type: LayoutType4::LayoutNfsv4Files,
            gdia_maxcount: 65536,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNotsupp);
    }
}
