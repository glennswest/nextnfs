use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
    filemanager::QuotaInfo,
    nfs40::op_pseudo, operation::NfsOperation, request::NfsRequest, response::NfsOpResponse,
};

use nextnfs_proto::nfs4_proto::{
    DirList4, Entry4, Fattr4, NfsResOp4, NfsStat4, ReadDir4res, ReadDir4resok, Readdir4args,
};

/// The cookie verifier of a directory listing: FNV-1a 64 over its names, in
/// listing order (#130, finding 33b440b1f96e). It is always eight bytes, the
/// same value returned and compared, and it changes exactly when the mapping
/// from cookie (position) to name does, which is what makes an old cookie
/// meaningless.
pub(crate) fn listing_verifier<'a>(names: impl IntoIterator<Item = &'a str>) -> [u8; 8] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for name in names {
        for b in name.bytes().chain(std::iter::once(0u8)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h.to_be_bytes()
}

/// XDR size of READDIR4resok without its entries: cookieverf (8), the
/// list's final "no more entries" (4) and eof (4).
const RESOK_HEADER: usize = 16;

/// The reply-size budget of one READDIR (#130, finding 9078222b42e0).
///
/// `maxcount` bounds the encoded READDIR4resok (RFC 7530 §16.24.4) and is
/// always enforced, whatever `dircount` says; each entry is counted at its
/// real XDR size (cookie, name, attribute mask and values, list link).
/// `dircount` is a hint for the cookie and name bytes: 0 means no hint, and
/// the first entry is never refused because of it.
pub(crate) struct Budget {
    maxcount: usize,
    dircount: usize,
    used: usize,
    dir_used: usize,
    taken: usize,
}

impl Budget {
    /// `Err(NFS4ERR_TOOSMALL)` when `maxcount` cannot hold even an empty
    /// reply.
    pub(crate) fn new(dircount: u32, maxcount: u32) -> Result<Self, NfsStat4> {
        let maxcount = maxcount as usize;
        if maxcount < RESOK_HEADER {
            return Err(NfsStat4::Nfs4errToosmall);
        }
        Ok(Budget {
            maxcount,
            dircount: dircount as usize,
            used: RESOK_HEADER,
            dir_used: 0,
            taken: 0,
        })
    }

    /// Count `entry` (its `nextentry` is not looked at) if it fits.
    pub(crate) fn take(&mut self, entry: &Entry4) -> bool {
        let size = entry_xdr_len(entry);
        if self.used + size > self.maxcount {
            return false;
        }
        let dir_size = 8 + 4 + entry.name.len().div_ceil(4) * 4;
        if self.dircount != 0 && self.taken > 0 && self.dir_used + dir_size > self.dircount {
            return false;
        }
        self.used += size;
        self.dir_used += dir_size;
        self.taken += 1;
        true
    }

    pub(crate) fn taken(&self) -> usize {
        self.taken
    }
}

/// Encoded size of `entry` in a dirlist4: its "value follows" word (4),
/// cookie, name, fattr4 — measured with the codec, so attribute values are
/// counted as sent.
fn entry_xdr_len(entry: &Entry4) -> usize {
    let alone = Entry4 {
        cookie: entry.cookie,
        name: entry.name.clone(),
        attrs: entry.attrs.clone(),
        nextentry: None,
    };
    // Serialized alone it ends with its own "no next entry" word, which
    // stands for the "value follows" word it gets in the list.
    nextnfs_proto::xdr::to_bytes(&alone).map(|b| b.len()).unwrap_or(usize::MAX / 2)
}

/// Link `entries` (in cookie order) into a dirlist4 chain.
pub(crate) fn link_entries(entries: Vec<Entry4>) -> Option<Entry4> {
    let mut next = None;
    for mut e in entries.into_iter().rev() {
        e.nextentry = next.map(Box::new);
        next = Some(e);
    }
    next
}

fn readdir_ok(
    request: NfsRequest<'_>,
    entries: Vec<Entry4>,
    eof: bool,
    cookieverf: [u8; 8],
) -> NfsOpResponse<'_> {
    NfsOpResponse {
        request,
        result: Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(ReadDir4resok {
            reply: DirList4 {
                entries: link_entries(entries),
                eof,
            },
            cookieverf,
        }))),
        status: NfsStat4::Nfs4Ok,
    }
}

