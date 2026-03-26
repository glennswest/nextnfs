use multi_index_map::MultiIndexMap;
use rand::distributions::Uniform;
use rand::Rng;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, debug};

use nextnfs_proto::nfs4_proto::NfsStat4;

type ClientDb = MultiIndexClientEntryMap;

#[derive(Debug)]
pub struct ClientManager {
    receiver: mpsc::Receiver<ClientManagerMessage>,
    db: Arc<ClientDb>,
    client_id_seq: u64,
    filehandles: HashMap<String, Vec<u8>>,
    /// NFSv4 lease time in seconds (default 90)
    lease_time: u64,
}

#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub struct ClientCallback {
    pub program: u32,
    pub rnetid: String,
    pub raddr: String,
    pub callback_ident: u32,
}

/// Please read: [RFC 7530, Section 16.33.5](https://datatracker.ietf.org/doc/html/rfc7530#section-16.33.5)
#[derive(MultiIndexMap, Debug, Clone)]
#[multi_index_derive(Debug, Clone)]
pub struct ClientEntry {
    /// Please read: [RFC 7530, Section 3.3.3](https://datatracker.ietf.org/doc/html/rfc7530#section-3.3.3)
    #[multi_index(hashed_non_unique)]
    pub principal: Option<String>,
    #[multi_index(hashed_non_unique)]
    pub verifier: [u8; 8],
    #[multi_index(hashed_non_unique)]
    pub id: String,
    #[multi_index(hashed_non_unique)]
    pub clientid: u64,
    pub callback: ClientCallback,
    #[multi_index(hashed_unique)]
    pub setclientid_confirm: [u8; 8],
    pub confirmed: bool,
    /// Last time this client's lease was renewed (RENEW, OPEN, CLOSE, etc.)
    pub last_renewed: Instant,
    /// Whether the client's lease has expired but state is preserved (courteous server)
    pub courtesy: bool,
}

struct UpsertClientRequest {
    pub verifier: [u8; 8],
    pub id: String,
    pub callback: ClientCallback,
    pub principal: Option<String>,
    pub respond_to: oneshot::Sender<Result<ClientEntry, ClientManagerError>>,
}

struct ConfirmClientRequest {
    pub client_id: u64,
    pub setclientid_confirm: [u8; 8],
    pub principal: Option<String>,
    pub respond_to: oneshot::Sender<Result<ClientEntry, ClientManagerError>>,
}

struct RenewLeasesRequest {
    pub client_id: u64,
    pub respond_to: oneshot::Sender<Result<(), ClientManagerError>>,
}

struct SweepLeasesRequest {
    pub respond_to: oneshot::Sender<Vec<u64>>,
}

struct IsCourtesyClientRequest {
    pub client_id: u64,
    pub respond_to: oneshot::Sender<bool>,
}

struct RevokeCourtesyClientRequest {
    pub client_id: u64,
    pub respond_to: oneshot::Sender<()>,
}

enum ClientManagerMessage {
    UpsertClient(UpsertClientRequest),
    ConfirmClient(ConfirmClientRequest),
    SetCurrentFilehandle(SetCurrentFilehandleRequest),
    RenewLeases(RenewLeasesRequest),
    SweepLeases(SweepLeasesRequest),
    IsCourtesyClient(IsCourtesyClientRequest),
    RevokeCourtesyClient(RevokeCourtesyClientRequest),
}

pub struct SetCurrentFilehandleRequest {
    pub client_addr: String,
    pub filehandle_id: Vec<u8>,
}

impl ClientManager {
    fn new(receiver: mpsc::Receiver<ClientManagerMessage>) -> Self {
        ClientManager {
            receiver,
            db: ClientDb::default().into(),
            client_id_seq: 0,
            filehandles: HashMap::new(),
            lease_time: 90,
        }
    }

