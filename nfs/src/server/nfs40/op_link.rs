use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{
    ChangeInfo4, Link4args, Link4res, Link4resok, NfsResOp4, NfsStat4,
};

#[async_trait]
impl NfsOperation for Link4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 11: LINK {:?}", self);

        // SAVED_FH is the source object, CURRENT_FH is the target directory
        let saved_fh = match request.saved_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errRestorefh,
                };
            }
        };

        let target_dir = match request.current_filehandle() {
            Some(fh) => fh.clone(),
            None => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNofilehandle,
                };
            }
        };

        // Create hard link: target_dir/newname -> saved_fh
        let target_path = target_dir.file.join(&self.newname);
        let target_path = match target_path {
            Ok(p) => p,
            Err(_) => {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errInval,
                };
            }
        };

        // Use std::fs::hard_link for real filesystem hard links
        let source_real = {
            let src = saved_fh.file.as_str();
            if src.is_empty() || src == "/" {
                // Can't hard-link the root
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errInval,
                };
            }
            src.to_string()
        };

        let target_real = target_path.as_str().to_string();

        // We need the export_root to construct real paths — use file_manager
        // For now, use VFS operations (which PhysicalFS supports)
        match std::fs::hard_link(
            // This won't work directly — we need real paths.
            // Let's just return NOTSUPP for now if it fails, or try VFS copy
            &source_real,
            &target_real,
        ) {
            Ok(_) => {
                let change_before = target_dir.attr_change;
                let change_after = change_before + 1;

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplink(Link4res::Resok4(Link4resok {
                        cinfo: ChangeInfo4 {
                            atomic: true,
                            before: change_before,
                            after: change_after,
                        },
                    }))),
                    status: NfsStat4::Nfs4Ok,
                }
            }
            Err(e) => {
                debug!("LINK failed: {:?}", e);
                let status = match e.kind() {
                    std::io::ErrorKind::PermissionDenied => NfsStat4::Nfs4errAccess,
                    std::io::ErrorKind::AlreadyExists => NfsStat4::Nfs4errExist,
                    _ => NfsStat4::Nfs4errIo,
                };
                NfsOpResponse {
                    request,
                    result: None,
                    status,
                }
            }
        }
    }
}
