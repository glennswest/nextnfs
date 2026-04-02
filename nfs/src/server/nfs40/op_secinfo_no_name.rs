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

/// Kerberos 5 OID (1.2.840.113554.1.2.2) encoded as ASN.1/DER integer components.
const KRB5_OID: &[u64] = &[1, 2, 840, 113554, 1, 2, 2];

#[async_trait]
impl NfsOperation for SecinfoNoName4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 52: SECINFO_NO_NAME style={:?}", self.sina_style);

        // Return supported security flavors including RPCSEC_GSS
        let flavors = vec![
            SeCinfo4::AuthSys,
            SeCinfo4::AuthNone,
            // Kerberos 5 authentication (krb5)
            SeCinfo4::FlavorInfo(RpcSecGssInfo {
                oid: KRB5_OID.to_vec(),
                qop: 0,
                service: RpcGssSvc::RpcGssSvcNone,
            }),
            // Kerberos 5 with integrity (krb5i)
            SeCinfo4::FlavorInfo(RpcSecGssInfo {
                oid: KRB5_OID.to_vec(),
                qop: 0,
                service: RpcGssSvc::RpcGssSvcIntegrity,
            }),
            // Kerberos 5 with privacy (krb5p)
            SeCinfo4::FlavorInfo(RpcSecGssInfo {
                oid: KRB5_OID.to_vec(),
                qop: 0,
                service: RpcGssSvc::RpcGssSvcPrivacy,
            }),
        ];

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
                assert_eq!(flavors.len(), 5);
                assert_eq!(flavors[0], SeCinfo4::AuthSys);
                assert_eq!(flavors[1], SeCinfo4::AuthNone);
                // krb5, krb5i, krb5p
                assert!(matches!(flavors[2], SeCinfo4::FlavorInfo(ref info) if info.service == RpcGssSvc::RpcGssSvcNone));
                assert!(matches!(flavors[3], SeCinfo4::FlavorInfo(ref info) if info.service == RpcGssSvc::RpcGssSvcIntegrity));
                assert!(matches!(flavors[4], SeCinfo4::FlavorInfo(ref info) if info.service == RpcGssSvc::RpcGssSvcPrivacy));
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