    fn handle_message(&mut self, msg: ClientManagerMessage) {
        // act on incoming messages
        match msg {
            ClientManagerMessage::ConfirmClient(request) => {
                let result = self.confirm_client(
                    request.client_id,
                    request.setclientid_confirm,
                    request.principal,
                );
                let _ = request.respond_to.send(result);
            }
            ClientManagerMessage::UpsertClient(request) => {
                let result = self.upsert_client(
                    request.verifier,
                    request.id,
                    request.callback,
                    request.principal,
                );
                let _ = request.respond_to.send(result);
            }
            ClientManagerMessage::SetCurrentFilehandle(request) => {
                self.set_current_fh(request.client_addr, request.filehandle_id);
            }
            ClientManagerMessage::RenewLeases(request) => {
                let result = self.renew_leases(request.client_id);
                let _ = request.respond_to.send(result);
            }
            ClientManagerMessage::SweepLeases(request) => {
                let expired = self.sweep_leases();
                let _ = request.respond_to.send(expired);
            }
            ClientManagerMessage::IsCourtesyClient(request) => {
                let is_courtesy = self.is_courtesy_client(request.client_id);
                let _ = request.respond_to.send(is_courtesy);
            }
            ClientManagerMessage::RevokeCourtesyClient(request) => {
                self.revoke_courtesy_client(request.client_id);
                let _ = request.respond_to.send(());
            }
        }
    }

    fn get_next_client_id(&mut self) -> u64 {
        self.client_id_seq += 1;
        self.client_id_seq
    }

    fn set_current_fh(&mut self, client_addr: String, filehandle: Vec<u8>) {
        self.filehandles.insert(client_addr, filehandle);
    }

    fn upsert_client(
        &mut self,
        verifier: [u8; 8],
        id: String,
        callback: ClientCallback,
        principal: Option<String>,
    ) -> Result<ClientEntry, ClientManagerError> {
        let db = Arc::get_mut(&mut self.db).unwrap();
        let entries = db.get_by_id(&id);
        let mut existing_clientid: Option<u64> = None;
        if !entries.is_empty() {
            // this is an update attempt
            let mut entries_to_remove = Vec::new();
            for entry in entries.clone() {
                if entry.confirmed && entry.principal != principal {
                    // For any confirmed record with the same id string x, if the recorded principal does
                    // not match that of the SETCLIENTID call, then the server returns an
                    // NFS4ERR_CLID_INUSE error.
                    return Err(ClientManagerError {
                        nfs_error: NfsStat4::Nfs4errClidInuse,
                    });
                }
                if !entry.confirmed {
                    entries_to_remove.push(entry.clone());
                }
                existing_clientid = Some(entry.clientid);
            }

            entries_to_remove.iter().for_each(|entry| {
                db.remove_by_setclientid_confirm(&entry.setclientid_confirm);
            });
        }

        Ok(self.add_client_record(verifier, id, callback, principal, existing_clientid))
    }

    fn add_client_record(
        &mut self,
        verifier: [u8; 8],
        id: String,
        callback: ClientCallback,
        principal: Option<String>,
        client_id: Option<u64>,
    ) -> ClientEntry {
        let client_id = client_id.unwrap_or_else(|| self.get_next_client_id());
        let mut rng = rand::thread_rng();
        // generate a random 8 byte array
        let setclientid_confirm_vec: Vec<u8> =
            (0..8).map(|_| rng.sample(Uniform::new(0, 255))).collect();
        let setclientid_confirm: [u8; 8] = setclientid_confirm_vec.try_into().unwrap();
        let client = ClientEntry {
            principal,
            verifier,
            id,
            clientid: client_id,
            callback,
            setclientid_confirm,
            confirmed: false,
            last_renewed: Instant::now(),
            courtesy: false,
        };

        let db = Arc::get_mut(&mut self.db).unwrap();
        db.insert(client.clone());
        client
    }

