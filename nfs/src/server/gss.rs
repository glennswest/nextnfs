//! RPCSEC_GSS (RFC 2203) with the Kerberos V5 mechanism: context
//! creation (INIT), per-call verification (DATA), DESTROY, and the
//! krb5 / krb5i / krb5p services (#106).
//!
//! A call with an RPCSEC_GSS credential goes through [`GssServer::handle`]
//! before the NFS service sees it. INIT and DESTROY are answered here.
//! For DATA the header's MIC is checked, the sequence number is checked
//! against the context's window, the arguments are unwrapped (krb5i:
//! checksum, krb5p: decrypted), and the credential is replaced by the
//! principal's mapped identity, so permissions and squash work as for
//! AUTH_SYS. The reply then goes through [`GssServer::wrap_reply`]:
//! its verifier is the MIC of the sequence number and, for krb5i/krb5p,
//! the results are wrapped the same way.
//!
//! Contexts live in one table shared by all connections (a client may
//! use a context on any of its connections) and expire with the ticket.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nextnfs_krb5::{Acceptor, ContextKey, Keytab, Sender};
use nextnfs_proto::rpc_proto::{
    AcceptBody, AcceptedReply, AuthStat, AuthUnix, CallBody, MsgType, OpaqueAuth, RejectedReply, ReplyBody,
    RpcCallMsg, RpcReplyMsg, RpcSecGssCred, RPCSEC_GSS_CONTINUE_INIT, RPCSEC_GSS_DATA, RPCSEC_GSS_DESTROY,
    RPCSEC_GSS_INIT, RPC_GSS_SVC_INTEGRITY, RPC_GSS_SVC_NONE, RPC_GSS_SVC_PRIVACY,
};
use tracing::{debug, info, warn};

use super::export_manager::SecFlavor;

/// Sequence window announced to clients (RFC 2203 §5.2.3.1).
pub const SEQ_WINDOW: u32 = 128;
/// Sequence numbers at or above this end the context (RFC 2203 §5.3.3.1).
const MAXSEQ: u32 = 0x8000_0000;
/// Contexts kept at most; the oldest are dropped beyond this.
const MAX_CONTEXTS: usize = 16_384;

/// GSS major status codes (RFC 2744 §3.9.1, routine errors << 16).
const GSS_S_DEFECTIVE_TOKEN: u32 = 9 << 16;
const GSS_S_DEFECTIVE_CREDENTIAL: u32 = 10 << 16;
const GSS_S_CREDENTIALS_EXPIRED: u32 = 11 << 16;
const GSS_S_NO_CONTEXT: u32 = 8 << 16;
const GSS_S_FAILURE: u32 = 13 << 16;

/// The identity a principal's calls run as: `(uid, gid, groups)`, or
/// `None` for the export's anonymous user.
pub type Identity = Option<(u32, u32, Vec<u32>)>;

/// An explicit principal → identity entry (config `[[server.krb5_map]]`).
#[derive(Debug, Clone)]
pub struct PrincipalMapping {
    pub principal: String,
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

/// Maps Kerberos principals to the uid/gid permissions are checked for.
///
/// In order: an explicit mapping; for a principal of a local realm (one
/// the keytab has keys in), a machine credential (`nfs/`, `host/` or
/// `root/<host>`) is uid 0 — so the export's `squash` decides, as for
/// AUTH_SYS root — and a one-part name is looked up in `/etc/passwd`
/// (supplementary groups from `/etc/group`). Anything else is the
/// export's anonymous user.
#[derive(Debug, Clone)]
pub struct IdMap {
    explicit: HashMap<String, (u32, u32, Vec<u32>)>,
    local_realms: HashSet<String>,
    etc: PathBuf,
}

impl IdMap {
    pub fn new(mappings: &[PrincipalMapping], local_realms: impl IntoIterator<Item = String>, etc: &Path) -> Self {
        IdMap {
            explicit: mappings.iter().map(|m| (m.principal.clone(), (m.uid, m.gid, m.gids.clone()))).collect(),
            local_realms: local_realms.into_iter().collect(),
            etc: etc.to_path_buf(),
        }
    }

    pub fn identity(&self, principal: &str) -> Identity {
        if let Some((uid, gid, gids)) = self.explicit.get(principal) {
            return Some((*uid, *gid, gids.clone()));
        }
        let (name, realm) = principal.rsplit_once('@')?;
        if !self.local_realms.contains(realm) {
            return None;
        }
        let parts: Vec<&str> = name.split('/').collect();
        match parts.as_slice() {
            [service, _host] if ["nfs", "host", "root"].contains(service) => Some((0, 0, Vec::new())),
            [user] => self.passwd(user),
            _ => None,
        }
    }

