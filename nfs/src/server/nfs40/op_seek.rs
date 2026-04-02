use std::io::{Seek, SeekFrom};

use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{
    DataContent4, NfsResOp4, NfsStat4, Seek4args, Seek4res, SeekRes4,
};

#[async_trait]
impl NfsOperation for Seek4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 69: SEEK offset={} what={:?}",
            self.sa_offset, self.sa_what
        );

        let filehandle = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("SEEK: no current filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        // Open the file to inspect content
        let mut rfile = match filehandle.file.open_file() {
            Ok(f) => f,
            Err(e) => {
                error!("SEEK: open failed: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errIo,
                };
            }
        };

        // Get file size
        let file_size = match rfile.seek(SeekFrom::End(0)) {
            Ok(s) => s,
            Err(_) => filehandle.attr_size,
        };

        // If offset is at or past end, return EOF
        if self.sa_offset >= file_size {
            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opseek(Seek4res::Resok4(SeekRes4 {
                    sr_eof: true,
                    sr_offset: file_size,
                }))),
                status: NfsStat4::Nfs4Ok,
            };
        }

        // For VFS-backed files, we don't have sparse file support (no SEEK_DATA/SEEK_HOLE).
        // Treat the entire file as one contiguous data region:
        // - SEEK DATA from offset → returns offset (data starts here)
        // - SEEK HOLE from offset → returns file_size (hole at EOF)
        let (sr_eof, sr_offset) = match self.sa_what {
            DataContent4::Data => {
                // Data starts at current offset
                (false, self.sa_offset)
            }
            DataContent4::Hole => {
                // No holes in VFS files — first hole is at EOF
                (false, file_size)
            }
        };

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opseek(Seek4res::Resok4(SeekRes4 {
                sr_eof,
                sr_offset,
            }))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{NfsResOp4, NfsStat4, Stateid4, StableHow4, Write4args},
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use nextnfs_proto::nfs4_proto::{DataContent4, Seek4args, Seek4res};
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_seek_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Seek4args {
            sa_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            sa_offset: 0,
            sa_what: DataContent4::Data,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_seek_data_on_empty_file() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        root.join("seekempty").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .get_filehandle_for_path("seekempty".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // SEEK DATA at offset 0 on empty file → EOF
        let args = Seek4args {
            sa_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            sa_offset: 0,
            sa_what: DataContent4::Data,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opseek(Seek4res::Resok4(res))) = &response.result {
            assert!(res.sr_eof);
        } else {
            panic!("Expected Seek4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_seek_data_with_content() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        root.join("seekdata").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .get_filehandle_for_path("seekdata".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // Write some data
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: vec![0xAA; 100],
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // SEEK DATA at offset 10 → should return offset 10 (data is there)
        let args = Seek4args {
            sa_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            sa_offset: 10,
            sa_what: DataContent4::Data,
        };
        let response = args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opseek(Seek4res::Resok4(res))) = &response.result {
            assert!(!res.sr_eof);
            assert_eq!(res.sr_offset, 10);
        } else {
            panic!("Expected Seek4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_seek_hole_returns_file_size() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();
        root.join("seekhole").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .get_filehandle_for_path("seekhole".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // Write data
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: vec![0xBB; 200],
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // SEEK HOLE at offset 0 → should return file_size (no holes in VFS)
        let args = Seek4args {
            sa_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            sa_offset: 0,
            sa_what: DataContent4::Hole,
        };
        let response = args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opseek(Seek4res::Resok4(res))) = &response.result {
            assert!(!res.sr_eof);
            assert_eq!(res.sr_offset, 200);
        } else {
            panic!("Expected Seek4res::Resok4");
        }
    }
}