    fn confirm_client(
        &mut self,
        client_id: u64,
        setclientid_confirm: [u8; 8],
        principal: Option<String>,
    ) -> Result<ClientEntry, ClientManagerError> {
        let db = Arc::get_mut(&mut self.db).unwrap();

        let entries = db.get_by_clientid(&client_id);
        let mut old_confirmed: Option<ClientEntry> = None;
        let mut new_confirmed: Option<ClientEntry> = None;
        if entries.is_empty() {
            // nothing to confirm
            return Err(ClientManagerError {
                nfs_error: NfsStat4::Nfs4errStaleClientid,
            });
        }

        for entry in entries {
            if entry.principal != principal {
                // For any confirmed record with the same id string x, if the recorded principal does
                // not match that of the SETCLIENTID call, then the server returns an
                // NFS4ERR_CLID_INUSE error.
                return Err(ClientManagerError {
                    nfs_error: NfsStat4::Nfs4errClidInuse,
                });
            }
            if entry.confirmed && entry.setclientid_confirm != setclientid_confirm {
                old_confirmed = Some(entry.clone());
            }
            if entry.setclientid_confirm == setclientid_confirm {
                let mut update_entry = entry.clone();
                update_entry.confirmed = true;
                new_confirmed = Some(update_entry);
            }
        }

        if let Some(old_confirmed) = old_confirmed {
            db.remove_by_setclientid_confirm(&(old_confirmed.setclientid_confirm));
        }

        match new_confirmed {
            Some(new_confirmed) => {
                db.modify_by_setclientid_confirm(&new_confirmed.setclientid_confirm, |c| {
                    c.confirmed = true;
                });
                Ok(new_confirmed)
            }
            None => Err(ClientManagerError {
                nfs_error: NfsStat4::Nfs4errStaleClientid,
            }),
        }
    }

    fn renew_leases(&mut self, client_id: u64) -> Result<(), ClientManagerError> {
        let db = Arc::get_mut(&mut self.db).unwrap();
        let entries = db.get_by_clientid(&client_id);
        if entries.is_empty() {
            return Err(ClientManagerError {
                nfs_error: NfsStat4::Nfs4errStaleClientid,
            });
        }

        // Check if any confirmed entry has its lease expired beyond courtesy threshold
        let now = Instant::now();
        for entry in entries.clone() {
            if entry.confirmed && entry.courtesy {
                let elapsed = now.duration_since(entry.last_renewed).as_secs();
                // Courteous: allow renewal if within 2x lease_time (grace window)
                if elapsed > self.lease_time * 2 {
                    debug!(
                        clientid = client_id,
                        elapsed_secs = elapsed,
                        "client exceeded courtesy window, rejecting renewal"
                    );
                    return Err(ClientManagerError {
                        nfs_error: NfsStat4::Nfs4errExpired,
                    });
                }
            }
        }

        // Renew: update last_renewed and clear courtesy flag
        db.modify_by_clientid(&client_id, |c| {
            c.last_renewed = Instant::now();
            c.courtesy = false;
        });

        Ok(())
    }

    /// Check all clients for expired leases and mark them as courtesy clients.
    /// Called periodically by the background sweep task.
    fn sweep_leases(&mut self) -> Vec<u64> {
        let db = Arc::get_mut(&mut self.db).unwrap();
        let now = Instant::now();
        let lease_time = self.lease_time;
        let mut courtesy_clients = Vec::new();
        let mut expired_clients = Vec::new();

        // Collect all confirmed client IDs
        let all_entries: Vec<ClientEntry> = db.iter().map(|(_, e)| e.clone()).collect();

        for entry in &all_entries {
            if !entry.confirmed {
                continue;
            }
            let elapsed = now.duration_since(entry.last_renewed).as_secs();
            if elapsed >= lease_time && !entry.courtesy {
                // Mark as courtesy — don't purge yet
                courtesy_clients.push(entry.clientid);
            } else if entry.courtesy && elapsed >= lease_time * 2 {
                // Past courtesy window — purge
                expired_clients.push(entry.clientid);
            }
        }

        // Mark courtesy clients
        for cid in &courtesy_clients {
            db.modify_by_clientid(cid, |c| {
                c.courtesy = true;
            });
            debug!(clientid = cid, "client lease expired, entering courtesy state");
        }

        // Purge clients past the courtesy window
        for cid in &expired_clients {
            db.remove_by_clientid(cid);
            debug!(clientid = cid, "client removed after courtesy window expired");
        }

        expired_clients
    }

