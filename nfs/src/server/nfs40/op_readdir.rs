use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    filemanager::QuotaInfo,
    nfs40::op_pseudo, operation::NfsOperation, request::NfsRequest, response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    DirList4, Entry4, Fattr4, NfsResOp4, NfsStat4, ReadDir4res, ReadDir4resok, Readdir4args,
};

#[async_trait]
impl NfsOperation for Readdir4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 26: READDIR - Read Directory {:?}, with request {:?}",
            self, request
        );

        // If on pseudo-root, list exports
        if request.is_pseudo_root() {
            let em = request.export_manager();
            let (entries, eof) =
                op_pseudo::pseudo_readdir(&em, &self.attr_request, self.cookie).await;

            // Build linked list from entries (reverse order for linked list construction)
            let mut next_entry = None;
            for entry in entries.into_iter().rev() {
                let mut e = entry;
                e.nextentry = next_entry.map(Box::new);
                next_entry = Some(e);
            }

            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(ReadDir4resok {
                    reply: DirList4 {
                        entries: next_entry,
                        eof,
                    },
                    cookieverf: [0u8; 8],
                }))),
                status: NfsStat4::Nfs4Ok,
            };
        }

        let current_fh = request.current_filehandle();
        let dir_fh = match current_fh {
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
        let dir = match dir_fh.file.read_dir() {
            Ok(d) => d,
            Err(e) => {
                error!("READDIR: read_dir failed: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errIo,
                };
            }
        };

        let mut fnames = Vec::new();
        let mut filehandles = Vec::new();
        let dircount: usize = self.dircount as usize;
        let maxcount: usize = self.maxcount as usize;
        let mut maxcount_actual: usize = 128;
        let mut dircount_actual = 0;
        for (i, entry) in dir.enumerate() {
            let name = entry.filename();
            // Hide the named-attribute store from directory listings
            if name == ".nfs4attrs" {
                continue;
            }
            fnames.push(name.clone());
            if (i + 2) >= self.cookie as usize {
                dircount_actual = dircount_actual + 8 + name.len() + 5;
                maxcount_actual += 200;
                if dircount == 0 || (dircount > dircount_actual && maxcount > maxcount_actual) {
                    let filehandle = request
                        .file_manager()
                        .get_filehandle_for_path(entry.as_str().to_string())
                        .await;
                    match filehandle {
                        Err(_e) => {
                            error!("None filehandle");
                            return NfsOpResponse {
                                request,
                                result: None,
                                status: NfsStat4::Nfs4errFhexpired,
                            };
                        }
                        Ok(filehandle) => {
                            filehandles.push((i + 3, filehandle));
                        }
                    }
                }
            }
        }

        let seed: String = fnames
            .iter()
            .flat_map(|s| s.as_str().chars().collect::<Vec<_>>())
            .collect();
        let mut cookieverf = seed
            .as_bytes()
            .iter()
            .step_by(seed.len() / 8 + 1)
            .copied()
            .collect::<Vec<_>>();
        if self.cookie != 0 && cookieverf != self.cookieverf {
            error!("Nfs4errNotSame");
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errNotSame,
            };
        }

        if cookieverf.is_empty() {
            cookieverf = [0u8; 8].to_vec();
        } else if cookieverf.len() < 8 {
            cookieverf.resize(8, 0);
        } else if cookieverf.len() > 8 {
            cookieverf.truncate(8);
        }

        let quota_info = request.quota_manager().map(|qm| {
            const DEFAULT_SPACE_TOTAL: u64 = 1_099_511_627_776;
            let used = qm.bytes_used();
            QuotaInfo {
                quota_avail_hard: qm.quota_avail_hard(),
                quota_avail_soft: qm.quota_avail_soft(),
                quota_used: used,
                space_total: DEFAULT_SPACE_TOTAL,
                space_free: DEFAULT_SPACE_TOTAL.saturating_sub(used),
                space_avail: DEFAULT_SPACE_TOTAL.saturating_sub(used),
            }
        });

        let mut tnextentry = None;
        let mut added_entries = 0;
        for (cookie, fh) in filehandles.into_iter().rev() {
            let resp = request
                .file_manager()
                .filehandle_attrs(&self.attr_request, &fh, quota_info.as_ref());
            let (answer_attrs, attrs) = match resp {
                Some(inner) => inner,
                None => {
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errServerfault,
                    };
                }
            };

            let entry = Entry4 {
                name: fh.file.filename(),
                cookie: cookie as u64,
                attrs: Fattr4 {
                    attrmask: answer_attrs,
                    attr_vals: attrs,
                },
                nextentry: tnextentry.map(Box::new),
            };
            added_entries += 1;
            tnextentry = Some(entry);
        }
        let eof = match tnextentry {
            Some(ref entry) => (entry.cookie + added_entries) >= fnames.len() as u64,
            None => true,
        };

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(ReadDir4resok {
                reply: DirList4 {
                    entries: tnextentry.clone(),
                    eof,
                },
                cookieverf: cookieverf
                    .as_slice()
                    .try_into()
                    .unwrap_or([0u8; 8]),
            }))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{
                Attrlist4, Create4args, Createtype4, Fattr4, FileAttr, NfsResOp4,
                NfsStat4, ReadDir4res, Readdir4args,
            },
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_readdir_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Readdir4args {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 4096,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_readdir_empty_root() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Readdir4args {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 4096,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) = response.result {
            // Empty directory — should have no entries and eof=true
            assert!(resok.reply.eof);
        } else {
            panic!("Expected Opreaddir Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_readdir_with_entries() {
        // Create some directories, chaining through the same request/VFS
        let request = create_nfs40_server_with_root_fh(None).await;
        let create1 = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "dir_a".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create1.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Chain request — reset to root for second create
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let create2 = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "dir_b".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create2.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Chain again — reset to root for readdir
        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 4096,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) = response.result {
            assert!(resok.reply.entries.is_some());
        } else {
            panic!("Expected Opreaddir Resok4 with entries");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_readdir_eof_flag() {
        let request = create_nfs40_server_with_root_fh(None).await;
        // Create a single entry
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "single".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 4096,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) = response.result {
            assert!(resok.reply.eof);
        } else {
            panic!("Expected Opreaddir Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_readdir_stale_cookieverf() {
        // If cookie != 0 and cookieverf doesn't match, should return Nfs4errNotSame
        let request = create_nfs40_server_with_root_fh(None).await;
        // Create an entry so we have a non-trivial directory
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "staledir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        // Use a non-zero cookie with a fabricated verifier
        let args = Readdir4args {
            cookie: 3, // non-zero triggers verifier check
            cookieverf: [0xFF; 8], // wrong verifier
            dircount: 4096,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errNotSame);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_readdir_shows_dot_files() {
        let request = create_nfs40_server_with_root_fh(None).await;
        // Create a hidden dir and a normal dir
        let create_hidden = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: ".hidden".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_hidden.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let create_normal = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "visible".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_normal.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 4096,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) = response.result {
            // Collect all names from the linked list
            let mut names = vec![];
            let mut entry = resok.reply.entries.as_ref();
            while let Some(e) = entry {
                names.push(e.name.clone());
                entry = e.nextentry.as_deref();
            }
            assert!(names.contains(&".hidden".to_string()), "readdir should include dot files, got: {:?}", names);
            assert!(names.contains(&"visible".to_string()), "readdir should include normal dirs, got: {:?}", names);
        } else {
            panic!("Expected Opreaddir Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_readdir_multiple_attrs() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let create_args = Create4args {
            objtype: Createtype4::Nf4dir,
            objname: "attrdir".to_string(),
            createattrs: Fattr4 {
                attrmask: Attrlist4(vec![]),
                attr_vals: Attrlist4(vec![]),
            },
        };
        let response = create_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let mut request = response.request;
        let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
        request.set_filehandle(root_fh);

        let readdir_args = Readdir4args {
            cookie: 0,
            cookieverf: [0u8; 8],
            dircount: 4096,
            maxcount: 8192,
            attr_request: Attrlist4(vec![FileAttr::Type, FileAttr::Size, FileAttr::Mode]),
        };
        let response = readdir_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) = response.result {
            assert!(resok.reply.entries.is_some());
            let entry = resok.reply.entries.unwrap();
            assert_eq!(entry.attrs.attrmask.len(), 3);
        } else {
            panic!("Expected Opreaddir Resok4");
        }
    }
}
