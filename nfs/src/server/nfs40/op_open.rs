use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    filemanager::Filehandle,
    nfs40::{
        ChangeInfo4, Open4res, Open4resok, OpenDelegation4, OpenReadDelegation4,
        OPEN4_RESULT_CONFIRM,
    },
    operation::NfsOperation,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    Attrlist4, CreateHow4, FileAttr, Nfsace4, NfsResOp4, NfsStat4, Open4args, OpenClaim4,
    OpenFlag4, Stateid4, ACE4_ACCESS_ALLOWED_ACE_TYPE,
};

async fn open_for_reading<'a>(
    args: &Open4args,
    file: &String,
    mut request: NfsRequest<'a>,
) -> NfsOpResponse<'a> {
    let filehandle = match request.current_filehandle() {
        Some(fh) => fh,
        None => {
            error!("OPEN read: no current filehandle");
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errNofilehandle,
            };
        }
    };
    let path = &filehandle.path;

    let fh_path = {
        if path == "/" {
            format!("{}{}", path, file)
        } else {
            format!("{}/{}", path, file)
        }
    };

    debug!("open_for_reading {:?}", fh_path);
    let filehandle = match request
        .file_manager()
        .get_filehandle_for_path(fh_path)
        .await
    {
        Ok(filehandle) => filehandle,
        Err(e) => {
            error!("Err {:?}", e);
            return NfsOpResponse {
                request,
                result: None,
                status: e.nfs_error,
            };
        }
    };

    let fh_id = filehandle.id;
    request.set_filehandle(filehandle);

    // Attempt to grant a read delegation
    let delegation = match request
        .file_manager()
        .grant_delegation(fh_id, args.owner.clientid, false)
        .await
    {
        Some(stateid) => OpenDelegation4::Read(OpenReadDelegation4 {
            stateid,
            recall: false,
            permissions: Nfsace4 {
                acetype: ACE4_ACCESS_ALLOWED_ACE_TYPE,
                flag: 0,
                access_mask: 0x00000001, // ACE4_READ_DATA
                who: "EVERYONE@".to_string(),
            },
        }),
        None => OpenDelegation4::None,
    };

    NfsOpResponse {
        request,
        result: Some(NfsResOp4::Opopen(Open4res::Resok4(Open4resok {
            stateid: Stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            cinfo: ChangeInfo4 {
                atomic: false,
                before: 0,
                after: 0,
            },
            rflags: OPEN4_RESULT_CONFIRM,
            attrset: Attrlist4::<FileAttr>::new(None),
            delegation,
        }))),
        status: NfsStat4::Nfs4Ok,
    }
}

async fn open_for_writing<'a>(
    args: &Open4args,
    filehandle: &Filehandle,
    file: &String,
    how: &CreateHow4,
    mut request: NfsRequest<'a>,
) -> NfsOpResponse<'a> {
    let path = &filehandle.path;

    let fh_path = {
        if path == "/" {
            format!("{}{}", path, file)
        } else {
            format!("{}/{}", path, file)
        }
    };

    debug!("open_for_writing {:?}", fh_path);

    // Quota enforcement: reject file creation if hard limit already exceeded
    if let Some(qm) = request.quota_manager() {
        if !qm.check_write(0) {
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errDquot,
            };
        }
    }

    let newfile = match filehandle.file.join(file) {
        Ok(p) => p,
        Err(e) => {
            error!("OPEN write: invalid path join: {:?}", e);
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errInval,
            };
        }
    };

    let filehandle = match how {
        CreateHow4::UNCHECKED4(_fattr) => {
            match request
                .file_manager()
                .create_file(
                    newfile.clone(),
                    args.owner.clientid,
                    args.owner.owner.clone(),
                    args.share_access,
                    args.share_deny,
                    None,
                )
                .await
            {
                Ok(filehandle) => filehandle,
                Err(e) => {
                    error!("Err {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errServerfault,
                    };
                }
            }
        }
        CreateHow4::EXCLUSIVE4(verifier) => {
            match request
                .file_manager()
                .create_file(
                    newfile,
                    args.owner.clientid,
                    args.owner.owner.clone(),
                    args.share_access,
                    args.share_deny,
                    Some(*verifier),
                )
                .await
            {
                Ok(filehandle) => filehandle,
                Err(e) => {
                    error!("Err {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errServerfault,
                    };
                }
            }
        }
        _ => {
            error!("Unsupported CreateHow4 {:?}", how);
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errNotsupp,
            };
        }
    };

    request.set_filehandle(filehandle.clone());
    // we expect this filehandle to have one lock (for the shared reservation)
    let lock = &filehandle.locks[0];

    NfsOpResponse {
        request,
        result: Some(NfsResOp4::Opopen(Open4res::Resok4(Open4resok {
            stateid: Stateid4 {
                seqid: lock.seqid,
                other: lock.stateid,
            },
            cinfo: ChangeInfo4 {
                atomic: false,
                before: 0,
                after: 0,
            },
            // OPEN4_RESULT_CONFIRM indicates that the client MUST execute an
            // OPEN_CONFIRM operation before using the open file.
            rflags: OPEN4_RESULT_CONFIRM,
            attrset: Attrlist4::<FileAttr>::new(None),
            delegation: OpenDelegation4::None,
        }))),
        status: NfsStat4::Nfs4Ok,
    }
}