    fn passwd(&self, user: &str) -> Identity {
        let passwd = std::fs::read_to_string(self.etc.join("passwd")).ok()?;
        let (uid, gid) = passwd.lines().find_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() >= 4 && f[0] == user {
                Some((f[2].parse().ok()?, f[3].parse().ok()?))
            } else {
                None
            }
        })?;
        let mut gids = Vec::new();
        if let Ok(group) = std::fs::read_to_string(self.etc.join("group")) {
            for line in group.lines() {
                let f: Vec<&str> = line.split(':').collect();
                if f.len() >= 4 && f[3].split(',').any(|m| m == user) {
                    if let Ok(g) = f[2].parse::<u32>() {
                        if g != gid && !gids.contains(&g) {
                            gids.push(g);
                        }
                    }
                }
            }
        }
        Some((uid, gid, gids))
    }
}

/// Sliding window of seen sequence numbers (RFC 2203 §5.3.3.1).
#[derive(Debug, Default)]
struct SeqWindow {
    started: bool,
    max: u32,
    /// Bit `i`: `max - i` has been seen.
    seen: u128,
}

impl SeqWindow {
    /// Records `seq`; false if it is a replay or below the window.
    fn accept(&mut self, seq: u32) -> bool {
        if !self.started {
            self.started = true;
            self.max = seq;
            self.seen = 1;
            return true;
        }
        if seq > self.max {
            let shift = seq - self.max;
            self.seen = if shift >= 128 { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.max = seq;
            return true;
        }
        let d = self.max - seq;
        if d >= SEQ_WINDOW || self.seen & (1u128 << d) != 0 {
            return false;
        }
        self.seen |= 1u128 << d;
        true
    }
}

struct GssContext {
    key: ContextKey,
    principal: String,
    identity: Identity,
    expires: i64,
    created: std::time::Instant,
    window: Mutex<SeqWindow>,
    send_seq: AtomicU64,
}

impl GssContext {
    fn mic(&self, data: &[u8]) -> Vec<u8> {
        let seq = self.send_seq.fetch_add(1, Ordering::Relaxed);
        self.key.get_mic(Sender::Acceptor, seq, data).unwrap_or_default()
    }
}

/// What to do with an RPCSEC_GSS call.
pub enum GssCall {
    /// Send this reply (INIT, DESTROY, errors).
    Reply(Box<RpcReplyMsg>),
    /// Send nothing: the sequence number is a replay or outside the window.
    Drop,
    /// Run the call (its credential now the mapped identity), then pass
    /// the reply through [`GssServer::wrap_reply`].
    Proceed(RpcCallMsg, GssReply),
}

/// How to protect the reply of a DATA call.
pub struct GssReply {
    ctx: Arc<GssContext>,
    seq: u32,
    service: u32,
    proc: u32,
    /// The call's security flavour (krb5, krb5i or krb5p).
    pub flavor: SecFlavor,
}

impl GssReply {
    pub fn principal(&self) -> &str {
        &self.ctx.principal
    }
}

/// The server side of RPCSEC_GSS.
pub struct GssServer {
    acceptor: Acceptor,
    idmap: IdMap,
    contexts: Mutex<HashMap<Vec<u8>, Arc<GssContext>>>,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn put_opaque(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out.resize(out.len() + (4 - data.len() % 4) % 4, 0);
}

/// Reads an XDR opaque<> at the front of `buf`; returns it and the rest.
fn take_opaque(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = u32::from_be_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let data = buf.get(4..4usize.checked_add(len)?)?;
    let padded = 4 + len + (4 - len % 4) % 4;
    Some((data, buf.get(padded..).unwrap_or(&[])))
}

fn denied(xid: u32, stat: AuthStat) -> Box<RpcReplyMsg> {
    Box::new(RpcReplyMsg { xid, body: MsgType::Reply(ReplyBody::MsgDenied(RejectedReply::AuthError(stat))) })
}

fn accepted(xid: u32, verf: OpaqueAuth, reply_data: AcceptBody) -> Box<RpcReplyMsg> {
    Box::new(RpcReplyMsg { xid, body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply { verf, reply_data })) })
}

impl GssServer {
    pub fn new(keytab: Keytab, mappings: &[PrincipalMapping]) -> Self {
        Self::with_etc(keytab, mappings, Path::new("/etc"))
    }

    /// As [`GssServer::new`], reading `passwd` and `group` from `etc`.
    pub fn with_etc(keytab: Keytab, mappings: &[PrincipalMapping], etc: &Path) -> Self {
        let realms: Vec<String> = keytab.entries.iter().map(|e| e.realm.clone()).collect();
        GssServer {
            idmap: IdMap::new(mappings, realms, etc),
            acceptor: Acceptor::new(keytab),
            contexts: Mutex::new(HashMap::new()),
        }
    }

