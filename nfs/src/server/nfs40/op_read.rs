use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{NfsResOp4, NfsStat4, Read4args, Read4res, Read4resok};

#[async_trait]
impl NfsOperation for Read4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 25: READ offset={} count={}",
            self.offset, self.count
        );
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

        // Use VFS open_file
        match filehandle.file.open_file() {
            Ok(mut rfile) => {
                // Get actual file size from the open handle
                let file_size = match rfile.seek(SeekFrom::End(0)) {
                    Ok(size) => size,
                    Err(_) => filehandle.attr_size,
                };

                // Seek to read position
                if let Err(e) = rfile.seek(SeekFrom::Start(self.offset)) {
                    error!("seek failed: {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errIo,
                    };
                }

                // Allocate buffer — cap at remaining bytes to avoid oversized alloc
                let remaining = file_size.saturating_sub(self.offset);
                let read_size = (self.count as u64).min(remaining) as usize;
                let mut buffer = vec![0u8; read_size];

                let bytes_read = match rfile.read(&mut buffer) {
                    Ok(n) => n,
                    Err(e) => {
                        error!("read failed: {:?}", e);
                        return NfsOpResponse {
                            request,
                            result: None,
                            status: NfsStat4::Nfs4errIo,
                        };
                    }
                };

                buffer.truncate(bytes_read);
                let eof = (self.offset + bytes_read as u64) >= file_size;

                // Update per-export read stats
                if let Some(stats) = request.export_stats() {
                    stats.reads.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_read.fetch_add(bytes_read as u64, Ordering::Relaxed);
                }

                NfsOpResponse {
                    request,
                    result: Some(NfsResOp4::Opread(Read4res::Resok4(Read4resok {
                        eof,
                        data: buffer,
                    }))),
                    status: NfsStat4::Nfs4Ok,
                }
            }
            Err(e) => {
                error!("open_file failed: {:?}", e);
                NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errIo,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{NfsStat4, Read4args, Stateid4},
            operation::NfsOperation,
        },
        test_utils::{create_nfs40_server, create_nfs40_server_with_root_fh},
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_read_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Read4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0u8; 12],
            },
            offset: 0,
            count: 4096,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_directory_fails() {
        // READ on a directory should fail with I/O error (can't open_file on dir)
        let request = create_nfs40_server_with_root_fh(None).await;
        let args = Read4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0u8; 12],
            },
            offset: 0,
            count: 4096,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errIo);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_empty_file() {
        use crate::server::nfs40::{NfsResOp4, Read4res};
        let mut request = create_nfs40_server_with_root_fh(None).await;
        // Create a file via FM and set as current fh
        let fh = request
            .file_manager()
            .create_file(
                request.current_filehandle().unwrap().file.join("empty.txt").unwrap(),
                1, b"owner".to_vec(), 1, 0, None,
            )
            .await
            .unwrap();
        request.set_filehandle(fh);

        let args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            count: 4096,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(resok))) = response.result {
            assert!(resok.eof);
            assert!(resok.data.is_empty());
        } else {
            panic!("Expected Read4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_file_with_data() {
        use crate::server::nfs40::{NfsResOp4, Read4res};
        use std::io::Write;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        // Create file with content
        root_file.join("data.txt").unwrap().create_file().unwrap();
        {
            let mut f = root_file.join("data.txt").unwrap().append_file().unwrap();
            f.write_all(b"hello world").unwrap();
        }
        let fh = request
            .file_manager()
            .get_filehandle_for_path("data.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        let args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            count: 4096,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(resok))) = response.result {
            assert_eq!(resok.data, b"hello world");
            assert!(resok.eof);
        } else {
            panic!("Expected Read4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_with_offset() {
        use crate::server::nfs40::{NfsResOp4, Read4res};
        use std::io::Write;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("offset.txt").unwrap().create_file().unwrap();
        {
            let mut f = root_file.join("offset.txt").unwrap().append_file().unwrap();
            f.write_all(b"abcdefghij").unwrap();
        }
        let fh = request
            .file_manager()
            .get_filehandle_for_path("offset.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        let args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 5,
            count: 4096,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(resok))) = response.result {
            assert_eq!(resok.data, b"fghij");
            assert!(resok.eof);
        } else {
            panic!("Expected Read4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_at_exact_eof() {
        use crate::server::nfs40::{NfsResOp4, Read4res};
        use std::io::Write;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("ateof.txt").unwrap().create_file().unwrap();
        {
            let mut f = root_file.join("ateof.txt").unwrap().append_file().unwrap();
            f.write_all(b"short").unwrap();
        }
        let fh = request
            .file_manager()
            .get_filehandle_for_path("ateof.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // Read at the exact end of file (offset == file_size)
        let args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 5, // file is 5 bytes
            count: 4096,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(resok))) = response.result {
            assert!(resok.data.is_empty());
            assert!(resok.eof);
        } else {
            panic!("Expected Read4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_zero_count() {
        use crate::server::nfs40::{NfsResOp4, Read4res};
        use std::io::Write;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("zerocount.txt").unwrap().create_file().unwrap();
        {
            let mut f = root_file.join("zerocount.txt").unwrap().append_file().unwrap();
            f.write_all(b"content").unwrap();
        }
        let fh = request
            .file_manager()
            .get_filehandle_for_path("zerocount.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // Read with count=0
        let args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            count: 0,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(resok))) = response.result {
            assert!(resok.data.is_empty());
            // offset 0 + 0 bytes = 0, which is < file_size, so eof=false
            // Actually with count=0, read_size=0, bytes_read=0, 0+0=0 < 7, so not eof
            assert!(!resok.eof);
        } else {
            panic!("Expected Read4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_partial_count() {
        use crate::server::nfs40::{NfsResOp4, Read4res};
        use std::io::Write;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("partial.txt").unwrap().create_file().unwrap();
        {
            let mut f = root_file.join("partial.txt").unwrap().append_file().unwrap();
            f.write_all(b"0123456789").unwrap();
        }
        let fh = request
            .file_manager()
            .get_filehandle_for_path("partial.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        // Read only 3 bytes
        let args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            count: 3,
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(resok))) = response.result {
            assert_eq!(resok.data, b"012");
            assert!(!resok.eof); // not at end
        } else {
            panic!("Expected Read4res::Resok4");
        }
    }
}