#[async_trait]
impl NfsOperation for Open4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        // Description: https://datatracker.ietf.org/doc/html/rfc7530#section-16.16.5
        debug!(
            "Operation 18: OPEN - Open a Regular File {:?}, with request {:?}",
            self, request
        );
        // CLAIM_PREVIOUS — Reclaim open state from a previous server instance.
        // Current filehandle is the FILE (not directory), so handle before dir check.
        if let OpenClaim4::ClaimPrevious(_delegation_type) = &self.claim {
            let fh = match request.current_filehandle() {
                Some(fh) => fh.clone(),
                None => {
                    error!("CLAIM_PREVIOUS: no current filehandle");
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errNofilehandle,
                    };
                }
            };
            // Register an open lock for this reclaimed file
            match request.file_manager().create_open_state(
                fh.file.clone(),
                self.owner.clientid,
                self.owner.owner.clone(),
                self.share_access,
                self.share_deny,
            ).await {
                Ok(lock) => {
                    return NfsOpResponse {
                        request,
                        result: Some(NfsResOp4::Opopen(Open4res::Resok4(Open4resok {
                            stateid: Stateid4 {
                                seqid: lock.seqid,
                                other: lock.stateid,
                            },
                            cinfo: ChangeInfo4 {
                                atomic: false,
                                before: 0,
                                after: 0,
                            },
                            rflags: 0, // No CONFIRM needed for reclaim
                            attrset: Attrlist4::<FileAttr>::new(None),
                            delegation: OpenDelegation4::None,
                        }))),
                        status: NfsStat4::Nfs4Ok,
                    };
                }
                Err(e) => {
                    error!("CLAIM_PREVIOUS: failed to create open state: {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: e.nfs_error,
                    };
                }
            }
        }

        // open sets the current filehandle to the looked up filehandle
        let current_filehandle = request.current_filehandle();
        let filehandle = match current_filehandle {
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

        // If the current filehandle is not a directory, the error
        // NFS4ERR_NOTDIR will be returned.
        if !filehandle.file.is_dir().unwrap_or(false) {
            error!("Not a directory");
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errNotdir,
            };
        }

        let file = match &self.claim {
            OpenClaim4::ClaimNull(file) => file,
            // Delegation claim types — not yet supported (requires callback channel)
            _ => {
                error!("Unsupported OpenClaim4 {:?}", self.claim);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNotsupp,
                };
            }
        };

        // If the component is of zero length, NFS4ERR_INVAL will be returned.
        // The component is also subject to the normal UTF-8, character support,
        // and name checks.  See Section 12.7 for further discussion.
        if file.is_empty() {
            error!("Empty file name");
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errInval,
            };
        }

        match &self.openhow {
            OpenFlag4::Open4Nocreate => {
                // Open a file for reading
                open_for_reading(self, file, request).await
            }
            OpenFlag4::How(how) => {
                // Open a file for writing
                open_for_writing(self, &filehandle.clone(), file, how, request).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::operation::NfsOperation;
    use crate::test_utils::*;
    use nextnfs_proto::nfs4_proto::{Fattr4, OpenOwner4};

    fn make_open_args(file: &str, how: OpenFlag4) -> Open4args {
        Open4args {
            seqid: 1,
            share_access: 1, // OPEN4_SHARE_ACCESS_READ
            share_deny: 0,   // OPEN4_SHARE_DENY_NONE
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"test_owner".to_vec(),
            },
            openhow: how,
            claim: OpenClaim4::ClaimNull(file.to_string()),
        }
    }

    #[tokio::test]
    async fn test_open_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = make_open_args("testfile", OpenFlag4::Open4Nocreate);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    async fn test_open_empty_filename() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_open_args("", OpenFlag4::Open4Nocreate);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errInval);
    }

    #[tokio::test]
    async fn test_open_create_file() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_open_args(
            "newfile.txt",
            OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
        );
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => {
                assert_eq!(resok.rflags, OPEN4_RESULT_CONFIRM);
            }
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_open_read_nonexistent() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_open_args("nonexistent.txt", OpenFlag4::Open4Nocreate);
        let response = args.execute(request).await;
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_open_exclusive_create() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Open4args {
            seqid: 1,
            share_access: 2, // WRITE
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"excl_owner".to_vec(),
            },
            openhow: OpenFlag4::How(CreateHow4::EXCLUSIVE4([1, 2, 3, 4, 5, 6, 7, 8])),
            claim: OpenClaim4::ClaimNull("exclusive.dat".to_string()),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => {
                assert_eq!(resok.rflags, OPEN4_RESULT_CONFIRM);
            }
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_open_on_non_directory() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Create a file and set it as current fh
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("afile").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("afile".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        let args = make_open_args("nested", OpenFlag4::Open4Nocreate);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNotdir);
    }

    #[tokio::test]
    async fn test_open_unsupported_claim_type() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Open4args {
            seqid: 1,
            share_access: 1,
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"test".to_vec(),
            },
            openhow: OpenFlag4::Open4Nocreate,
            // CLAIM_DELEGATE_PREV is unsupported (requires callback channel)
            claim: OpenClaim4::ClaimDelegatePrev("delegfile".to_string()),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNotsupp);
    }

    #[tokio::test]
    async fn test_open_claim_previous_reclaim() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Create a file first, then reclaim it with CLAIM_PREVIOUS
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("reclaim.txt").unwrap().create_file().unwrap();
        let fh = request.file_manager()
            .get_filehandle_for_path("reclaim.txt".to_string())
            .await.unwrap();
        request.set_filehandle(fh);

        let args = Open4args {
            seqid: 1,
            share_access: 1,
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"reclaim_owner".to_vec(),
            },
            openhow: OpenFlag4::Open4Nocreate,
            claim: OpenClaim4::ClaimPrevious(
                nextnfs_proto::nfs4_proto::OpenDelegationType4::OpenDelegateNone,
            ),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        // CLAIM_PREVIOUS should not require OPEN_CONFIRM (rflags = 0)
        match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => {
                assert_eq!(resok.rflags, 0);
            }
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_open_claim_previous_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Open4args {
            seqid: 1,
            share_access: 1,
            share_deny: 0,
            owner: OpenOwner4 {
                clientid: 1,
                owner: b"test".to_vec(),
            },
            openhow: OpenFlag4::Open4Nocreate,
            claim: OpenClaim4::ClaimPrevious(
                nextnfs_proto::nfs4_proto::OpenDelegationType4::OpenDelegateNone,
            ),
        };
        let response = args.execute(request).await;
        // Without a current filehandle, CLAIM_PREVIOUS fails before reaching reclaim
        assert_ne!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_open_create_dot_hidden_file() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_open_args(
            ".hidden_file",
            OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
        );
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_open_create_file_with_spaces() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = make_open_args(
            "file with spaces.txt",
            OpenFlag4::How(CreateHow4::UNCHECKED4(Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            })),
        );
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_open_read_dot_hidden_file() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join(".dotfile").unwrap().create_file().unwrap();
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let args = make_open_args(".dotfile", OpenFlag4::Open4Nocreate);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }

    #[tokio::test]
    async fn test_open_read_existing_file() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Create file first
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("readable.txt").unwrap().create_file().unwrap();
        // Reset fh to root for OPEN
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let args = make_open_args("readable.txt", OpenFlag4::Open4Nocreate);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
    }
}
