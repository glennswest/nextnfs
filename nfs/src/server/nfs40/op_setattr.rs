use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    nfs40::NfsStat4, operation::NfsOperation, request::NfsRequest, response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{Attrlist4, FileAttr, NfsResOp4, SetAttr4args, SetAttr4res};

#[async_trait]
impl NfsOperation for SetAttr4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 34: SETATTR - Set Attributes {:?}, with request {:?}",
            self, request
        );
        let filehandle = request.current_filehandle();
        match filehandle {
            None => {
                error!("None filehandle");
                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opsetattr(SetAttr4res {
                        status: NfsStat4::Nfs4errStale,
                        attrsset: Attrlist4::<FileAttr>::new(None),
                    })),
                    status: NfsStat4::Nfs4errStale,
                }
            }
            Some(filehandle) => {
                let attrsset = if !self.obj_attributes.attrmask.is_empty() {
                    let attrsset = request
                        .file_manager()
                        .set_attr(filehandle, &self.obj_attributes.attr_vals);

                    request
                        .file_manager()
                        .touch_file(filehandle.id)
                        .await;

                    match request.set_filehandle_id(filehandle.id).await {
                        Ok(fh) => {
                            request.cache_filehandle(fh);
                        }
                        Err(e) => {
                            return NfsOpResponse {
                                request,
                                result: None,
                                status: e,
                            };
                        }
                    }

                    attrsset
                } else {
                    Attrlist4::<FileAttr>::new(None)
                };

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opsetattr(SetAttr4res {
                        status: NfsStat4::Nfs4Ok,
                        attrsset,
                    })),
                    status: NfsStat4::Nfs4Ok,
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
    use nextnfs_proto::nfs4_proto::{Fattr4, Stateid4};

    #[tokio::test]
    async fn test_setattr_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = SetAttr4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errStale);
    }

    #[tokio::test]
    async fn test_setattr_empty_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = SetAttr4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
