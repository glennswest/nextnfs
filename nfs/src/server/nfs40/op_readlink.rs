//! READLINK operation — read symbolic link target.
//!
//! Returns NFS4ERR_NOTSUPP since the VFS backend doesn't support symlinks yet.

use async_trait::async_trait;

use crate::server::operation::NfsOperation;
use crate::server::request::NfsRequest;
use crate::server::response::NfsOpResponse;
use nextnfs_proto::nfs4_proto::*;
use tracing::debug;

/// Readlink has no arguments — uses current filehandle.
pub struct Readlink4args;

#[async_trait]
impl NfsOperation for Readlink4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("READLINK");

        // The vfs crate's MemoryFS doesn't support symlinks.
        // Return NOTSUPP for now — will be implemented when backed by StormFS VFS.
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opreadlink(Readlink4res {
                status: NfsStat4::Nfs4errNotsupp,
                link: String::new(),
            })),
            status: NfsStat4::Nfs4errNotsupp,
        }
    }
}
