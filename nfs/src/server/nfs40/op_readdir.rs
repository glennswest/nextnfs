use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{
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
        let dir = dir_fh.file.read_dir().unwrap();

        let mut fnames = Vec::new();
        let mut filehandles = Vec::new();
        let dircount: usize = self.dircount as usize;
        let maxcount: usize = self.maxcount as usize;
        let mut maxcount_actual: usize = 128;
        let mut dircount_actual = 0;
        for (i, entry) in dir.enumerate() {
            let name = entry.filename();
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
            let mut diff = 8 - cookieverf.len();
            while diff > 0 {
                cookieverf.push(0);
                diff -= 1;
            }
        }

        let mut tnextentry = None;
        let mut added_entries = 0;
        for (cookie, fh) in filehandles.into_iter().rev() {
            let resp = request
                .file_manager()
                .filehandle_attrs(&self.attr_request, &fh);
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
                nextentry: if tnextentry.is_some() {
                    Some(Box::new(tnextentry.unwrap()))
                } else {
                    None
                },
            };
            added_entries += 1;
            tnextentry = Some(entry);
        }
        let eof = {
            if tnextentry.is_some()
                && (tnextentry.clone().unwrap().cookie + added_entries) >= fnames.len() as u64
            {
                true
            } else {
                tnextentry.is_none()
            }
        };

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opreaddir(ReadDir4res::Resok4(ReadDir4resok {
                reply: DirList4 {
                    entries: tnextentry.clone(),
                    eof,
                },
                cookieverf: cookieverf.as_slice().try_into().unwrap(),
            }))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}