    /// Service principals this server accepts (for the startup log).
    pub fn principals(&self) -> Vec<String> {
        let mut p: Vec<String> = self.acceptor.keytab().entries.iter().map(|e| e.principal()).collect();
        p.sort();
        p.dedup();
        p
    }

    fn contexts(&self) -> std::sync::MutexGuard<'_, HashMap<Vec<u8>, Arc<GssContext>>> {
        self.contexts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Number of established contexts.
    pub fn context_count(&self) -> usize {
        self.contexts().len()
    }

    /// Handles a call whose credential is RPCSEC_GSS.
    pub fn handle(&self, msg: RpcCallMsg) -> GssCall {
        let xid = msg.xid;
        let mut call = match msg.body {
            MsgType::Call(call) => call,
            _ => return GssCall::Drop,
        };
        let cred = match &call.cred {
            OpaqueAuth::AuthGss(c) => c.clone(),
            _ => return GssCall::Reply(denied(xid, AuthStat::AuthBadCred)),
        };
        if cred.version != 1 {
            return GssCall::Reply(denied(xid, AuthStat::AuthBadCred));
        }
        let raw = call.gss.take().unwrap_or_default();
        match cred.gss_proc {
            RPCSEC_GSS_INIT | RPCSEC_GSS_CONTINUE_INIT => GssCall::Reply(self.init(xid, &call, &cred, &raw.args)),
            RPCSEC_GSS_DATA | RPCSEC_GSS_DESTROY => self.data(xid, call, cred, raw),
            _ => GssCall::Reply(denied(xid, AuthStat::AuthBadCred)),
        }
    }

    fn init(&self, xid: u32, call: &CallBody, cred: &RpcSecGssCred, args: &[u8]) -> Box<RpcReplyMsg> {
        // rpc_gss_init_res: handle, major, minor, seq_window, output token.
        let init_res = |handle: &[u8], major: u32, window: u32, token: &[u8]| {
            let mut b = Vec::new();
            put_opaque(&mut b, handle);
            b.extend_from_slice(&major.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&window.to_be_bytes());
            put_opaque(&mut b, token);
            b
        };
        let fail = |major: u32| accepted(xid, OpaqueAuth::AuthNull(Vec::new()), AcceptBody::SuccessRaw(init_res(&[], major, 0, &[])));

        if call.proc != 0 {
            return denied(xid, AuthStat::AuthBadCred);
        }
        // The Kerberos mechanism finishes in one round trip.
        if cred.gss_proc == RPCSEC_GSS_CONTINUE_INIT {
            return fail(GSS_S_NO_CONTEXT);
        }
        let token = match take_opaque(args) {
            Some((t, _)) => t,
            None => return accepted(xid, OpaqueAuth::AuthNull(Vec::new()), AcceptBody::GarbageArgs),
        };
        let acc = match self.acceptor.accept(token) {
            Ok(a) => a,
            Err(e) => {
                warn!(xid, error = %e, "nfs_audit: RPCSEC_GSS context refused");
                let major = match e {
                    nextnfs_krb5::Error::Token(_) | nextnfs_krb5::Error::Asn1(_) => GSS_S_DEFECTIVE_TOKEN,
                    nextnfs_krb5::Error::Rejected("ticket expired") => GSS_S_CREDENTIALS_EXPIRED,
                    nextnfs_krb5::Error::Integrity | nextnfs_krb5::Error::NoKey(_) => GSS_S_DEFECTIVE_CREDENTIAL,
                    _ => GSS_S_FAILURE,
                };
                return fail(major);
            }
        };
        let handle = match nextnfs_krb5::crypto::random_bytes(16) {
            Ok(h) => h,
            Err(_) => return fail(GSS_S_FAILURE),
        };
        let identity = self.idmap.identity(&acc.client);
        info!(
            client = %acc.client,
            service = %acc.service,
            enctype = ?acc.key.enctype,
            uid = ?identity.as_ref().map(|i| i.0),
            "RPCSEC_GSS context established"
        );
        let ctx = Arc::new(GssContext {
            key: acc.key,
            principal: acc.client,
            identity,
            expires: acc.expires,
            created: std::time::Instant::now(),
            window: Mutex::new(SeqWindow::default()),
            send_seq: AtomicU64::new(acc.acceptor_seq),
        });
        let verf = OpaqueAuth::GssVerf(ctx.mic(&SEQ_WINDOW.to_be_bytes()));
        {
            let mut table = self.contexts();
            if table.len() >= MAX_CONTEXTS {
                let now = now_secs();
                table.retain(|_, c| c.expires > now);
                while table.len() >= MAX_CONTEXTS {
                    let oldest = table.iter().min_by_key(|(_, c)| c.created).map(|(k, _)| k.clone());
                    match oldest {
                        Some(k) => table.remove(&k),
                        None => break,
                    };
                }
            }
            table.insert(handle.clone(), ctx);
        }
        let output = acc.output_token.unwrap_or_default();
        accepted(xid, verf, AcceptBody::SuccessRaw(init_res(&handle, 0, SEQ_WINDOW, &output)))
    }