    /// Check if a client is in courtesy state (lease expired but state preserved).
    /// Returns true if the client is a courtesy client — callers with conflicting
    /// requests may force-revoke.
    fn is_courtesy_client(&mut self, client_id: u64) -> bool {
        let db = Arc::get_mut(&mut self.db).unwrap();
        let entries = db.get_by_clientid(&client_id);
        entries.iter().any(|e| e.courtesy)
    }

    /// Force-revoke a courtesy client (when there's a conflicting lock request).
    fn revoke_courtesy_client(&mut self, client_id: u64) {
        let db = Arc::get_mut(&mut self.db).unwrap();
        db.remove_by_clientid(&client_id);
        debug!(clientid = client_id, "courtesy client forcibly revoked due to conflict");
    }

    pub fn get_record_count(&mut self) -> usize {
        let db = Arc::get_mut(&mut self.db).unwrap();
        db.len()
    }

    pub fn remove_client(&mut self, client_id: u64) {
        let db = Arc::get_mut(&mut self.db).unwrap();
        db.remove_by_clientid(&client_id);
    }

    pub fn get_client_confirmed(&mut self, clientid: u64) -> Option<&ClientEntry> {
        let db = Arc::get_mut(&mut self.db).unwrap();
        let records = db.get_by_clientid(&clientid);
        let _match = records.iter().find(|r| r.confirmed);
        match _match {
            Some(record) => Some(record),
            None => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientManagerError {
    pub nfs_error: NfsStat4,
}

impl fmt::Display for ClientManagerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ClientManagerError: {:?}", self.nfs_error)
    }
}

#[derive(Debug, Clone)]
pub struct ClientManagerHandle {
    sender: mpsc::Sender<ClientManagerMessage>,
}

impl Default for ClientManagerHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientManagerHandle {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(16);
        let cmanager = ClientManager::new(receiver);
        // start the client manager actor
        tokio::spawn(run_client_manager(cmanager));

        Self { sender }
    }

    pub async fn set_current_filehandle(&self, client_addr: String, filehandle_id: Vec<u8>) {
        let resp = self
            .sender
            .send(ClientManagerMessage::SetCurrentFilehandle(
                SetCurrentFilehandleRequest {
                    client_addr,
                    filehandle_id,
                },
            ))
            .await;
        match resp {
            Ok(_) => {}
            Err(e) => {
                error!("Couldn't set filehandle: {:?}", e);
            }
        }
    }

    pub async fn upsert_client(
        &self,
        verifier: [u8; 8],
        id: String,
        callback: ClientCallback,
        principal: Option<String>,
    ) -> Result<ClientEntry, ClientManagerError> {
        let (tx, rx) = oneshot::channel();
        let resp = self
            .sender
            .send(ClientManagerMessage::UpsertClient(UpsertClientRequest {
                verifier,
                id,
                callback,
                principal,
                respond_to: tx,
            }))
            .await;
        match resp {
            Ok(_) => match rx.await {
                Ok(result) => result,
                Err(_) => {
                    error!("client manager actor died before responding to upsert");
                    Err(ClientManagerError {
                        nfs_error: NfsStat4::Nfs4errServerfault,
                    })
                }
            },
            Err(e) => {
                error!("Couldn't upsert client: {:?}", e);
                Err(ClientManagerError {
                    nfs_error: NfsStat4::Nfs4errServerfault,
                })
            }
        }
    }

    pub async fn confirm_client(
        &self,
        client_id: u64,
        setclientid_confirm: [u8; 8],
        principal: Option<String>,
    ) -> Result<ClientEntry, ClientManagerError> {
        let (tx, rx) = oneshot::channel();
        let resp = self
            .sender
            .send(ClientManagerMessage::ConfirmClient(ConfirmClientRequest {
                client_id,
                setclientid_confirm,
                principal,
                respond_to: tx,
            }))
            .await;
        match resp {
            Ok(_) => match rx.await {
                Ok(result) => result,
                Err(_) => {
                    error!("client manager actor died before responding to confirm");
                    Err(ClientManagerError {
                        nfs_error: NfsStat4::Nfs4errServerfault,
                    })
                }
            },
            Err(e) => {
                error!("Couldn't confirm client: {:?}", e);
                Err(ClientManagerError {
                    nfs_error: NfsStat4::Nfs4errServerfault,
                })
            }
        }
    }

