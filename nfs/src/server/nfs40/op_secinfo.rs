//! SECINFO operation — return security flavors for a name in a directory.
//!
//! RFC 7530 Section 16.31: Returns the security mechanisms available for
//! a particular file object. The client uses this to negotiate security
//! when encountering NFS4ERR_WRONGSEC or during initial mount.

use async_trait::async_trait;
use tracing::debug;

use crate::server::operation::NfsOperation;
use crate::server::request::NfsRequest;
use crate::server::response::NfsOpResponse;
use nextnfs_proto::nfs4_proto::*;

#[async_trait]
impl NfsOperation for SecInfo4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("SECINFO name={}", self.name);

        // Current filehandle must be a directory
        let filehandle = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        if !filehandle.file.is_dir().unwrap_or(false) {
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errNotdir,
            };
        }

        // Advertise only AUTH_SYS: there is no RPCSEC_GSS context setup or
        // keytab (#99), so krb5/krb5i/krb5p must not be offered.
        // Per RFC 7530 S16.31.4, the current filehandle is consumed (set to absent)
        // after SECINFO completes.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::OpSecinfo(SecInfo4res::Resok4(vec![
                SeCinfo4::AuthSys,
            ]))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_secinfo_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = SecInfo4args { name: "test".to_string() };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_secinfo_returns_auth_flavors() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = SecInfo4args { name: "anything".to_string() };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match response.result {
            Some(NfsResOp4::OpSecinfo(SecInfo4res::Resok4(flavors))) => {
                // Only AUTH_SYS: no RPCSEC_GSS (krb5*) until contexts land (#99)
                assert_eq!(flavors, vec![SeCinfo4::AuthSys]);
            }
            _ => panic!("Expected SECINFO Resok4"),
        }
    }

    #[tokio::test]
    async fn test_secinfo_not_directory() {
        // Open/create a file, set it as current fh, then call SECINFO
        let request = create_nfs40_server_with_root_fh(None).await;
        // OPEN creates a regular file (CREATE is for non-regular objects)
        let open_args = Open4args {
            seqid: 1,
            share_access: 1,
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: vec![1, 2, 3, 4],
            },
            openhow: OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
            claim: OpenClaim4::ClaimNull("secinfo_test_file".to_string()),
        };
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // OPEN sets current fh to the new file — SECINFO should fail (not a dir)
        let secinfo_args = SecInfo4args { name: "anything".to_string() };
        let response = secinfo_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNotdir);
    }
}
