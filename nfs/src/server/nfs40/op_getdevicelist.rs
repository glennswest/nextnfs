use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{GetDeviceList4args, GetDeviceList4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for GetDeviceList4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 48: GETDEVICELIST type={:?} maxdevices={}",
            self.gdla_layout_type, self.gdla_maxdevices
        );

        // No pNFS data servers — empty device list.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpgetdeviceList(GetDeviceList4res::Err(
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
    use nextnfs_proto::nfs4_proto::{GetDeviceList4args, LayoutType4};
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_getdevicelist_not_supported() {
        let request = create_nfs40_server(None).await;
        let args = GetDeviceList4args {
            gdla_layout_type: LayoutType4::LayoutNfsv4Files,
            gdla_maxdevices: 100,
            gdla_cookie: 0,
            gdla_cookieverf: [0u8; 8],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNotsupp);
    }
}
