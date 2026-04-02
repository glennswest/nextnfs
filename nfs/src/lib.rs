pub mod server;

#[cfg(test)]
pub mod test_utils;

use std::collections::HashMap;
use std::net::SocketAddr;

use nextnfs_proto::rpc_proto::{AcceptBody, AcceptedReply, OpaqueAuth, ReplyBody};
use nextnfs_proto::XDRProtoCodec;
use futures::SinkExt;
use server::clientmanager::ClientManagerHandle;
use server::export_manager::ExportManagerHandle;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpListener;
use tokio_stream::StreamExt;
use tokio_util::codec::Framed;
use tracing::{error, info, span, warn, Level};
pub use vfs;
pub use vfs::VfsPath;

use crate::server::request::NfsRequest;
use crate::server::{NFSService, NfsProtoImpl};

/// Re-export for the binary crate.
pub use server::export_manager;
pub use server::state_recovery;

pub struct NFSServer {
    bind: String,
    export_manager: ExportManagerHandle,
    service_0: Option<server::nfs40::NFS40Server>,
    boot_time: u64,
    session_manager: server::nfs41::SessionManager,
    /// State directory for near-zero grace period recovery
    state_dir: Option<std::path::PathBuf>,
}

impl NFSServer {
    pub fn builder() -> ServerBuilder {
        ServerBuilder::new()
    }

    pub fn export_manager(&self) -> &ExportManagerHandle {
        &self.export_manager
    }

    pub async fn start_async(&self) {
        self.serve().await;
    }

    async fn serve(&self) {
        let sock = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("failed to create socket");

        sock.set_reuse_address(true).unwrap();
        sock.set_nonblocking(true).unwrap();

        // Large socket buffers for high throughput
        let _ = sock.set_send_buffer_size(4 * 1024 * 1024);
        let _ = sock.set_recv_buffer_size(4 * 1024 * 1024);

        let addr: SocketAddr = self.bind.parse().expect("invalid bind address");
        sock.bind(&addr.into()).expect("failed to bind");
        sock.listen(1024).expect("failed to listen");

        let listener = TcpListener::from_std(sock.into()).expect("failed to convert to tokio");
        info!(%self.bind, "nextnfs NFSv4 server listening");

        let client_manager_handle = ClientManagerHandle::new();
        let nfs40 = self.service_0.as_ref().unwrap();

        // Near-zero grace period: restore client state from snapshot
        if let Some(ref state_dir) = self.state_dir {
            let recovery = server::state_recovery::StateRecovery::new(state_dir);
            match recovery.load() {
                Ok(snapshot) => {
                    let count = client_manager_handle
                        .restore_clients(snapshot.clients)
                        .await;
                    info!(
                        clients_restored = count,
                        "state recovery complete — grace period skipped"
                    );
                    recovery.clear();
                    // No grace period needed — state was recovered
                }
                Err(e) => {
                    info!(reason = %e, "no state recovery — entering grace period");
                    nfs40.in_grace.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        // Start grace period expiry timer (90s = lease_time)
        {
            let grace_flag = nfs40.in_grace.clone();
            if grace_flag.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(90)).await;
                    grace_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                    info!("grace period expired — normal operations resumed");
                });
            }
        }

        // Start background lease sweep for courteous server behavior
        ClientManagerHandle::start_lease_sweeper(client_manager_handle.clone());