    fn data(&self, xid: u32, mut call: CallBody, cred: RpcSecGssCred, raw: nextnfs_proto::rpc_proto::GssCallData) -> GssCall {
        let ctx = match self.contexts().get(&cred.handle) {
            Some(c) => c.clone(),
            None => {
                debug!(xid, "RPCSEC_GSS: unknown context handle");
                return GssCall::Reply(denied(xid, AuthStat::RpcsecGssCredProblem));
            }
        };
        if ctx.expires < now_secs() {
            self.contexts().remove(&cred.handle);
            return GssCall::Reply(denied(xid, AuthStat::RpcsecGssCtxProblem));
        }
        // The verifier is the MIC of the header, XID through credential.
        let verified = match &call.verf {
            OpaqueAuth::GssVerf(tok) => ctx.key.verify_mic(Sender::Initiator, &raw.header, tok).is_ok(),
            _ => false,
        };
        if !verified {
            warn!(xid, client = %ctx.principal, "nfs_audit: RPCSEC_GSS header checksum failed");
            return GssCall::Reply(denied(xid, AuthStat::RpcsecGssCredProblem));
        }
        if cred.seq_num >= MAXSEQ {
            self.contexts().remove(&cred.handle);
            return GssCall::Reply(denied(xid, AuthStat::RpcsecGssCtxProblem));
        }
        if !ctx.window.lock().unwrap_or_else(|p| p.into_inner()).accept(cred.seq_num) {
            debug!(xid, seq = cred.seq_num, "RPCSEC_GSS: sequence number replayed or behind the window, dropped");
            return GssCall::Drop;
        }
        let seq_verf = || OpaqueAuth::GssVerf(ctx.mic(&cred.seq_num.to_be_bytes()));

        if cred.gss_proc == RPCSEC_GSS_DESTROY {
            self.contexts().remove(&cred.handle);
            info!(client = %ctx.principal, "RPCSEC_GSS context destroyed");
            return GssCall::Reply(accepted(xid, seq_verf(), AcceptBody::SuccessRaw(Vec::new())));
        }

        let flavor = match SecFlavor::from_gss_service(cred.service) {
            Some(f) => f,
            None => return GssCall::Reply(denied(xid, AuthStat::AuthBadCred)),
        };
        let garbage = || GssCall::Reply(accepted(xid, seq_verf(), AcceptBody::GarbageArgs));
        // rpc_gss_integ_data / rpc_gss_priv_data carry `seq_num | args`.
        let args: Vec<u8> = match cred.service {
            RPC_GSS_SVC_NONE => raw.args,
            RPC_GSS_SVC_INTEGRITY => {
                let Some((body, rest)) = take_opaque(&raw.args) else { return garbage() };
                let Some((cksum, _)) = take_opaque(rest) else { return garbage() };
                if ctx.key.verify_mic(Sender::Initiator, body, cksum).is_err() {
                    warn!(xid, client = %ctx.principal, "nfs_audit: RPCSEC_GSS argument checksum failed");
                    return garbage();
                }
                body.to_vec()
            }
            RPC_GSS_SVC_PRIVACY => {
                let Some((tok, _)) = take_opaque(&raw.args) else { return garbage() };
                match ctx.key.unwrap(Sender::Initiator, tok) {
                    Ok((plain, true, _)) => plain,
                    _ => {
                        warn!(xid, client = %ctx.principal, "nfs_audit: RPCSEC_GSS arguments failed to decrypt");
                        return garbage();
                    }
                }
            }
            _ => return GssCall::Reply(denied(xid, AuthStat::AuthBadCred)),
        };
        let args: &[u8] = if cred.service == RPC_GSS_SVC_NONE {
            &args
        } else {
            match args.get(..4) {
                Some(s) if u32::from_be_bytes(s.try_into().expect("4")) == cred.seq_num => &args[4..],
                _ => return garbage(),
            }
        };
        if call.proc == 1 && call.prog == 100003 {
            match nextnfs_proto::decode_compound_args(args) {
                Ok(a) => call.args = Some(a),
                Err(e) => {
                    debug!(xid, error = %e, "RPCSEC_GSS: COMPOUND arguments do not decode");
                    return garbage();
                }
            }
        }
        call.cred = match &ctx.identity {
            Some((uid, gid, gids)) => OpaqueAuth::AuthUnix(AuthUnix {
                stamp: 0,
                machinename: ctx.principal.clone(),
                uid: *uid,
                gid: *gid,
                gids: gids.clone(),
            }),
            None => OpaqueAuth::AuthNull(Vec::new()),
        };
        let proc = call.proc;
        GssCall::Proceed(
            RpcCallMsg { xid, body: MsgType::Call(call) },
            GssReply { ctx, seq: cred.seq_num, service: cred.service, proc, flavor },
        )
    }

