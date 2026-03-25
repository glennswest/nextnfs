use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::{op_pseudo, NfsStat4},
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Fattr4, Getattr4args, Getattr4resok, NfsResOp4};

#[async_trait]
impl NfsOperation for Getattr4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 9: GETATTR - Get Attributes {:?}, with request {:?}",
            self, request
        );

        // If on pseudo-root, return synthetic attrs
        if request.is_pseudo_root() {
            let (answer_attrs, attrs) =
                op_pseudo::pseudo_root_getattr(&self.attr_request);
            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                    status: NfsStat4::Nfs4Ok,
                    obj_attributes: Some(Fattr4 {
                        attrmask: answer_attrs,
                        attr_vals: attrs,
                    }),
                })),
                status: NfsStat4::Nfs4Ok,
            };
        }

        let filehandle = request.current_filehandle();
        match filehandle {
            None => {
                error!("None filehandle");
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                        obj_attributes: None,
                        status: NfsStat4::Nfs4errStale,
                    })),
                    status: NfsStat4::Nfs4errStale,
                }
            }
            Some(filehandle) => {
                let resp = request
                    .file_manager()
                    .filehandle_attrs(&self.attr_request, filehandle);

                let (answer_attrs, attrs) = match resp {
                    Some(inner) => inner,
                    None => {
                        return NfsOpResponse {
                            request,
                            result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                                obj_attributes: None,
                                status: NfsStat4::Nfs4errServerfault,
                            })),
                            status: NfsStat4::Nfs4errServerfault,
                        };
                    }
                };

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opgetattr(Getattr4resok {
                        status: NfsStat4::Nfs4Ok,
                        obj_attributes: Some(Fattr4 {
                            attrmask: answer_attrs,
                            attr_vals: attrs,
                        }),
                    })),
                    status: NfsStat4::Nfs4Ok,
                }
            }
        }
    }
}
