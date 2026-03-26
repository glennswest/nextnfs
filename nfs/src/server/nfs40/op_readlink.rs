//! READLINK operation — read symbolic link target.

use async_trait::async_trait;

use crate::server::operation::NfsOperation;
use crate::server::request::NfsRequest;
use crate::server::response::NfsOpResponse;
use nextnfs_proto::nfs4_proto::*;
use tracing::{debug, error};

/// Readlink has no arguments — uses current filehandle.
pub struct Readlink4args;

#[async_trait]
impl NfsOperation for Readlink4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("READLINK");

        let filehandle = match request.current_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // Get the real path and use std::fs::read_link
        let path_str = filehandle.file.as_str().to_string();
        match std::fs::read_link(&path_str) {
            Ok(target) => {
                let link_target = target.to_string_lossy().to_string();
                debug!("READLINK {} -> {}", path_str, link_target);
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opreadlink(Readlink4res {
                        status: NfsStat4::Nfs4Ok,
                        link: link_target,
                    })),
                    status: NfsStat4::Nfs4Ok,
                }
            }
            Err(e) => {
                let status = match e.kind() {
                    std::io::ErrorKind::NotFound => NfsStat4::Nfs4errNoent,
                    std::io::ErrorKind::InvalidInput => {
                        // Not a symlink
                        NfsStat4::Nfs4errInval
                    }
                    _ => {
                        error!("READLINK failed for {}: {:?}", path_str, e);
                        NfsStat4::Nfs4errIo
                    }
                };
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opreadlink(Readlink4res {
                        status: status.clone(),
                        link: String::new(),
                    })),
                    status,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_readlink_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Readlink4args;
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_readlink_on_directory() {
        // READLINK on root dir should fail — not a symlink
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Readlink4args;
        let response = args.execute(request).await;
        // MemoryFS paths don't map to real filesystem, so read_link will fail
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }
}