    /// Adds the RPCSEC_GSS verifier to a DATA call's reply and, for the
    /// integrity and privacy services, wraps its results.
    pub fn wrap_reply(&self, mut reply: Box<RpcReplyMsg>, info: &GssReply) -> Box<RpcReplyMsg> {
        let MsgType::Reply(ReplyBody::MsgAccepted(acc)) = &mut reply.body else { return reply };
        acc.verf = OpaqueAuth::GssVerf(info.ctx.mic(&info.seq.to_be_bytes()));
        if info.service == RPC_GSS_SVC_NONE {
            return reply;
        }
        let results = match &acc.reply_data {
            // NULL has no results.
            AcceptBody::Success(_) if info.proc == 0 => Vec::new(),
            AcceptBody::Success(res) => match nextnfs_proto::encode_compound_res(res) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "RPCSEC_GSS: results do not encode");
                    acc.reply_data = AcceptBody::GarbageArgs;
                    return reply;
                }
            },
            AcceptBody::SuccessRaw(b) => b.clone(),
            _ => return reply,
        };
        let mut body = Vec::with_capacity(4 + results.len());
        body.extend_from_slice(&info.seq.to_be_bytes());
        body.extend_from_slice(&results);
        let mut out = Vec::with_capacity(body.len() + 64);
        if info.service == RPC_GSS_SVC_INTEGRITY {
            let mic = info.ctx.mic(&body);
            put_opaque(&mut out, &body);
            put_opaque(&mut out, &mic);
        } else if info.service == RPC_GSS_SVC_PRIVACY {
            let seq = info.ctx.send_seq.fetch_add(1, Ordering::Relaxed);
            match info.ctx.key.wrap(Sender::Acceptor, seq, true, &body) {
                Ok(tok) => put_opaque(&mut out, &tok),
                Err(e) => {
                    warn!(error = %e, "RPCSEC_GSS: results do not encrypt");
                    acc.reply_data = AcceptBody::GarbageArgs;
                    return reply;
                }
            }
        }
        acc.reply_data = AcceptBody::SuccessRaw(out);
        reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nextnfs_krb5::acceptor::GSS_C_MUTUAL_FLAG;
    use nextnfs_krb5::initiator::{issue_ticket, Initiator};
    use nextnfs_krb5::{Enctype, KeytabEntry};
    use nextnfs_proto::nfs4_proto::{Compound4args, Compound4res, NfsStat4};

    fn now() -> i64 {
        now_secs()
    }

    /// An RPC call as a client puts it on the wire: the header, then the
    /// verifier (AUTH_NONE, or the MIC of the header), then `args`.
    fn call_bytes(xid: u32, proc: u32, cred: &RpcSecGssCred, key: Option<&ContextKey>, args: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        for w in [xid, 0, 2, 100003, 4, proc, 6] {
            b.extend_from_slice(&w.to_be_bytes());
        }
        put_opaque(&mut b, &cred.encode());
        match key {
            Some(k) => {
                let mic = k.get_mic(Sender::Initiator, 0, &b).unwrap();
                b.extend_from_slice(&6u32.to_be_bytes());
                put_opaque(&mut b, &mic);
            }
            None => b.extend_from_slice(&[0; 8]),
        }
        b.extend_from_slice(args);
        b
    }

    /// Splits an accepted reply into (verifier flavour, verifier body,
    /// accept_stat, results); or (u32::MAX, [], auth_stat, []) if denied.
    fn parse_reply(reply: &RpcReplyMsg) -> (u32, Vec<u8>, u32, Vec<u8>) {
        let b = reply.to_bytes().unwrap();
        let w = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
        assert_eq!(w(4), 1, "a reply");
        if w(8) == 1 {
            return (u32::MAX, Vec::new(), w(16), Vec::new());
        }
        let flavor = w(12);
        let (verf, rest) = take_opaque(&b[16..]).unwrap();
        let stat = u32::from_be_bytes(rest[..4].try_into().unwrap());
        (flavor, verf.to_vec(), stat, rest[4..].to_vec())
    }

    fn handle_bytes(g: &GssServer, bytes: Vec<u8>) -> GssCall {
        g.handle(nextnfs_proto::from_bytes(bytes).unwrap())
    }

    fn reply_of(call: GssCall) -> Box<RpcReplyMsg> {
        match call {
            GssCall::Reply(r) => r,
            GssCall::Drop => panic!("dropped"),
            GssCall::Proceed(..) => panic!("proceeded"),
        }
    }

    fn setup() -> (GssServer, tempfile::TempDir, Initiator, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("passwd"), "alice:x:1001:1001::/:/bin/sh\n").unwrap();
        std::fs::write(dir.path().join("group"), "").unwrap();
        let svc = KeytabEntry {
            realm: "EXAMPLE.LO".into(),
            components: vec!["nfs".into(), "server.example.lo".into()],
            name_type: 3,
            timestamp: 0,
            kvno: 2,
            enctype: Enctype::Aes256CtsHmacSha1_96.number(),
            key: nextnfs_krb5::crypto::random_bytes(32).unwrap(),
        };
        let g = GssServer::with_etc(Keytab { entries: vec![svc.clone()] }, &[], dir.path());
        let cred = issue_ticket(&svc, "alice", Enctype::Aes256CtsHmacSha1_96, now() - 10, now() + 3600).unwrap();
        let (mut ini, token) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG | 0x30, Some(Enctype::Aes256CtsHmacSha1_96), now()).unwrap();

        let init = RpcSecGssCred { version: 1, gss_proc: RPCSEC_GSS_INIT, seq_num: 0, service: 1, handle: vec![] };
        let mut args = Vec::new();
        put_opaque(&mut args, &token);
        let reply = reply_of(handle_bytes(&g, call_bytes(1, 0, &init, None, &args)));
        let (flavor, verf, stat, res) = parse_reply(&reply);
        assert_eq!((flavor, stat), (6, 0));
        let (handle, rest) = take_opaque(&res).unwrap();
        let w = |i: usize| u32::from_be_bytes(rest[i..i + 4].try_into().unwrap());
        assert_eq!((w(0), w(4), w(8)), (0, 0, SEQ_WINDOW), "major, minor, window");
        let (out_token, _) = take_opaque(&rest[12..]).unwrap();
        ini.finish(out_token).unwrap();
        ini.key.verify_mic(Sender::Acceptor, &SEQ_WINDOW.to_be_bytes(), &verf).expect("INIT verifier is the MIC of the window");
        assert_eq!(g.context_count(), 1);
        (g, dir, ini, handle.to_vec())
    }

    fn data_args(ini: &Initiator, service: u32, seq: u32, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if service == RPC_GSS_SVC_NONE {
            return plain.to_vec();
        }
        let mut body = seq.to_be_bytes().to_vec();
        body.extend_from_slice(plain);
        if service == RPC_GSS_SVC_INTEGRITY {
            put_opaque(&mut out, &body);
            put_opaque(&mut out, &ini.key.get_mic(Sender::Initiator, seq as u64, &body).unwrap());
        } else {
            put_opaque(&mut out, &ini.key.wrap(Sender::Initiator, seq as u64, true, &body).unwrap());
        }
        out
    }

    #[test]
    fn init_then_compound_under_each_service() {
        let (g, _dir, ini, handle) = setup();
        let args = Compound4args { tag: "gss".into(), minor_version: 0, argarray: vec![] };
        let plain = nextnfs_proto::xdr::to_bytes(&args).unwrap();
        let res = Compound4res { status: NfsStat4::Nfs4Ok, tag: "gss".into(), resarray: vec![] };
        let res_bytes = nextnfs_proto::encode_compound_res(&res).unwrap();
        for (i, service) in [RPC_GSS_SVC_NONE, RPC_GSS_SVC_INTEGRITY, RPC_GSS_SVC_PRIVACY].into_iter().enumerate() {
            let seq = 10 + i as u32;
            let cred = RpcSecGssCred { version: 1, gss_proc: RPCSEC_GSS_DATA, seq_num: seq, service, handle: handle.clone() };
            let bytes = call_bytes(100 + seq, 1, &cred, Some(&ini.key), &data_args(&ini, service, seq, &plain));
            if service == RPC_GSS_SVC_PRIVACY {
                assert!(!bytes.windows(3).any(|w| w == b"gss"), "krb5p arguments are encrypted");
            }
            let (msg, info) = match handle_bytes(&g, bytes.clone()) {
                GssCall::Proceed(m, info) => (m, info),
                _ => panic!("service {} did not proceed", service),
            };
            assert_eq!(info.flavor, SecFlavor::from_gss_service(service).unwrap());
            assert_eq!(info.principal(), "alice@EXAMPLE.LO");
            let MsgType::Call(call) = &msg.body else { panic!() };
            assert_eq!(call.args.as_ref().unwrap().tag, "gss");
            match &call.cred {
                OpaqueAuth::AuthUnix(a) => assert_eq!((a.uid, a.gid), (1001, 1001)),
                other => panic!("identity not mapped: {:?}", other),
            }

            // The reply: verifier = MIC(seq); results wrapped per service.
            let reply = Box::new(RpcReplyMsg {
                xid: msg.xid,
                body: MsgType::Reply(ReplyBody::MsgAccepted(AcceptedReply {
                    verf: OpaqueAuth::AuthNull(Vec::new()),
                    reply_data: AcceptBody::Success(res.clone()),
                })),
            });
            let (flavor, verf, stat, results) = parse_reply(&g.wrap_reply(reply, &info));
            assert_eq!((flavor, stat), (6, 0));
            ini.key.verify_mic(Sender::Acceptor, &seq.to_be_bytes(), &verf).unwrap();
            let body = match service {
                RPC_GSS_SVC_NONE => {
                    assert_eq!(results, res_bytes);
                    continue;
                }
                RPC_GSS_SVC_INTEGRITY => {
                    let (body, rest) = take_opaque(&results).unwrap();
                    let (mic, _) = take_opaque(rest).unwrap();
                    ini.key.verify_mic(Sender::Acceptor, body, mic).unwrap();
                    body.to_vec()
                }
                _ => {
                    let (tok, _) = take_opaque(&results).unwrap();
                    let (body, sealed, _) = ini.key.unwrap(Sender::Acceptor, tok).unwrap();
                    assert!(sealed);
                    body
                }
            };
            assert_eq!(&body[..4], &seq.to_be_bytes());
            assert_eq!(&body[4..], &res_bytes[..]);

            // The same call again is a replay: no reply at all.
            assert!(matches!(handle_bytes(&g, bytes), GssCall::Drop));
        }
    }

    #[test]
    fn data_errors_and_destroy() {
        let (g, _dir, ini, handle) = setup();
        let args = nextnfs_proto::xdr::to_bytes(&Compound4args { tag: "t".into(), minor_version: 0, argarray: vec![] }).unwrap();
        let cred = |seq: u32, service: u32, h: &[u8], p: u32| RpcSecGssCred {
            version: 1,
            gss_proc: p,
            seq_num: seq,
            service,
            handle: h.to_vec(),
        };
        let denied_with = |call: GssCall| parse_reply(&reply_of(call)).2;

        // Unknown context.
        let c = cred(1, 1, b"nope", RPCSEC_GSS_DATA);
        assert_eq!(denied_with(handle_bytes(&g, call_bytes(1, 1, &c, Some(&ini.key), &args))), 13);

        // Verifier over a different header (another key).
        let other = ContextKey { enctype: Enctype::Aes256CtsHmacSha1_96, key: vec![7; 32], acceptor_subkey: false };
        let c = cred(2, 1, &handle, RPCSEC_GSS_DATA);
        assert_eq!(denied_with(handle_bytes(&g, call_bytes(2, 1, &c, Some(&other), &args))), 13);

        // Tampered integrity checksum: GARBAGE_ARGS (accept_stat 4).
        let c = cred(3, RPC_GSS_SVC_INTEGRITY, &handle, RPCSEC_GSS_DATA);
        let mut a = data_args(&ini, RPC_GSS_SVC_INTEGRITY, 3, &args);
        let last = a.len() - 1;
        a[last - 3] ^= 1;
        let (flavor, _, stat, _) = parse_reply(&reply_of(handle_bytes(&g, call_bytes(3, 1, &c, Some(&ini.key), &a))));
        assert_eq!((flavor, stat), (6, 4));

        // Sequence number inside the body must match the credential's.
        let c = cred(4, RPC_GSS_SVC_INTEGRITY, &handle, RPCSEC_GSS_DATA);
        let a = data_args(&ini, RPC_GSS_SVC_INTEGRITY, 5, &args);
        assert_eq!(parse_reply(&reply_of(handle_bytes(&g, call_bytes(4, 1, &c, Some(&ini.key), &a)))).2, 4);

        // Unknown service.
        let c = cred(6, 9, &handle, RPCSEC_GSS_DATA);
        assert_eq!(denied_with(handle_bytes(&g, call_bytes(6, 1, &c, Some(&ini.key), &args))), 1);

        // Sequence number at MAXSEQ ends the context.
        let c = cred(MAXSEQ, 1, &handle, RPCSEC_GSS_DATA);
        assert_eq!(denied_with(handle_bytes(&g, call_bytes(7, 1, &c, Some(&ini.key), &args))), 14);
        assert_eq!(g.context_count(), 0);
    }

    #[test]
    fn destroy_removes_the_context() {
        let (g, _dir, ini, handle) = setup();
        let c = RpcSecGssCred { version: 1, gss_proc: RPCSEC_GSS_DESTROY, seq_num: 1, service: 1, handle: handle.clone() };
        let (flavor, verf, stat, results) = parse_reply(&reply_of(handle_bytes(&g, call_bytes(1, 0, &c, Some(&ini.key), &[]))));
        assert_eq!((flavor, stat), (6, 0));
        assert!(results.is_empty());
        ini.key.verify_mic(Sender::Acceptor, &1u32.to_be_bytes(), &verf).unwrap();
        assert_eq!(g.context_count(), 0);
        let c = RpcSecGssCred { version: 1, gss_proc: RPCSEC_GSS_DATA, seq_num: 2, service: 1, handle };
        assert_eq!(parse_reply(&reply_of(handle_bytes(&g, call_bytes(2, 0, &c, Some(&ini.key), &[])))).2, 13);
    }

    #[test]
    fn init_failures_report_gss_status() {
        let (g, _dir, _ini, _) = setup();
        let init = RpcSecGssCred { version: 1, gss_proc: RPCSEC_GSS_INIT, seq_num: 0, service: 1, handle: vec![] };
        let mut args = Vec::new();
        put_opaque(&mut args, b"not a token");
        let (flavor, _, stat, res) = parse_reply(&reply_of(handle_bytes(&g, call_bytes(1, 0, &init, None, &args))));
        assert_eq!((flavor, stat), (0, 0));
        let (h, rest) = take_opaque(&res).unwrap();
        assert!(h.is_empty());
        assert_eq!(u32::from_be_bytes(rest[..4].try_into().unwrap()), GSS_S_DEFECTIVE_TOKEN);
        // INIT on a procedure other than NULL, and an unknown version.
        assert_eq!(parse_reply(&reply_of(handle_bytes(&g, call_bytes(2, 1, &init, None, &args)))).2, 1);
        let v2 = RpcSecGssCred { version: 2, ..init };
        assert_eq!(parse_reply(&reply_of(handle_bytes(&g, call_bytes(3, 0, &v2, None, &args)))).2, 1);
    }

    #[test]
    fn seq_window() {
        let mut w = SeqWindow::default();
        assert!(w.accept(10));
        assert!(!w.accept(10), "replay");
        assert!(w.accept(12));
        assert!(w.accept(11), "late but in the window");
        assert!(!w.accept(11));
        assert!(w.accept(500));
        assert!(w.accept(500 - 127));
        assert!(!w.accept(500 - 128), "below the window");
        assert!(w.accept(10_000));
        assert!(!w.accept(500));
    }

    #[test]
    fn opaque_helpers() {
        let mut b = Vec::new();
        put_opaque(&mut b, &[1, 2, 3]);
        put_opaque(&mut b, &[]);
        assert_eq!(b, vec![0, 0, 0, 3, 1, 2, 3, 0, 0, 0, 0, 0]);
        let (first, rest) = take_opaque(&b).unwrap();
        assert_eq!(first, &[1, 2, 3]);
        let (second, rest) = take_opaque(rest).unwrap();
        assert!(second.is_empty() && rest.is_empty());
        assert!(take_opaque(&[0, 0, 0, 9, 1]).is_none());
        assert!(take_opaque(&[0xff, 0xff, 0xff, 0xff]).is_none());
    }

    #[test]
    fn idmap_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("passwd"), "root:x:0:0::/root:/bin/sh\nalice:x:1001:1001::/home/alice:/bin/sh\n").unwrap();
        std::fs::write(dir.path().join("group"), "wheel:x:10:alice,bob\nalice:x:1001:\nstaff:x:50:alice\n").unwrap();
        let map = IdMap::new(
            &[PrincipalMapping { principal: "bob@EXAMPLE.LO".into(), uid: 2000, gid: 2000, gids: vec![10] }],
            ["EXAMPLE.LO".to_string()],
            dir.path(),
        );
        assert_eq!(map.identity("bob@EXAMPLE.LO"), Some((2000, 2000, vec![10])));
        assert_eq!(map.identity("alice@EXAMPLE.LO"), Some((1001, 1001, vec![10, 50])));
        assert_eq!(map.identity("nfs/client.example.lo@EXAMPLE.LO"), Some((0, 0, vec![])));
        assert_eq!(map.identity("host/client.example.lo@EXAMPLE.LO"), Some((0, 0, vec![])));
        assert_eq!(map.identity("alice@OTHER.REALM"), None, "foreign realm");
        assert_eq!(map.identity("carol@EXAMPLE.LO"), None, "not in passwd");
        assert_eq!(map.identity("alice/admin@EXAMPLE.LO"), None);
        assert_eq!(map.identity("noatsign"), None);
    }
}
