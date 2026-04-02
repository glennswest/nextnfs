use std::io::{Read, Seek, SeekFrom, Write};

use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};
use nextnfs_proto::nfs4_proto::{
    Copy4args, Copy4res, Copy4resok, CopyRequirements4, NfsResOp4, NfsStat4, StableHow4,
    WriteResponse4,
};

#[async_trait]
impl NfsOperation for Copy4args {
    async fn execute<'a>(&self, request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 60: COPY src_offset={} dst_offset={} count={}",
            self.ca_src_offset, self.ca_dst_offset, self.ca_count
        );

        // COPY reads from the saved filehandle (source) and writes to
        // the current filehandle (destination).
        let dst_fh = match request.current_filehandle() {
            Some(fh) => fh,
            None => {
                error!("COPY: no current (destination) filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        let src_fh = match request.saved_filehandle() {
            Some(fh) => fh,
            None => {
                error!("COPY: no saved (source) filehandle");
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errFhexpired,
                };
            }
        };

        // Check quota before copy
        if let Some(qm) = request.quota_manager() {
            if !qm.check_write(self.ca_count) {
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errDquot,
                };
            }
        }

        // Open source for reading
        let mut src_file = match src_fh.file.open_file() {
            Ok(f) => f,
            Err(e) => {
                error!("COPY: source open failed: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errIo,
                };
            }
        };

        // Get source file size
        let src_size = match src_file.seek(SeekFrom::End(0)) {
            Ok(s) => s,
            Err(_) => src_fh.attr_size,
        };

        // Clamp copy count to available source data
        let available = src_size.saturating_sub(self.ca_src_offset);
        let copy_count = if self.ca_count == 0 {
            available // 0 means "copy to end of file"
        } else {
            self.ca_count.min(available)
        };

        if copy_count == 0 {
            // Nothing to copy — success with 0 bytes
            return NfsOpResponse {
                request,
                result: Some(NfsResOp4::Opcopy(Copy4res::Resok4(Copy4resok {
                    cr_response: WriteResponse4 {
                        wr_callback_id: vec![],
                        wr_count: 0,
                        wr_committed: StableHow4::FileSync4,
                        wr_writeverf: [0; 8],
                    },
                    cr_requirements: CopyRequirements4 {
                        cr_consecutive: true,
                        cr_synchronous: true,
                    },
                }))),
                status: NfsStat4::Nfs4Ok,
            };
        }

        // Seek source to read position
        if let Err(e) = src_file.seek(SeekFrom::Start(self.ca_src_offset)) {
            error!("COPY: source seek failed: {:?}", e);
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errIo,
            };
        }

        // Open destination for writing
        let mut dst_file = match dst_fh.file.append_file() {
            Ok(f) => f,
            Err(e) => {
                error!("COPY: destination open failed: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errIo,
                };
            }
        };

        // Seek destination to write position
        if let Err(e) = dst_file.seek(SeekFrom::Start(self.ca_dst_offset)) {
            error!("COPY: destination seek failed: {:?}", e);
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errIo,
            };
        }

        // Copy loop in chunks
        let chunk_size = 256 * 1024; // 256KB
        let mut total_copied: u64 = 0;
        let mut buf = vec![0u8; chunk_size];

        while total_copied < copy_count {
            let to_read = ((copy_count - total_copied) as usize).min(chunk_size);
            let bytes_read = match src_file.read(&mut buf[..to_read]) {
                Ok(0) => break, // EOF
                Ok(n) => n,
                Err(e) => {
                    error!("COPY: source read failed: {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errIo,
                    };
                }
            };

            if let Err(e) = dst_file.write_all(&buf[..bytes_read]) {
                error!("COPY: destination write failed: {:?}", e);
                return NfsOpResponse {
                    request,
                    result: None,
                    status: NfsStat4::Nfs4errIo,
                };
            }

            total_copied += bytes_read as u64;
        }

        // Flush destination
        if let Err(e) = dst_file.flush() {
            error!("COPY: flush failed: {:?}", e);
            return NfsOpResponse {
                request,
                result: None,
                status: NfsStat4::Nfs4errIo,
            };
        }

        // Track quota for copied bytes
        if let Some(qm) = request.quota_manager() {
            qm.record_write(total_copied);
        }

        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opcopy(Copy4res::Resok4(Copy4resok {
                cr_response: WriteResponse4 {
                    wr_callback_id: vec![],
                    wr_count: total_copied,
                    wr_committed: StableHow4::FileSync4,
                    wr_writeverf: [0; 8],
                },
                cr_requirements: CopyRequirements4 {
                    cr_consecutive: true,
                    cr_synchronous: true,
                },
            }))),
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
    use nextnfs_proto::nfs4_proto::{Copy4args, Copy4res, Copy4resok};
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_copy_no_destination() {
        let request = create_nfs40_server(None).await;
        let args = Copy4args {
            ca_src_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            ca_dst_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            ca_src_offset: 0,
            ca_dst_offset: 0,
            ca_count: 100,
            ca_consecutive: true,
            ca_synchronous: true,
            ca_source_server: vec![],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_copy_file() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();

        // Create source file via VFS
        root.join("copysrc").unwrap().create_file().unwrap();
        let src_fh = request
            .file_manager()
            .get_filehandle_for_path("copysrc".to_string())
            .await
            .unwrap();
        request.set_filehandle(src_fh);

        // Write data to source
        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"Hello, server-side COPY!".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        // Save source filehandle
        let mut request = response.request;
        let src = request.current_filehandle().cloned().unwrap();

        // Create destination file via VFS
        root.join("copydst").unwrap().create_file().unwrap();
        let dst_fh = request
            .file_manager()
            .get_filehandle_for_path("copydst".to_string())
            .await
            .unwrap();
        request.set_filehandle(dst_fh);
        request.set_saved_filehandle(src);

        // COPY from source to destination (count=0 means all)
        let copy_args = Copy4args {
            ca_src_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            ca_dst_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            ca_src_offset: 0,
            ca_dst_offset: 0,
            ca_count: 0,
            ca_consecutive: true,
            ca_synchronous: true,
            ca_source_server: vec![],
        };
        let response = copy_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        if let Some(NfsResOp4::Opcopy(Copy4res::Resok4(Copy4resok { cr_response, .. }))) =
            &response.result
        {
            assert_eq!(cr_response.wr_count, 24);
        } else {
            panic!("Expected Copy4resok");
        }

        // READ destination to verify
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
            assert_eq!(data, b"Hello, server-side COPY!");
        } else {
            panic!("Expected Read4resok");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_copy_partial() {
        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root = request.current_filehandle().unwrap().file.clone();

        // Create and write source
        root.join("partsrc").unwrap().create_file().unwrap();
        let src_fh = request
            .file_manager()
            .get_filehandle_for_path("partsrc".to_string())
            .await
            .unwrap();
        request.set_filehandle(src_fh);

        let write_args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"ABCDEFGHIJ".to_vec(),
        };
        let response = write_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        let mut request = response.request;
        let src = request.current_filehandle().cloned().unwrap();

        // Create destination
        root.join("partdst").unwrap().create_file().unwrap();
        let dst_fh = request
            .file_manager()
            .get_filehandle_for_path("partdst".to_string())
            .await
            .unwrap();
        request.set_filehandle(dst_fh);
        request.set_saved_filehandle(src);

        // COPY 5 bytes from source offset 3 → "DEFGH"
        let copy_args = Copy4args {
            ca_src_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            ca_dst_stateid: Stateid4 { seqid: 0, other: [0; 12] },
            ca_src_offset: 3,
            ca_dst_offset: 0,
            ca_count: 5,
            ca_consecutive: true,
            ca_synchronous: true,
            ca_source_server: vec![],
        };
        let response = copy_args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);

        if let Some(NfsResOp4::Opcopy(Copy4res::Resok4(Copy4resok { cr_response, .. }))) =
            &response.result
        {
            assert_eq!(cr_response.wr_count, 5);
        } else {
            panic!("Expected Copy4resok");
        }

        // Verify destination content
        let read_args = Read4args {
            stateid: Stateid4 { seqid: 0, other: [0; 12] },
            offset: 0,
            count: 100,
        };
        let response = read_args.execute(response.request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opread(Read4res::Resok4(Read4resok { data, .. }))) =
            &response.result
        {
            assert_eq!(data, b"DEFGH");
        } else {
            panic!("Expected Read4resok");
        }
    }
}
