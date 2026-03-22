use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::{op_pseudo, Lookup4res, NfsResOp4},
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Lookup4args, NfsStat4};

#[async_trait]
impl NfsOperation for Lookup4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 15: LOOKUP - Look Up Filename {:?}, with request {:?}",
            self, request
        );

        // If on pseudo-root, resolve export name
        if request.is_pseudo_root() {
            let em = request.export_manager();
            if let Some((info, _fm)) = em.get_export_by_name(&self.objname).await {
                // Switch to this export
                request.set_export(info.export_id).await;
                match request.file_manager().get_root_filehandle().await {
                    Ok(mut root_fh) => {
                        // Stamp export_id into the filehandle
                        op_pseudo::stamp_export_id(&mut root_fh.id, info.export_id);
                        request.set_filehandle(root_fh);
                        return NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Oplookup(Lookup4res {
                                status: NfsStat4::Nfs4Ok,
                            })),
                            status: NfsStat4::Nfs4Ok,
                        };
                    }
                    Err(e) => {
                        error!("Failed to get export root: {:?}", e);
                        return NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Oplookup(Lookup4res {
                                status: NfsStat4::Nfs4errServerfault,
                            })),
                            status: NfsStat4::Nfs4errServerfault,
                        };
                    }
                }
            } else {
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplookup(Lookup4res {
                        status: NfsStat4::Nfs4errNoent,
                    })),
                    status: NfsStat4::Nfs4errNoent,
                };
            }
        }

        let current_fh = request.current_filehandle();
        let filehandle = match current_fh {
            Some(filehandle) => filehandle,
            None => {
                error!("None filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        let mut path = filehandle.path.clone();
        if path == "/" {
            path.push_str(self.objname.as_str());
        } else {
            path.push('/');
            path.push_str(self.objname.as_str());
        }

        debug!("lookup {:?}", path);

        let resp = request.file_manager().get_filehandle_for_path(path).await;
        let filehandle = match resp {
            Ok(filehandle) => filehandle,
            Err(e) => {
                // a missing file during lookup is not an error
                debug!("FileManagerError {:?}", e);
                request.unset_filehandle();
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Oplookup(Lookup4res {
                        status: e.nfs_error.clone(),
                    })),
                    status: e.nfs_error,
                };
            }
        };

        // lookup sets the current filehandle to the looked up filehandle
        request.set_filehandle(filehandle);

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Oplookup(Lookup4res {
                status: NfsStat4::Nfs4Ok,
            })),
            status: NfsStat4::Nfs4Ok,
        }
    }
}
