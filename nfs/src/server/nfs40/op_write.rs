use std::io::{Seek, SeekFrom, Write};

use async_trait::async_trait;
use tracing::{debug, error};

use crate::server::{operation::NfsOperation, request::NfsRequest, response::NfsOpResponse};

use nextnfs_proto::nfs4_proto::{NfsResOp4, NfsStat4, StableHow4, Write4args, Write4res, Write4resok};

fn verifier_from_boot(boot_time: &u64) -> [u8; 8] {
    let mut verifier = [0; 8];
    verifier.copy_from_slice(boot_time.to_be_bytes().as_ref());
    verifier
}

#[async_trait]
impl NfsOperation for Write4args {
    async fn execute<'a>(&self, mut request: NfsRequest<'a>) -> NfsOpResponse<'a> {
        debug!(
            "Operation 38: WRITE - Write to File {:?}, with request {:?}",
            self, request
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

        let mut stable = StableHow4::Unstable4;
        let mut count: u32 = self.data.len() as u32;
        if self.stable == StableHow4::Unstable4 {
            // write to cache
            let write_cache = match &filehandle.write_cache {
                Some(write_cache) => write_cache,
                None => {
                    let write_cache = match request
                        .file_manager()
                        .get_write_cache_handle(filehandle.clone())
                        .await
                    {
                        Ok(wc) => wc,
                        Err(e) => {
                            error!("WRITE: failed to get write cache: {:?}", e);
                            return NfsOpResponse {
                                request,
                                result: None,
                                status: NfsStat4::Nfs4errServerfault,
                            };
                        }
                    };
                    request.drop_filehandle_from_cache(filehandle.id);
                    &write_cache.clone()
                }
            };

            write_cache
                .write_bytes(self.offset, self.data.clone())
                .await;
        } else {
            // write to file
            let mut file = match filehandle.file.append_file() {
                Ok(f) => f,
                Err(e) => {
                    error!("WRITE: append_file failed: {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errIo,
                    };
                }
            };
            let _ = file.seek(SeekFrom::Start(self.offset));
            count = match file.write(&self.data) {
                Ok(n) => n as u32,
                Err(e) => {
                    error!("WRITE: write failed: {:?}", e);
                    return NfsOpResponse {
                        request,
                        result: None,
                        status: NfsStat4::Nfs4errIo,
                    };
                }
            };
            stable = StableHow4::FileSync4;

            if count > 0 {
                if let Err(e) = file.flush() {
                    error!("WRITE: flush failed: {:?}", e);
                }
                request.file_manager().touch_file(filehandle.id).await;
            }
        }

        let boot_time = request.boot_time;
        NfsOpResponse {
            request,
            result: Some(NfsResOp4::Opwrite(Write4res::Resok4(Write4resok {
                count,
                committed: stable,
                writeverf: verifier_from_boot(&boot_time),
            }))),
            status: NfsStat4::Nfs4Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::{
            nfs40::{NfsStat4, Stateid4, StableHow4, Write4args},
            operation::NfsOperation,
        },
        test_utils::create_nfs40_server,
    };
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_write_no_filehandle() {
        let request = create_nfs40_server(None).await;
        let args = Write4args {
            stateid: Stateid4 {
                seqid: 0,
                other: [0u8; 12],
            },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: vec![1, 2, 3, 4],
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4errFhexpired);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_write_file_sync() {
        use crate::server::nfs40::{NfsResOp4, Write4res};
        use crate::test_utils::create_nfs40_server_with_root_fh;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("wtest.txt").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .get_filehandle_for_path("wtest.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        let args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"test data".to_vec(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opwrite(Write4res::Resok4(resok))) = response.result {
            assert_eq!(resok.count, 9);
            assert_eq!(resok.committed, StableHow4::FileSync4);
            assert_ne!(resok.writeverf, [0u8; 8]);
        } else {
            panic!("Expected Write4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_write_unstable() {
        use crate::server::nfs40::{NfsResOp4, Write4res};
        use crate::test_utils::create_nfs40_server_with_root_fh;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("unstable.txt").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .create_file(
                root_file.join("unstable.txt").unwrap(),
                1, b"o".to_vec(), 1, 0, None,
            )
            .await
            .unwrap();
        request.set_filehandle(fh);

        let args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::Unstable4,
            data: b"cached write".to_vec(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opwrite(Write4res::Resok4(resok))) = response.result {
            assert_eq!(resok.count, 12);
            assert_eq!(resok.committed, StableHow4::Unstable4);
        } else {
            panic!("Expected Write4res::Resok4");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_write_verifier_from_boot_time() {
        use crate::server::nfs40::{NfsResOp4, Write4res};
        use crate::test_utils::create_nfs40_server_with_root_fh;

        let mut request = create_nfs40_server_with_root_fh(None).await;
        let boot_time = request.boot_time;
        let root_file = request.current_filehandle().unwrap().file.clone();
        root_file.join("verf.txt").unwrap().create_file().unwrap();
        let fh = request
            .file_manager()
            .get_filehandle_for_path("verf.txt".to_string())
            .await
            .unwrap();
        request.set_filehandle(fh);

        let args = Write4args {
            stateid: Stateid4 { seqid: 0, other: [0u8; 12] },
            offset: 0,
            stable: StableHow4::FileSync4,
            data: b"x".to_vec(),
        };
        let response = args.execute(request).await;
        assert_eq!(response.status, NfsStat4::Nfs4Ok);
        if let Some(NfsResOp4::Opwrite(Write4res::Resok4(resok))) = response.result {
            let expected_verf = boot_time.to_be_bytes();
            assert_eq!(resok.writeverf, expected_verf);
        } else {
            panic!("Expected Write4res::Resok4");
        }
    }
}
