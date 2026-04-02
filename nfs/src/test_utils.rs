use std::path::PathBuf;

use vfs::{MemoryFS, VfsPath};

use crate::server::clientmanager::ClientManagerHandle;
use crate::server::export_manager::ExportManagerHandle;
use crate::server::filemanager::FileManagerHandle;
use crate::server::nfs41::SessionManager;
use crate::server::request::NfsRequest;

use nextnfs_proto::nfs4_proto::{
    CbClient4, ClientAddr4, NfsClientId4, SetClientId4args,
};

/// Create an NfsRequest suitable for unit testing NFS operations.
///
/// Sets up a ClientManagerHandle, ExportManagerHandle, and an in-memory
/// FileManagerHandle so tests don't need a real filesystem.
pub async fn create_nfs40_server(_principal: Option<String>) -> NfsRequest<'static> {
    let client_manager = ClientManagerHandle::new();
    let export_manager = ExportManagerHandle::new();

    // In-memory VFS — no real filesystem needed for protocol-level tests
    let vfs_root: VfsPath = VfsPath::new(MemoryFS::new());
    let file_manager = FileManagerHandle::new(vfs_root, Some(1), PathBuf::from("/tmp"));

    let boot_time = std::time::UNIX_EPOCH.elapsed().unwrap().as_secs();

    NfsRequest::new(
        "127.0.0.1:12345".to_string(),
        client_manager,
        export_manager,
        Some(file_manager),
        boot_time,
        None,
        Some(SessionManager::new()),
    )
}

/// Create an NfsRequest with the root filehandle already set.
/// This simulates the state after PUTROOTFH in single-export mode.
pub async fn create_nfs40_server_with_root_fh(_principal: Option<String>) -> NfsRequest<'static> {
    let mut request = create_nfs40_server(_principal).await;
    let root_fh = request.file_manager().get_root_filehandle().await.unwrap();
    request.set_filehandle(root_fh);
    request
}

/// Create a SetClientId4args for testing SETCLIENTID / SETCLIENTID_CONFIRM.
pub fn create_client(verifier: [u8; 8], id: String) -> SetClientId4args {
    SetClientId4args {
        client: NfsClientId4 { verifier, id },
        callback: CbClient4 {
            cb_program: 0x40000000,
            cb_location: ClientAddr4 {
                rnetid: "tcp".to_string(),
                raddr: "127.0.0.1.0.0".to_string(),
            },
        },
        callback_ident: 1,
    }
}