        // Start periodic state save task (every 30s) for near-zero grace period
        if let Some(ref state_dir) = self.state_dir {
            let cm_save = client_manager_handle.clone();
            let save_dir = state_dir.clone();
            let boot_time = self.boot_time;
            tokio::spawn(async move {
                let recovery = server::state_recovery::StateRecovery::new(&save_dir);
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    let clients = cm_save.get_all_clients().await;
                    let mut snapshot =
                        server::state_recovery::StateSnapshot::new(boot_time);
                    snapshot.clients = clients
                        .into_iter()
                        .map(|c| server::state_recovery::ClientSnapshot {
                            principal: c.principal,
                            verifier: c.verifier,
                            id: c.id,
                            clientid: c.clientid,
                            callback_program: c.callback.program,
                            callback_rnetid: c.callback.rnetid,
                            callback_raddr: c.callback.raddr,
                            callback_ident: c.callback.callback_ident,
                            confirmed: c.confirmed,
                        })
                        .collect();
                    if let Err(e) = recovery.save(&snapshot) {
                        warn!(error = %e, "failed to save state snapshot");
                    }
                }
            });
        }

        let export_manager = self.export_manager.clone();

        // Pre-resolve default file manager (first export) for the accept loop
        let default_fm = {
            let exports = export_manager.list_exports().await;
            if let Some(first) = exports.first() {
                export_manager
                    .get_export_by_id(first.export_id)
                    .await
                    .map(|(_, fm)| fm)
            } else {
                None
            }
        };

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    // TCP tuning for performance
                    let sock_ref = SockRef::from(&stream);
                    let _ = sock_ref.set_nodelay(true);
                    let _ = sock_ref.set_send_buffer_size(4 * 1024 * 1024);
                    let _ = sock_ref.set_recv_buffer_size(4 * 1024 * 1024);
                    let keepalive = TcpKeepalive::new()
                        .with_time(std::time::Duration::from_secs(60))
                        .with_interval(std::time::Duration::from_secs(15));
                    let _ = sock_ref.set_tcp_keepalive(&keepalive);

                    info!(%addr, "NFS client connected");
                    let cm = client_manager_handle.clone();
                    let em = export_manager.clone();
                    let dfm = default_fm.clone();
                    let boot_time = self.boot_time;
                    let service_0 = self.service_0.clone();
                    let sm = self.session_manager.clone();

                    tokio::spawn(async move {
                        let span = span!(Level::DEBUG, "nfs_client", %addr);
                        let _enter = span.enter();
                        let mut nfs_transport = Framed::new(stream, XDRProtoCodec::new());
                        let mut filehandle_cache = HashMap::new();

                        loop {
                            let msg = nfs_transport.next().await;
                            match msg {
                                Some(Ok(msg)) => {
                                    // Handle parse errors delivered as ParseError variant
                                    // (codec never returns Err to avoid Framed stream termination)
                                    if let nextnfs_proto::rpc_proto::MsgType::ParseError(ref reason) = msg.body {
                                        warn!(%addr, xid = msg.xid, %reason, "RPC parse error");
                                        let resp = Box::new(nextnfs_proto::rpc_proto::RpcReplyMsg {
                                            xid: msg.xid,
                                            body: nextnfs_proto::rpc_proto::MsgType::Reply(
                                                ReplyBody::MsgAccepted(AcceptedReply {
                                                    verf: OpaqueAuth::AuthNull(Vec::<u8>::new()),
                                                    reply_data: AcceptBody::GarbageArgs,
                                                }),
                                            ),
                                        });
                                        match nfs_transport.send(resp).await {
                                            Ok(_) => {}
                                            Err(e) => {
                                                error!(%addr, "couldn't send GarbageArgs: {:?}", e);
                                                break;
                                            }
                                        }
                                        continue;
                                    }

                                    let request = NfsRequest::new(
                                        addr.to_string(),
                                        cm.clone(),
                                        em.clone(),
                                        dfm.clone(),
                                        boot_time,
                                        Some(&mut filehandle_cache),
                                        Some(sm.clone()),
                                    );
                                    let nfs_protocol = service_0.as_ref().unwrap();
                                    let service = NFSService::new(nfs_protocol.clone());

                                    let resp = service.call(msg, request).await;
                                    match nfs_transport.send(resp).await {
                                        Ok(_) => {}
                                        Err(e) => {
                                            error!(%addr, "couldn't send response: {:?}", e);
                                            break;
                                        }
                                    }
                                }
                                Some(Err(e)) => {
                                    // This should rarely happen now — codec wraps errors in ParseError
                                    error!(%addr, "codec error: {:?}", e);
                                    break;
                                }
                                None => {
                                    info!(%addr, "NFS client disconnected");
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(e) => error!("couldn't get client: {:?}", e),
            }
        }
    }
}

pub struct ServerBuilder {
    bind: String,
    export_manager: Option<ExportManagerHandle>,
    state_dir: Option<std::path::PathBuf>,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerBuilder {
    pub fn new() -> Self {
        ServerBuilder {
            bind: "0.0.0.0:2049".to_string(),
            export_manager: None,
            state_dir: None,
        }
    }

    pub fn bind(&mut self, bind: &str) -> &mut Self {
        self.bind = bind.to_string();
        self
    }

    pub fn export_manager(&mut self, em: ExportManagerHandle) -> &mut Self {
        self.export_manager = Some(em);
        self
    }

    pub fn state_dir(&mut self, dir: std::path::PathBuf) -> &mut Self {
        self.state_dir = Some(dir);
        self
    }

    pub fn build(&self) -> NFSServer {
        let boot_time = std::time::UNIX_EPOCH.elapsed().unwrap().as_secs();
        let session_manager = server::nfs41::SessionManager::new();
        NFSServer {
            bind: self.bind.clone(),
            export_manager: self
                .export_manager
                .clone()
                .expect("export_manager must be set before build()"),
            service_0: Some(server::nfs40::NFS40Server::new()),
            boot_time,
            session_manager,
            state_dir: self.state_dir.clone(),
        }
    }
}
