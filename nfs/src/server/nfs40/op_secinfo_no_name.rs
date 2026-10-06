//! SECINFO_NO_NAME operation — return security flavors without naming a file.
//!
//! RFC 5661 §18.45: Like SECINFO but operates on the current filehandle
//! (SECINFO_STYLE4_CURRENT_FH) or its parent (SECINFO_STYLE4_PARENT).
//! Used during initial mount security negotiation at the pseudo-root.

use async_trait::async_trait;
use tracing::debug;

use crate::server::operation::NfsOperation;
use crate::server::request::NfsRequest;
use crate::server::response::NfsOpResponse;
use nextnfs_proto::nfs4_proto::*;

#[async_trait]
impl NfsOperation for SecinfoNoName4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 52: SECINFO_NO_NAME style={:?}", self.sina_style);

        // Advertise only the flavors the server can serve. There is no
        // RPCSEC_GSS context setup or keytab (#99), so krb5/krb5i/krb5p must
        // not be offered: a client negotiating from this list would pick a
        // flavor the server rejects (and AUTH_NONE would be answered as uid 0).
        let flavors = vec![SeCinfo4::AuthSys];

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpsecinfoNoName(SecinfoNoName4res::Resok4(
                flavors,
            ))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_secinfo_no_name_current_fh() {
        let request = create_nfs40_server(None).await;
        let args = SecinfoNoName4args {
            sina_style: SecinfoStyle4::SecinfoStyle4CurrentFh,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::OpsecinfoNoName(SecinfoNoName4res::Resok4(flavors))) => {
                // Only AUTH_SYS: no RPCSEC_GSS (krb5*) until contexts land (#99)
                assert_eq!(flavors, vec![SeCinfo4::AuthSys]);
            }
            _ => panic!("Expected SECINFO_NO_NAME Resok4"),
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_secinfo_no_name_parent() {
        let request = create_nfs40_server(None).await;
        let args = SecinfoNoName4args {
            sina_style: SecinfoStyle4::SecinfoStyle4Parent,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
