use std::io::{Read, Seek, SeekFrom};

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
