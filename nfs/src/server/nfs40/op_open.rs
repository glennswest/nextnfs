use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    createattrs::{self, io_status, CreateAttrs, DEFAULT_FILE_MODE},
    filemanager::Filehandle,
    nfs40::{
        ChangeInfo4, Open4res, Open4resok, OpenDelegation4, OPEN4_RESULT_CONFIRM,
    },
    operation::NfsOperation,
    perm,
    request::NfsRequest,
    response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    Attrlist4, CreateHow4, FileAttr, NfsResOp4, NfsStat4, Open4args, OpenClaim4, OpenFlag4,
    Stateid4,
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

    let fh_path = match perm::component_path(path, file) {
        Ok(p) => p,
        Err(status) => {
            return NfsOpResponse {
                request,
                result: None,
                status,
            }
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

    let filehandle_file = filehandle.file.clone();

    // Create open state lock in lockdb (same pattern as CLAIM_PREVIOUS)
    let lock = match request
        .file_manager()
        .create_open_state(
            filehandle_file,
            args.owner.clientid,
            args.owner.owner.clone(),
            args.share_access,
            args.share_deny,
        )
        .await
    {
        Ok(lock) => lock,
        Err(e) => {
            error!("OPEN read: failed to create open state: {:?}", e);
            return NfsOpResponse {
                request,
                result: None,
                status: e.nfs_error,
            };
        }
    };

    // Attach lock to filehandle so OPEN_CONFIRM can find it
    let mut filehandle = filehandle;
    filehandle.locks.push(lock.clone());
    // Cache the filehandle WITH locks so PUTFH in the OPEN_CONFIRM compound
    // gets fresh data. Without this, a stale cache entry (from a previous
    // stat after CLOSE, or inode reuse) could return the fh without locks,
    // causing OPEN_CONFIRM to fail with NFS4ERR_BAD_STATEID.
    request.cache_filehandle(filehandle.clone());
    request.set_filehandle(filehandle);

    // Don't grant read delegations — the Linux NFS client returns the
    // delegation during unlink(), which triggers a premature CLOSE of the
    // NFS open state. This breaks silly-rename (delete-while-open) because
    // the fd becomes stale before the application can read from it.
    // Most NFS servers (including Linux knfsd) don't grant read delegations
    // by default either.
    let delegation = OpenDelegation4::None;

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

    let fh_path = match perm::component_path(path, file) {
        Ok(p) => p,
        Err(status) => {
            return NfsOpResponse {
                request,
                result: None,
                status,
            }
        }
    };

    debug!("open_for_writing {:?}", fh_path);

    macro_rules! fail {
        ($status:expr) => {
            return NfsOpResponse {
                request,
                result: None,
                status: $status,
            }
        };
    }

    // Quota enforcement: reject file creation if hard limit already exceeded
    if let Some(qm) = request.quota_manager() {
        if !qm.check_write(0) {
            fail!(NfsStat4::Nfs4errDquot);
        }
    }

    // createattrs are validated before anything is created (#127).
    let caller = request.caller();
    let (attrs, guarded, verifier) = match how {
        CreateHow4::UNCHECKED4(fattr) => (CreateAttrs::parse(fattr), false, None),
        CreateHow4::GUARDED4(fattr) => (CreateAttrs::parse(fattr), true, None),
        CreateHow4::EXCLUSIVE4(verifier) => (Ok(CreateAttrs::default()), false, Some(*verifier)),
    };
    let attrs = match attrs.and_then(|a| a.check(&caller).map(|_| a)) {
        Ok(a) => a,
        Err(status) => fail!(status),
    };

    let newfile = match filehandle.file.join(file) {
        Ok(p) => p,
        Err(e) => {
            error!("OPEN write: invalid path join: {:?}", e);
            fail!(NfsStat4::Nfs4errInval);
        }
    };

    // Capture parent dir change attr before the create so we can return real
    // ChangeInfo4 before/after — kernel clients need this to keep their
    // dentry/readdir caches in sync with directory mutations.
    let parent_vfs_str = filehandle.path.clone();
    let fm = request.file_manager();
    let before_change = fm.dir_change_attr(&parent_vfs_str);

    // Create the file with the requested permission bits from the start
    // (O_EXCL: the file is ours only if this call made it), resolved
    // beneath the export root (#123). The file manager then opens what is
    // there; it never truncates (#127: UNCHECKED4 on an existing file
    // keeps its data unless size 0 is asked).
    let created = if fm.vfs_only() {
        !newfile.exists().unwrap_or(false)
    } else {
        let flags = libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL;
        match fm.beneath().open_file(&fh_path, flags, attrs.initial_mode(DEFAULT_FILE_MODE)) {
            Ok(_) => true,
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => false,
            Err(e) => {
                debug!("OPEN create {:?}: {}", fh_path, e);
                fail!(io_status(&e));
            }
        }
    };
    if !created {
        if guarded {
            fail!(NfsStat4::Nfs4errExist);
        }
        let is_dir = if fm.vfs_only() {
            newfile.is_dir().unwrap_or(false)
        } else {
            fm.beneath().is_dir(&fh_path)
        };
        if is_dir {
            fail!(NfsStat4::Nfs4errIsdir);
        }
    }

    let filehandle = match fm
        .create_file(
            newfile.clone(),
            args.owner.clientid,
            args.owner.owner.clone(),
            args.share_access,
            args.share_deny,
            verifier,
        )
        .await
    {
        Ok(filehandle) => filehandle,
        Err(e) => {
            error!("Err {:?}", e);
            fail!(NfsStat4::Nfs4errServerfault);
        }
    };

    // Cache the filehandle WITH locks so PUTFH in the OPEN_CONFIRM compound
    // gets fresh data (prevents stale cache from stat-after-CLOSE or inode reuse).
    request.cache_filehandle(filehandle.clone());
    request.set_filehandle(filehandle.clone());
    // we expect this filehandle to have one lock (for the shared reservation)
    let lock = filehandle.locks[0].clone();

    let attrset = if created {
        // Owner, exact mode, size (#127); the filehandle keeps its locks.
        createattrs::finish_new_object(&mut request, &caller, &attrs).await
    } else if attrs.size == Some(0) && !fm.vfs_only() {
        // UNCHECKED4 on an existing file applies only a size of zero
        // (RFC 7530 §16.16.5); the dispatcher checked write access.
        match fm.beneath().set_len(&fh_path, 0) {
            Ok(()) => {
                createattrs::refresh_current(&mut request).await;
                Attrlist4::<FileAttr>::new(Some(vec![FileAttr::Size]))
            }
            Err(e) => {
                error!("OPEN truncate {:?}: {}", fh_path, e);
                fail!(io_status(&e));
            }
        }
    } else {
        Attrlist4::<FileAttr>::new(None)
    };

    let after_change = fm.dir_change_attr(&parent_vfs_str);

    NfsOpResponse {
        request,
        result: Some(NfsResOp4::Opopen(Open4res::Resok4(Open4resok {
            stateid: Stateid4 {
                seqid: lock.seqid,
                other: lock.stateid,
            },
            cinfo: ChangeInfo4 {
                atomic: false,
                before: before_change,
                after: after_change,
            },
            // OPEN4_RESULT_CONFIRM indicates that the client MUST execute an
            // OPEN_CONFIRM operation before using the open file.
            rflags: OPEN4_RESULT_CONFIRM,
            attrset,
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
            // nextnfs never grants a delegation (see `open_for_reading`), so
            // no delegation stateid a client presents can be one we issued:
            // CLAIM_DELEGATE_CUR is answered NFS4ERR_BAD_STATEID
            // (RFC 7530 §16.16.5, §9.1.4.3).
            OpenClaim4::ClaimDelegateCur(_) => {
                debug!("CLAIM_DELEGATE_CUR: no delegation was granted, bad stateid");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errBadStateid,
                };
            }
            // CLAIM_DELEGATE_PREV reclaims a delegation across a client
            // reboot; a server that does not support delegation reclaim
            // answers NFS4ERR_NOTSUPP (RFC 7530 §16.16.5). With no
            // delegations ever granted there is nothing to reclaim.
            OpenClaim4::ClaimDelegatePrev(_) => {
                debug!("CLAIM_DELEGATE_PREV: no delegations to reclaim");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errNotsupp,
                };
            }
            // Handled above, before the directory check.
            OpenClaim4::ClaimPrevious(_) => unreachable!("CLAIM_PREVIOUS handled earlier"),
        };

        // If the component is of zero length, NFS4ERR_INVAL will be returned.
        // The component is also subject to the normal UTF-8, character support,
        // and name checks.  See Section 12.7 for further discussion.
        // Also a single component, not `.`/`..` (#129): see `perm::check_name`.
        if let Err(status) = perm::check_name(file) {
            error!("OPEN: bad name {:?}", file);
            return NfsOpResponse {
                request,
                result: None,
                status,
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
    use nextnfs_proto::nfs4_proto::{Fattr4, OpenClaimDelegateCur4, OpenOwner4};

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
    async fn test_open_claim_delegate_prev_notsupp() {
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
            // No delegations are ever granted, so none can be reclaimed.
            claim: OpenClaim4::ClaimDelegatePrev("delegfile".to_string()),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNotsupp);
    }

    #[tokio::test]
    async fn test_open_claim_delegate_cur_bad_stateid() {
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
            claim: OpenClaim4::ClaimDelegateCur(OpenClaimDelegateCur4 {
                delegate_stateid: Stateid4 { seqid: 1, other: [7; 12] },
                file: "delegfile".to_string(),
            }),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errBadStateid);
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

    /// Regression test for #33: OPEN for reading must create lock state so
    /// OPEN_CONFIRM succeeds. Without this, the client gets NFS4ERR_BAD_STATEID.
    #[tokio::test]
    async fn test_open_read_creates_lock_state_for_confirm() {
        use nextnfs_proto::nfs4_proto::{OpenConfirm4args, OpenConfirm4res};

        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Create a file first
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("read_lock_test.txt").unwrap().create_file().unwrap();
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // OPEN for reading (Open4Nocreate)
        let open_args = make_open_args("read_lock_test.txt", OpenFlag4::Open4Nocreate);
        let response = open_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Extract the stateid from OPEN result
        let open_stateid = match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => resok.stateid.clone(),
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        };

        // The current filehandle should have locks so OPEN_CONFIRM succeeds
        let request = response.request;
        let fh = request.current_filehandle().expect("must have current fh after OPEN");
        assert!(
            !fh.locks.is_empty(),
            "OPEN for reading must populate filehandle locks (got empty)"
        );

        // OPEN_CONFIRM must succeed
        let confirm_args = OpenConfirm4args {
            open_stateid,
            seqid: 1,
        };
        let response = confirm_args.execute(request).await;
        assert_eq!(
            response.status, NfsStat4::Nfs4Ok,
            "OPEN_CONFIRM after OPEN-for-reading must succeed, got {:?}",
            response.status
        );
        match &response.result {
            Some(NfsResOp4::OpopenConfirm(OpenConfirm4res::Resok4(resok))) => {
                assert_ne!(resok.open_stateid.other, [0; 12], "stateid must not be all-zeros");
            }
            other => panic!("Expected OpopenConfirm Resok4, got {:?}", other),
        }
    }

    /// Regression test for #33: OPEN read must return a non-zero stateid.
    #[tokio::test]
    async fn test_open_read_returns_valid_stateid() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("stateid_test.txt").unwrap().create_file().unwrap();
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let args = make_open_args("stateid_test.txt", OpenFlag4::Open4Nocreate);
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        match &response.result {
            Some(NfsResOp4::Opopen(Open4res::Resok4(resok))) => {
                // A valid OPEN must return a non-zero stateid
                assert_ne!(
                    resok.stateid.other, [0; 12],
                    "OPEN for reading returned all-zero stateid"
                );
            }
            other => panic!("Expected Opopen Resok4, got {:?}", other),
        }
    }
}