    pub async fn renew_leases(&self, client_id: u64) -> Result<(), ClientManagerError> {
        let (tx, rx) = oneshot::channel();
        let resp = self
            .sender
            .send(ClientManagerMessage::RenewLeases(RenewLeasesRequest {
                client_id,
                respond_to: tx,
            }))
            .await;
        match resp {
            Ok(_) => match rx.await {
                Ok(result) => result,
                Err(_) => {
                    error!("client manager actor died before responding to renew");
                    Err(ClientManagerError {
                        nfs_error: NfsStat4::Nfs4errServerfault,
                    })
                }
            },
            Err(e) => {
                error!("Couldn't renew leases: {:?}", e);
                Err(ClientManagerError {
                    nfs_error: NfsStat4::Nfs4errServerfault,
                })
            }
        }
    }

    /// Run a lease sweep — marks expired leases as courtesy, purges past-courtesy clients.
    /// Returns list of client IDs that were fully purged.
    pub async fn sweep_leases(&self) -> Vec<u64> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .send(ClientManagerMessage::SweepLeases(SweepLeasesRequest {
                respond_to: tx,
            }))
            .await;
        rx.await.unwrap_or_default()
    }

    /// Check if a client is in courtesy state.
    pub async fn is_courtesy_client(&self, client_id: u64) -> bool {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .send(ClientManagerMessage::IsCourtesyClient(
                IsCourtesyClientRequest {
                    client_id,
                    respond_to: tx,
                },
            ))
            .await;
        rx.await.unwrap_or(false)
    }

    /// Force-revoke a courtesy client due to conflicting lock request.
    pub async fn revoke_courtesy_client(&self, client_id: u64) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .send(ClientManagerMessage::RevokeCourtesyClient(
                RevokeCourtesyClientRequest {
                    client_id,
                    respond_to: tx,
                },
            ))
            .await;
        let _ = rx.await;
    }

    /// Start a background lease sweep task that runs every 30 seconds.
    pub fn start_lease_sweeper(handle: ClientManagerHandle) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                let expired = handle.sweep_leases().await;
                if !expired.is_empty() {
                    debug!(count = expired.len(), "lease sweep purged expired courtesy clients");
                }
            }
        });
    }
}

/// ClientManager is run as with the actor pattern
///
/// Learn more: https://ryhl.io/blog/actors-with-tokio/
async fn run_client_manager(mut actor: ClientManager) {
    while let Some(msg) = actor.receiver.recv().await {
        actor.handle_message(msg);
    }
}

#[cfg(test)]
mod tests {

    use tokio::sync::mpsc;

    use nextnfs_proto::nfs4_proto::NfsStat4;

