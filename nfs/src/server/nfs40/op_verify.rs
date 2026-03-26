//! VERIFY and NVERIFY operations — attribute cache validation (RFC 7530 S16.32, S16.15).
//!
//! VERIFY: succeeds (NFS4_OK) if object attributes match provided values, fails NFS4ERR_NOT_SAME.
//! NVERIFY: succeeds (NFS4_OK) if object attributes DON'T match, fails NFS4ERR_SAME.
//!
//! These operations are critical for client cache validation. Without them, clients
//! must always fetch fresh attributes, adding round-trip latency.

use async_trait::async_trait;
use tracing::debug;

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{
    FileAttrValue, Nverify4args, Nverify4res, NfsResOp4, NfsStat4, Verify4args, Verify4res,
};

/// Compare provided attribute values with the filehandle's actual values.
/// Returns true if all provided attributes match the actual ones.
fn attrs_match(
    provided: &[FileAttrValue],
    actual_fh: &crate::server::filemanager::Filehandle,
) -> bool {
    for attr_val in provided {
        let matches = match attr_val {
            FileAttrValue::Type(provided_type) => actual_fh.attr_type == *provided_type,
            FileAttrValue::Size(provided_size) => actual_fh.attr_size == *provided_size,
            FileAttrValue::Change(provided_change) => actual_fh.attr_change == *provided_change,
            FileAttrValue::Mode(provided_mode) => actual_fh.attr_mode == *provided_mode,
            FileAttrValue::Numlinks(provided_nlinks) => {
                actual_fh.attr_nlink == *provided_nlinks
            }
            FileAttrValue::SpaceUsed(provided_space) => {
                actual_fh.attr_space_used == *provided_space
            }
            // For attributes we don't track precisely, skip the comparison (accept as matching)
            _ => true,
        };
        if !matches {
            return false;
        }
    }
    true
}

#[async_trait]
impl NfsOperation for Verify4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 37: VERIFY");

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

        let matches = attrs_match(&self.obj_attributes.attr_vals.0, &filehandle);

        if matches {
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opverify(Verify4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            }
        } else {
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opverify(Verify4res {
                    status: NfsStat4::Nfs4errNotSame,
                })),
                status: NfsStat4::Nfs4errNotSame,
            }
        }
    }
}

#[async_trait]
impl NfsOperation for Nverify4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!("Operation 17: NVERIFY");

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

        let matches = attrs_match(&self.obj_attributes.attr_vals.0, &filehandle);

        if matches {
            // Attributes match — NVERIFY fails (caller wanted them to be different)
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opnverify(Nverify4res {
                    status: NfsStat4::Nfs4errSame,
                })),
                status: NfsStat4::Nfs4errSame,
            }
        } else {
            // Attributes don't match — NVERIFY succeeds
            NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opnverify(Nverify4res {
                    status: NfsStat4::Nfs4Ok,
                })),
                status: NfsStat4::Nfs4Ok,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::nfs40::{Attrlist4, Fattr4, FileAttr, FileAttrValue};
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;

    #[tokio::test]
    async fn test_verify_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Verify4args {
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_verify_empty_attrs_matches() {
        // No attributes to verify — should always match
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Verify4args {
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_verify_mode_matches() {
        // Use mode attribute — root fh always has a valid mode from stat
        let request = create_nfs40_server_with_root_fh(None).await;
        let actual_mode = request.current_filehandle().unwrap().attr_mode;
        let args = Verify4args {
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Mode]),
                attr_vals: Attrlist4(vec![FileAttrValue::Mode(actual_mode)]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_verify_mode_mismatch() {
        let request = create_nfs40_server_with_root_fh(None).await;
        // Use an impossible mode value to guarantee mismatch
        let args = Verify4args {
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Mode]),
                attr_vals: Attrlist4(vec![FileAttrValue::Mode(0o777)]),
            },
        };
        let response = args.execute(request).await;
        // Mode 0o777 should differ from the root fh's real mode
        assert_eq!(response.status, NfsStat4::Nfs4errNotSame);
    }

    #[tokio::test]
    async fn test_nverify_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Nverify4args {
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNofilehandle);
    }

    #[tokio::test]
    async fn test_nverify_attrs_match_returns_same() {
        // When attrs match, NVERIFY should return ERR_SAME
        let request = create_nfs40_server_with_root_fh(None).await;
        let actual_mode = request.current_filehandle().unwrap().attr_mode;
        let args = Nverify4args {
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Mode]),
                attr_vals: Attrlist4(vec![FileAttrValue::Mode(actual_mode)]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errSame);
    }

    #[tokio::test]
    async fn test_nverify_attrs_differ_returns_ok() {
        // When attrs differ, NVERIFY should return OK
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Nverify4args {
            obj_attributes: Fattr4 {
                attrmask: Attrlist4(vec![FileAttr::Mode]),
                attr_vals: Attrlist4(vec![FileAttrValue::Mode(0o000)]),
            },
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