fn readdir_err(request: NfsRequest<'_>, status: NfsStat4) -> NfsOpResponse<'_> {
    NfsOpResponse {
        request,
        result: None,
        status,
    }
}

#[async_trait]
impl NfsOperation for Readdir4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 26: READDIR - Read Directory {:?}, with request {:?}",
            self, request
        );

        let mut budget = match Budget::new(self.dircount, self.maxcount) {
            Ok(b) => b,
            Err(status) => return readdir_err(request, status),
        };

        // If on pseudo-root, list exports (within the same budget)
        if request.is_pseudo_root() {
            let em = request.export_manager();
            let (candidates, cookieverf) = op_pseudo::pseudo_readdir(
                &em,
                request.client_addr(),
                &self.attr_request,
                self.cookie,
            )
            .await;
            if self.cookie != 0 && self.cookieverf != cookieverf {
                return readdir_err(request, NfsStat4::Nfs4errNotSame);
            }
            let total = candidates.len();
            let entries: Vec<Entry4> =
                candidates.into_iter().take_while(|e| budget.take(e)).collect();
            if entries.is_empty() && total > 0 {
                return readdir_err(request, NfsStat4::Nfs4errToosmall);
            }
            let eof = entries.len() == total;
            return readdir_ok(request, entries, eof, cookieverf);
        }

        let current_fh = request.current_filehandle();
        let dir_fh = match current_fh {
            Some(filehandle) => filehandle,
            None => {
                error!("None filehandle");
                return readdir_err(request, NfsStat4::Nfs4errFhexpired);
            }
        };
        let dir = match dir_fh.file.read_dir() {
            Ok(d) => d,
            Err(e) => {
                error!("READDIR: read_dir failed: {:?}", e);
                return readdir_err(request, NfsStat4::Nfs4errIo);
            }
        };

        // The whole listing, in order: an entry's cookie is its position + 3
        // (0, 1 and 2 are reserved, RFC 7530 §16.24.4).
        // Hide the named-attribute store and silly-rename files from listings
        let listing: Vec<_> = dir
            .filter(|entry| {
                let name = entry.filename();
                name != ".nfs4attrs" && !name.starts_with(".nfs.")
            })
            .collect();
        let names: Vec<String> = listing.iter().map(|e| e.filename()).collect();
        let cookieverf = listing_verifier(names.iter().map(String::as_str));
        if self.cookie != 0 && cookieverf != self.cookieverf {
            error!("Nfs4errNotSame");
            return readdir_err(request, NfsStat4::Nfs4errNotSame);
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

        // Entries after the client's cookie, each built and counted in turn
        // until one does not fit.
        let first = (self.cookie as usize).saturating_sub(2);
        let mut entries = Vec::new();
        let mut eof = true;
        for (idx, (entry, name)) in listing.iter().zip(&names).enumerate().skip(first) {
            let fh = match request
                .file_manager()
                .get_filehandle_for_path(entry.as_str().to_string())
                .await
            {
                Ok(fh) => fh,
                Err(_e) => {
                    error!("None filehandle");
                    return readdir_err(request, NfsStat4::Nfs4errFhexpired);
                }
            };
            let (answer_attrs, attrs) = match request.file_manager().filehandle_attrs(
                &self.attr_request,
                &fh,
                quota_info.as_ref(),
            ) {
                Some(inner) => inner,
                None => return readdir_err(request, NfsStat4::Nfs4errServerfault),
            };
            let entry = Entry4 {
                name: name.clone(),
                cookie: (idx + 3) as u64,
                attrs: Fattr4 {
                    attrmask: answer_attrs,
                    attr_vals: attrs,
                },
                nextentry: None,
            };
            if !budget.take(&entry) {
                eof = false;
                break;
            }
            entries.push(entry);
        }
        if budget.taken() == 0 && !eof {
            // Not even one entry fits in maxcount.
            return readdir_err(request, NfsStat4::Nfs4errToosmall);
        }

        readdir_ok(request, entries, eof, cookieverf)
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

    // --- #130: fixed-size verifier, maxcount always enforced ---

    use super::{listing_verifier, RESOK_HEADER};
    use crate::server::request::NfsRequest;
    use nextnfs_proto::nfs4_proto::ReadDir4resok;

    fn args(cookie: u64, cookieverf: [u8; 8], dircount: u32, maxcount: u32, attrs: Vec<FileAttr>) -> Readdir4args {
        Readdir4args { cookie, cookieverf, dircount, maxcount, attr_request: Attrlist4(attrs) }
    }

    async fn readdir(
        request: NfsRequest<'_>,
        a: Readdir4args,
    ) -> (NfsRequest<'_>, Result<ReadDir4resok, NfsStat4>) {
        let response = a.execute(request).await;
        let result = match response.result {
            Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(resok))) => Ok(resok),
            _ => Err(response.status.clone()),
        };
        if result.is_ok() {
            assert_eq!(response.status, NfsStat4::Nfs4Ok);
        }
        (response.request, result)
    }

    fn entries(resok: &ReadDir4resok) -> Vec<(String, u64)> {
        let mut out = vec![];
        let mut e = resok.reply.entries.as_ref();
        while let Some(entry) = e {
            out.push((entry.name.clone(), entry.cookie));
            e = entry.nextentry.as_deref();
        }
        out
    }

    fn encoded_len(resok: &ReadDir4resok) -> usize {
        nextnfs_proto::xdr::to_bytes(resok).unwrap().len()
    }

    /// Page through the root with `dircount`/`maxcount`, continuing with the
    /// cookie and verifier each reply gives; every page must fit `maxcount`.
    async fn read_all(
        mut request: NfsRequest<'_>,
        dircount: u32,
        maxcount: u32,
        attrs: Vec<FileAttr>,
    ) -> (NfsRequest<'_>, Vec<String>, usize) {
        let (mut cookie, mut verf, mut names, mut pages) = (0u64, [0u8; 8], vec![], 0);
        loop {
            let (r, result) = readdir(request, args(cookie, verf, dircount, maxcount, attrs.clone())).await;
            request = r;
            let resok = result.unwrap_or_else(|s| panic!("page {} cookie {}: {:?}", pages, cookie, s));
            pages += 1;
            assert!(
                encoded_len(&resok) <= maxcount as usize,
                "page {} is {} bytes, maxcount {}",
                pages,
                encoded_len(&resok),
                maxcount
            );
            let page = entries(&resok);
            if resok.reply.eof {
                names.extend(page.into_iter().map(|(n, _)| n));
                break;
            }
            assert!(!page.is_empty(), "a page that is not eof has entries");
            cookie = page.last().unwrap().1;
            verf = resok.cookieverf;
            names.extend(page.into_iter().map(|(n, _)| n));
            assert!(pages < 1000, "no progress");
        }
        (request, names, pages)
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[tokio::test]
    async fn test_readdir_seven_byte_name_continues() {
        // Finding 33b440b1f96e: a 7-byte name sampled into 7 verifier bytes,
        // padded to 8 on the wire, so the continuation got NOT_SAME.
        let request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        root.join("abcdefg").unwrap().create_file().unwrap();
        let (request, first) = readdir(request, args(0, [0; 8], 0, 8192, vec![FileAttr::Type])).await;
        let first = first.unwrap();
        assert_eq!(entries(&first), vec![("abcdefg".to_string(), 3)]);
        let (_, next) = readdir(request, args(3, first.cookieverf, 0, 8192, vec![FileAttr::Type])).await;
        let next = next.expect("continuation with the returned verifier is accepted");
        assert!(next.reply.eof);
        assert!(entries(&next).is_empty());
    }

    #[tokio::test]
    async fn test_readdir_verifier_is_fixed_size_for_every_name_length() {
        for len in 1..=20 {
            let request = create_nfs40_server_with_root_fh(None).await;
            let root = request.current_filehandle().unwrap().file.clone();
            for i in 0..3 {
                root.join(format!("{}{}", i, "x".repeat(len))).unwrap().create_file().unwrap();
            }
            // One entry per page, so every page after the first continues
            // with a returned verifier. The names are the same length, so
            // the entries are the same size.
            let (request, all) = readdir(request, args(0, [0; 8], 0, 8192, vec![FileAttr::Type])).await;
            let one = (encoded_len(&all.unwrap()) - RESOK_HEADER) / 3;
            let maxcount = (RESOK_HEADER + one + one / 2) as u32;
            let (_, names, pages) = read_all(request, 0, maxcount, vec![FileAttr::Type]).await;
            assert_eq!(names.len(), 3, "name length {}", len);
            assert_eq!(pages, 3, "name length {}: one entry per page", len);
        }
    }

    #[tokio::test]
    async fn test_readdir_verifier_changes_with_directory() {
        let request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        for n in ["a", "b", "c"] {
            root.join(n).unwrap().create_file().unwrap();
        }
        let (request, first) = readdir(request, args(0, [0; 8], 0, 60, vec![])).await;
        let first = first.unwrap();
        assert!(!first.reply.eof);
        let cookie = entries(&first).last().unwrap().1;
        root.join("d").unwrap().create_file().unwrap();
        let (_, next) = readdir(request, args(cookie, first.cookieverf, 0, 60, vec![])).await;
        assert_eq!(next.unwrap_err(), NfsStat4::Nfs4errNotSame);
        assert_ne!(listing_verifier(["a", "b"]), listing_verifier(["ab"]));
    }

    #[tokio::test]
    async fn test_readdir_dircount_zero_still_bounded_by_maxcount() {
        // Finding 9078222b42e0: dircount 0 returned every entry whatever
        // maxcount said.
        let request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        let want: Vec<String> = (0..200).map(|i| format!("file_{:03}", i)).collect();
        for n in &want {
            root.join(n).unwrap().create_file().unwrap();
        }
        let (request, first) = readdir(request, args(0, [0; 8], 0, 1024, vec![FileAttr::Type])).await;
        let first = first.unwrap();
        assert!(!first.reply.eof);
        assert!(encoded_len(&first) <= 1024);
        assert!(!entries(&first).is_empty());
        let (_, names, _) = read_all(request, 0, 1024, vec![FileAttr::Type]).await;
        assert_eq!(sorted(names), want);
    }

    #[tokio::test]
    async fn test_readdir_counts_real_attribute_sizes() {
        // Large attributes (owner strings, times) must be counted as sent,
        // not as a fixed per-entry guess.
        let attrs = vec![
            FileAttr::Type,
            FileAttr::Change,
            FileAttr::Size,
            FileAttr::Fileid,
            FileAttr::Mode,
            FileAttr::Numlinks,
            FileAttr::Owner,
            FileAttr::OwnerGroup,
            FileAttr::TimeAccess,
            FileAttr::TimeMetadata,
            FileAttr::TimeModify,
        ];
        for (dircount, maxcount) in [(0, 256), (0, 333), (4096, 512), (50, 4096), (0, 65536)] {
            let request = create_nfs40_server_with_root_fh(None).await;
            let root = request.current_filehandle().unwrap().file.clone();
            let want: Vec<String> = (0..40).map(|i| format!("entry-{:02}-{}", i, "n".repeat(i))).collect();
            for n in &want {
                root.join(n).unwrap().create_file().unwrap();
            }
            let (_, names, _) = read_all(request, dircount, maxcount, attrs.clone()).await;
            assert_eq!(sorted(names), sorted(want), "dircount {} maxcount {}", dircount, maxcount);
        }
    }

    #[tokio::test]
    async fn test_readdir_toosmall_when_no_entry_fits() {
        let request = create_nfs40_server_with_root_fh(None).await;
        // An empty directory's reply is the header alone.
        let (request, empty) =
            readdir(request, args(0, [0; 8], 0, RESOK_HEADER as u32, vec![FileAttr::Type])).await;
        let empty = empty.unwrap();
        assert!(empty.reply.eof);
        assert_eq!(encoded_len(&empty), RESOK_HEADER);
        let (request, tiny) = readdir(request, args(0, [0; 8], 0, 8, vec![FileAttr::Type])).await;
        assert_eq!(tiny.unwrap_err(), NfsStat4::Nfs4errToosmall);

        let root = request.current_filehandle().unwrap().file.clone();
        root.join("some_file").unwrap().create_file().unwrap();
        let (_, r) = readdir(request, args(0, [0; 8], 0, 24, vec![FileAttr::Type])).await;
        assert_eq!(r.unwrap_err(), NfsStat4::Nfs4errToosmall);
    }
}