    #[test]
    fn test_upsert_clients_no_principals() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);

        let verifier = [0; 8];
        let id = "test".to_string();
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = manager
            .upsert_client(verifier, id.clone(), callback.clone(), None)
            .unwrap();
        assert_eq!(client.id, id);
        assert_eq!(client.verifier, verifier);
        assert_eq!(client.callback, callback);

        let updated_callback = super::ClientCallback {
            program: 10,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 2,
        };

        let same_client = manager
            .upsert_client(verifier, id.clone(), updated_callback.clone(), None)
            .unwrap();
        assert_eq!(same_client.id, id);
        assert_eq!(same_client.verifier, verifier);
        assert_eq!(same_client.callback, updated_callback);
        assert_eq!(same_client.clientid, client.clientid);

        // confirm after update
        let err_confirm = manager.confirm_client(client.clientid, client.setclientid_confirm, None);
        assert_eq!(
            err_confirm.unwrap_err().nfs_error,
            NfsStat4::Nfs4errStaleClientid
        );

        let confirmed_client = manager
            .confirm_client(client.clientid, same_client.setclientid_confirm, None)
            .unwrap();
        assert!(confirmed_client.confirmed);
        assert_eq!(confirmed_client.clientid, client.clientid);

        let other_callback = super::ClientCallback {
            program: 1,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };
        let err_client = manager.upsert_client(
            verifier,
            id,
            other_callback.clone(),
            Some("LINUX".to_string()),
        );
        assert_eq!(
            err_client.unwrap_err().nfs_error,
            NfsStat4::Nfs4errClidInuse
        );

        let stale_client = manager.confirm_client(1234, client.setclientid_confirm, None);
        assert_eq!(
            stale_client.unwrap_err().nfs_error,
            NfsStat4::Nfs4errStaleClientid
        );

        let confirmed = manager.get_client_confirmed(client.clientid);
        assert_eq!(confirmed.unwrap().clientid, client.clientid);
        assert!(confirmed.unwrap().confirmed);

        let c = manager.get_record_count();
        assert_eq!(c, 1);
        manager.remove_client(client.clientid);
        let c = manager.get_record_count();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_upsert_clients_double_confirm() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);

        let verifier = [0; 8];
        let id = "test".to_string();
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = manager
            .upsert_client(verifier, id.clone(), callback.clone(), None)
            .unwrap();

        let confirmed_client = manager
            .confirm_client(client.clientid, client.setclientid_confirm, None)
            .unwrap();
        assert!(confirmed_client.confirmed);
        assert_eq!(confirmed_client.clientid, client.clientid);
        let confirmed_client = manager
            .confirm_client(client.clientid, client.setclientid_confirm, None)
            .unwrap();
        assert!(confirmed_client.confirmed);
        assert_eq!(confirmed_client.clientid, client.clientid);
    }

    #[test]
    fn test_renew_leases_valid_client() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);

        let verifier = [0; 8];
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = manager
            .upsert_client(verifier, "renew_test".to_string(), callback, None)
            .unwrap();
        let result = manager.renew_leases(client.clientid);
        assert!(result.is_ok());
    }

    #[test]
    fn test_renew_leases_stale_client() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        let result = manager.renew_leases(99999);
        assert_eq!(
            result.unwrap_err().nfs_error,
            NfsStat4::Nfs4errStaleClientid
        );
    }

    #[test]
    fn test_set_current_filehandle() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        manager.set_current_fh("192.168.1.1:1234".to_string(), vec![1, 2, 3]);
        // Verify it was stored (no panic)
        manager.set_current_fh("192.168.1.1:1234".to_string(), vec![4, 5, 6]);
    }

    #[test]
    fn test_sweep_no_clients() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        let expired = manager.sweep_leases();
        assert!(expired.is_empty());
    }

    #[test]
    fn test_sweep_fresh_client_not_expired() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };
        let client = manager
            .upsert_client([0; 8], "fresh".to_string(), callback, None)
            .unwrap();
        manager
            .confirm_client(client.clientid, client.setclientid_confirm, None)
            .unwrap();
        let expired = manager.sweep_leases();
        assert!(expired.is_empty());
        // Client should not be in courtesy state
        assert!(!manager.is_courtesy_client(client.clientid));
    }

    #[test]
    fn test_courtesy_client_flag() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        // Set lease time to 0 so it expires immediately
        manager.lease_time = 0;
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };
        let client = manager
            .upsert_client([0; 8], "courtesy".to_string(), callback, None)
            .unwrap();
        manager
            .confirm_client(client.clientid, client.setclientid_confirm, None)
            .unwrap();
        // First sweep marks as courtesy
        let expired = manager.sweep_leases();
        assert!(expired.is_empty()); // not purged yet
        assert!(manager.is_courtesy_client(client.clientid));
    }

    #[test]
    fn test_revoke_courtesy_client() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        manager.lease_time = 0;
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };
        let client = manager
            .upsert_client([0; 8], "revoke".to_string(), callback, None)
            .unwrap();
        manager
            .confirm_client(client.clientid, client.setclientid_confirm, None)
            .unwrap();
        manager.sweep_leases(); // mark courtesy
        assert!(manager.is_courtesy_client(client.clientid));
        manager.revoke_courtesy_client(client.clientid);
        assert_eq!(manager.get_record_count(), 0);
    }

    #[test]
    fn test_renew_clears_courtesy() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        manager.lease_time = 0;
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };
        let client = manager
            .upsert_client([0; 8], "renew_courtesy".to_string(), callback, None)
            .unwrap();
        manager
            .confirm_client(client.clientid, client.setclientid_confirm, None)
            .unwrap();
        // Make courtesy
        manager.sweep_leases();
        assert!(manager.is_courtesy_client(client.clientid));
        // Renew should clear courtesy and succeed
        manager.lease_time = 90; // restore normal lease time
        let result = manager.renew_leases(client.clientid);
        assert!(result.is_ok());
        assert!(!manager.is_courtesy_client(client.clientid));
    }

    #[test]
    fn test_multiple_clients_unique_ids() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let c1 = manager
            .upsert_client([1; 8], "client1".to_string(), callback.clone(), None)
            .unwrap();
        let c2 = manager
            .upsert_client([2; 8], "client2".to_string(), callback.clone(), None)
            .unwrap();
        assert_ne!(c1.clientid, c2.clientid);
        assert_eq!(manager.get_record_count(), 2);
    }

    #[test]
    fn test_confirm_wrong_principal() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = manager
            .upsert_client([0; 8], "test".to_string(), callback, Some("Linux".to_string()))
            .unwrap();

        // Confirm with wrong principal
        let result = manager.confirm_client(
            client.clientid,
            client.setclientid_confirm,
            Some("FreeBSD".to_string()),
        );
        assert_eq!(result.unwrap_err().nfs_error, NfsStat4::Nfs4errClidInuse);
    }

    #[test]
    fn test_get_client_confirmed_not_found() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        assert!(manager.get_client_confirmed(9999).is_none());
    }

    #[test]
    fn test_get_client_unconfirmed_not_returned() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = manager
            .upsert_client([0; 8], "unconf".to_string(), callback, None)
            .unwrap();
        // Not confirmed yet
        assert!(manager.get_client_confirmed(client.clientid).is_none());
    }

    // ── Async handle tests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_handle_upsert_and_confirm() {
        let handle = super::ClientManagerHandle::new();
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = handle
            .upsert_client([0; 8], "async_test".to_string(), callback, None)
            .await
            .unwrap();
        assert!(!client.confirmed);

        let confirmed = handle
            .confirm_client(client.clientid, client.setclientid_confirm, None)
            .await
            .unwrap();
        assert!(confirmed.confirmed);
    }

    #[tokio::test]
    async fn test_handle_renew_leases() {
        let handle = super::ClientManagerHandle::new();
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = handle
            .upsert_client([0; 8], "renew_async".to_string(), callback, None)
            .await
            .unwrap();
        let result = handle.renew_leases(client.clientid).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_renew_stale() {
        let handle = super::ClientManagerHandle::new();
        let result = handle.renew_leases(999999).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().nfs_error, NfsStat4::Nfs4errStaleClientid);
    }

    #[tokio::test]
    async fn test_handle_set_current_filehandle() {
        let handle = super::ClientManagerHandle::new();
        // Should not panic
        handle
            .set_current_filehandle("10.0.0.1:2049".to_string(), vec![0xAA; 26])
            .await;
    }

    #[test]
    fn test_upsert_clients_principals() {
        let (_, receiver) = mpsc::channel(16);
        let mut manager = super::ClientManager::new(receiver);

        let verifier = [0; 8];
        let id = "test".to_string();
        let callback = super::ClientCallback {
            program: 0,
            rnetid: "tcp".to_string(),
            raddr: "".to_string(),
            callback_ident: 0,
        };

        let client = manager
            .upsert_client(
                verifier,
                id.clone(),
                callback.clone(),
                Some("Linux".to_string()),
            )
            .unwrap();

        let same_client = manager
            .confirm_client(
                client.clientid,
                client.setclientid_confirm,
                Some("Linux".to_string()),
            )
            .unwrap();

        assert_eq!(same_client.id, id);
        assert_eq!(same_client.verifier, verifier);
        assert_eq!(same_client.callback, callback);
        assert_eq!(same_client.clientid, client.clientid);
        assert_eq!(same_client.principal, Some("Linux".to_string()));
        assert!(same_client.confirmed);
    }
}
