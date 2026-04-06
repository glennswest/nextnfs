use std::io::{Seek, SeekFrom, Write};

use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{Allocate4args, Allocate4res, NfsResOp4, NfsStat4};

#[async_trait]
impl NfsOperation for Allocate4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 59: ALLOCATE offset={} length={}",
            self.aa_offset, self.aa_length
        );

        let filehandle = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("ALLOCATE: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opallocate(Allocate4res {
                        ar_status: NfsStat4::Nfs4errFhexpired,
                    })),
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        // Check quota before allocating
        if let Some(qm) = request.quota_manager() {
            if !qm.check_write(self.aa_length) {
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opallocate(Allocate4res {
                        ar_status: NfsStat4::Nfs4errDquot,
                    })),
                    status: NfsStat4::Nfs4errDquot,
                };
            }
        }

        // Open file for writing to extend it if needed
        let mut wfile = match filehandle.file.append_file() {
            Ok(f) => f,
            Err(e) => {
                error!("ALLOCATE: open for write failed: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opallocate(Allocate4res {
                        ar_status: NfsStat4::Nfs4errIo,
                    })),
                    status: NfsStat4::Nfs4errIo,
                };
            }
        };

        // Get current file size
        let file_size = match wfile.seek(SeekFrom::End(0)) {
            Ok(s) => s,
            Err(e) => {
                error!("ALLOCATE: seek failed: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opallocate(Allocate4res {
                        ar_status: NfsStat4::Nfs4errIo,
                    })),
                    status: NfsStat4::Nfs4errIo,
                };
            }
        };

        // If the allocation extends past the end, pad with zeros
        let alloc_end = self.aa_offset + self.aa_length;
        if alloc_end > file_size {
            let pad_size = (alloc_end - file_size) as usize;
            // Write zeros in chunks to avoid huge allocations
            let chunk_size = 64 * 1024; // 64KB
            let mut remaining = pad_size;
            while remaining > 0 {
                let write_size = remaining.min(chunk_size);
                let zeros = vec![0u8; write_size];
                if let Err(e) = wfile.write_all(&zeros) {
                    error!("ALLOCATE: write failed: {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: Some(NfsResOp4::Opallocate(Allocate4res {
                            ar_status: NfsStat4::Nfs4errIo,
                        })),
                        status: NfsStat4::Nfs4errIo,
                    };
                }
                remaining -= write_size;
            }

            // Track quota for newly allocated space
            if let Some(qm) = request.quota_manager() {
                qm.record_write(alloc_end - file_size);
            }
        }

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opallocate(Allocate4res {
                ar_status: NfsStat4::Nfs4Ok,
            })),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{NfsResOp4, NfsStat4, Read4args, Read4res, Read4resok, Stateid4, StableHow4, Write4args},
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use nextnfs_proto::nfs4_proto::Allocate4args;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_allocate_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Allocate4args {
            aa_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            aa_offset: 0,
            aa_length: 1024,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_allocate_extends_file() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        root.join("alloctest").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .get_filehandle_for_path("alloctest".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // ALLOCATE 1000 bytes from offset 0
        let args = Allocate4args {
            aa_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            aa_offset: 0,
            aa_length: 1000,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // READ the file — should have 1000 zero bytes
        let read_args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            count: 2000,
        };
        let response = read_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(Read4resok { data, eof }))) =
            &response.result
        {
            assert_eq!(data.len(), 1000);
            assert!(data.iter().all(|&b| b == 0));
            assert!(*eof);
        } else {
            panic!("Expected Read4resok");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_allocate_within_existing_size() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        root.join("allocexist").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .get_filehandle_for_path("allocexist".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // Write 500 bytes
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: vec![0xFF; 500],
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // ALLOCATE 100 bytes from offset 0 — file already >= 100 bytes, no-op
        let args = Allocate4args {
            aa_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            aa_offset: 0,
            aa_length: 100,
        };
        let response = args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // READ — should still be 500 bytes of 0xFF
        let read_args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            count: 1000,
        };
        let response = read_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(Read4resok { data, .. }))) =
            &response.result
        {
            assert_eq!(data.len(), 500);
            assert!(data.iter().all(|&b| b == 0xFF));
        } else {
            panic!("Expected Read4resok");
        }
    }
}
