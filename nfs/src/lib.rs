pub mod server;

#[cfg(test)]
pub mod test_utils;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

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
    /// TLS acceptor for RPC-over-TLS (RFC 9289). None = plain TCP.
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
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
                    info!(reason = %e, "no state to recover — skipping grace period");
                    // No grace period: there are no clients to reclaim state,
                    // so blocking mutating ops would only cause unnecessary EIO.
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
                    let tls = self.tls_acceptor.clone();

                    tokio::spawn(async move {
                        let span = span!(Level::DEBUG, "nfs_client", %addr);
                        let _enter = span.enter();

                        let ctx = ConnectionContext { addr, cm, em, dfm, boot_time, service_0, sm };
                        if let Some(acceptor) = tls {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    info!(%addr, "TLS handshake complete");
                                    let nfs_transport = Framed::new(tls_stream, XDRProtoCodec::new());
                                    handle_connection(nfs_transport, ctx).await;
                                }
                                Err(e) => {
                                    error!(%addr, "TLS handshake failed: {:?}", e);
                                }
                            }
                        } else {
                            let nfs_transport = Framed::new(stream, XDRProtoCodec::new());
                            handle_connection(nfs_transport, ctx).await;
                        }
                    });
                }
                Err(e) => error!("couldn't get client: {:?}", e),
            }
        }
    }
}

/// Bundled context for a single NFS connection (avoids too-many-arguments).
struct ConnectionContext {
    addr: SocketAddr,
    cm: ClientManagerHandle,
    em: ExportManagerHandle,
    dfm: Option<server::filemanager::FileManagerHandle>,
    boot_time: u64,
    service_0: Option<server::nfs40::NFS40Server>,
    sm: server::nfs41::SessionManager,
}

/// Handle an NFS connection over any transport implementing AsyncRead + AsyncWrite.
async fn handle_connection<T>(
    mut nfs_transport: Framed<T, XDRProtoCodec>,
    ctx: ConnectionContext,
) where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ConnectionContext { addr, cm, em, dfm, boot_time, service_0, sm } = ctx;
    let mut filehandle_cache = HashMap::new();

    loop {
        let msg = nfs_transport.next().await;
        match msg {
            Some(Ok(msg)) => {
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
                error!(%addr, "codec error: {:?}", e);
                break;
            }
            None => {
                info!(%addr, "NFS client disconnected");
                break;
            }
        }
    }
}

pub struct ServerBuilder {
    bind: String,
    export_manager: Option<ExportManagerHandle>,
    state_dir: Option<std::path::PathBuf>,
    tls_cert: Option<std::path::PathBuf>,
    tls_key: Option<std::path::PathBuf>,
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
            tls_cert: None,
            tls_key: None,
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

    /// Set TLS certificate and private key paths for RPC-over-TLS (RFC 9289).
    pub fn tls(&mut self, cert: std::path::PathBuf, key: std::path::PathBuf) -> &mut Self {
        self.tls_cert = Some(cert);
        self.tls_key = Some(key);
        self
    }

    pub fn build(&self) -> NFSServer {
        let boot_time = std::time::UNIX_EPOCH.elapsed().unwrap().as_secs();
        let session_manager = server::nfs41::SessionManager::new();

        let tls_acceptor = match (&self.tls_cert, &self.tls_key) {
            (Some(cert_path), Some(key_path)) => {
                Some(build_tls_acceptor(cert_path, key_path))
            }
            _ => None,
        };

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
            tls_acceptor,
        }
    }
}

/// Build a TLS acceptor from PEM certificate and key files.
fn build_tls_acceptor(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> tokio_rustls::TlsAcceptor {
    use rustls::ServerConfig;
    use rustls_pemfile::{certs, private_key};
    use std::io::BufReader;

    let cert_file = std::fs::File::open(cert_path)
        .unwrap_or_else(|e| panic!("failed to open TLS cert {}: {}", cert_path.display(), e));
    let key_file = std::fs::File::open(key_path)
        .unwrap_or_else(|e| panic!("failed to open TLS key {}: {}", key_path.display(), e));

    let certs: Vec<_> = certs(&mut BufReader::new(cert_file))
        .filter_map(|c| c.ok())
        .collect();
    let key = private_key(&mut BufReader::new(key_file))
        .expect("failed to read TLS private key")
        .expect("no private key found in key file");

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("failed to build TLS config");

    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_builder_defaults() {
        let builder = ServerBuilder::new();
        assert_eq!(builder.bind, "0.0.0.0:2049");
        assert!(builder.tls_cert.is_none());
        assert!(builder.tls_key.is_none());
        assert!(builder.state_dir.is_none());
    }

    #[test]
    fn test_server_builder_tls_config() {
        let mut builder = ServerBuilder::new();
        builder.tls(
            std::path::PathBuf::from("/tmp/cert.pem"),
            std::path::PathBuf::from("/tmp/key.pem"),
        );
        assert_eq!(
            builder.tls_cert.as_ref().unwrap().to_str().unwrap(),
            "/tmp/cert.pem"
        );
        assert_eq!(
            builder.tls_key.as_ref().unwrap().to_str().unwrap(),
            "/tmp/key.pem"
        );
    }

    #[tokio::test]
    async fn test_tls_acceptor_from_generated_certs() {
        use std::io::Write;

        // Install crypto provider for rustls
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Generate self-signed cert and key using rcgen
        let subject_alt_names = vec!["localhost".to_string()];
        let cert_params = rcgen::CertificateParams::new(subject_alt_names).unwrap();
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = cert_params.self_signed(&key_pair).unwrap();

        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        // Write to temp files
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        std::fs::File::create(&cert_path)
            .unwrap()
            .write_all(cert_pem.as_bytes())
            .unwrap();
        std::fs::File::create(&key_path)
            .unwrap()
            .write_all(key_pem.as_bytes())
            .unwrap();

        // build_tls_acceptor should succeed
        let _acceptor = build_tls_acceptor(&cert_path, &key_path);
    }
}
