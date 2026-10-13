//! GSS-API `accept_sec_context` for Kerberos V5 (RFC 4121 §4.1, RFC 4120
//! §3.2.3): decrypt the ticket with the service key from the keytab,
//! decrypt and check the authenticator, answer with an AP-REP when the
//! initiator asks for mutual authentication.
//!
//! Follows irondirectory's `iron-ldap` acceptor (`crates/ldap/src/gssapi/
//! accept.rs`) with these differences: the authenticator and AP-REP use
//! the *session key's* enctype (it may differ from the ticket's), the
//! ticket's start time is checked, authenticators are kept in a replay
//! cache, and the AP-REP carries the acceptor's initial sequence number.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use rasn_kerberos::{ApRep, ApReq, Authenticator, EncApRepPart, EncTicketPart, EncryptedData, KerberosTime, PrincipalName};

use crate::cfx::ContextKey;
use crate::crypto::{self, Enctype};
use crate::keytab::Keytab;
use crate::{token, Error};

/// Allowed clock difference between client and server (RFC 4120 default).
pub const CLOCK_SKEW_SECS: i64 = 300;

/// GSS checksum type in the authenticator (RFC 4121 §4.1.1).
const GSS_CHECKSUM_TYPE: i32 = 0x8003;

pub const GSS_C_DELEG_FLAG: u32 = 1;
pub const GSS_C_MUTUAL_FLAG: u32 = 2;
pub const GSS_C_REPLAY_FLAG: u32 = 4;
pub const GSS_C_SEQUENCE_FLAG: u32 = 8;
pub const GSS_C_CONF_FLAG: u32 = 16;
pub const GSS_C_INTEG_FLAG: u32 = 32;

/// RFC 4120 §7.5.1 key usages.
pub const KU_TICKET: u32 = 2;
pub const KU_AP_REQ_AUTH: u32 = 11;
pub const KU_AP_REP_ENCPART: u32 = 12;

/// An established context.
#[derive(Debug)]
pub struct Accepted {
    /// The client principal, `name[/instance]@REALM`.
    pub client: String,
    /// The service principal the ticket was for, `nfs/host@REALM`.
    pub service: String,
    /// Key for MIC and Wrap tokens (the initiator's subkey, else the
    /// ticket session key).
    pub key: ContextKey,
    /// GSS flags the initiator asked for.
    pub flags: u32,
    /// First sequence number the initiator will send.
    pub initiator_seq: u64,
    /// First sequence number this acceptor sends (announced in the AP-REP).
    pub acceptor_seq: u64,
    /// Ticket end time, seconds since the epoch: the context expires then.
    pub expires: i64,
    /// AP-REP token to return (mutual authentication), framed.
    pub output_token: Option<Vec<u8>>,
}

pub(crate) fn gstring(s: &rasn::types::GeneralString) -> String {
    String::from_utf8_lossy(s.as_bytes()).into_owned()
}

pub(crate) fn principal_string(name: &PrincipalName, realm: &rasn::types::GeneralString) -> String {
    let parts: Vec<String> = name.string.iter().map(gstring).collect();
    format!("{}@{}", parts.join("/"), gstring(realm))
}

pub(crate) fn kerberos_time(secs: i64) -> KerberosTime {
    KerberosTime(Utc.timestamp_opt(secs, 0).single().expect("valid timestamp").fixed_offset())
}

fn now_secs() -> i64 {
    Utc::now().timestamp()
}

/// Seen authenticators, so a captured AP-REQ can not be replayed within
/// the clock-skew window (RFC 4120 §3.2.3).
#[derive(Default)]
struct ReplayCache {
    seen: HashMap<Vec<u8>, Instant>,
}

impl ReplayCache {
    fn check_and_insert(&mut self, key: Vec<u8>) -> bool {
        let now = Instant::now();
        let ttl = Duration::from_secs(2 * CLOCK_SKEW_SECS as u64);
        if self.seen.len() > 1024 {
            self.seen.retain(|_, t| now.duration_since(*t) < ttl);
        }
        match self.seen.get(&key) {
            Some(t) if now.duration_since(*t) < ttl => false,
            _ => {
                self.seen.insert(key, now);
                true
            }
        }
    }
}

/// Accepts Kerberos AP-REQs for the principals in a keytab.
pub struct Acceptor {
    keytab: Keytab,
    replay: Mutex<ReplayCache>,
}

impl Acceptor {
    pub fn new(keytab: Keytab) -> Self {
        Acceptor { keytab, replay: Mutex::new(ReplayCache::default()) }
    }

    pub fn keytab(&self) -> &Keytab {
        &self.keytab
    }

    /// Accepts a framed GSS initial context token carrying an AP-REQ.
    pub fn accept(&self, input: &[u8]) -> Result<Accepted, Error> {
        self.accept_at(input, now_secs())
    }

    pub(crate) fn accept_at(&self, input: &[u8], now: i64) -> Result<Accepted, Error> {
        let (tok_id, msg) = token::parse(input)?;
        if tok_id != token::TOK_ID_AP_REQ {
            return Err(Error::Token("expected an AP-REQ"));
        }
        let ap_req: ApReq = rasn::der::decode(msg).map_err(|e| Error::Asn1(e.to_string()))?;
        let ticket = &ap_req.ticket;
        let service = principal_string(&ticket.sname, &ticket.realm);
        let tkt_etype = Enctype::from_i32(ticket.enc_part.etype).ok_or(Error::UnsupportedEnctype(ticket.enc_part.etype))?;

        // Service keys for this principal and enctype; the ticket's kvno first.
        let sname: Vec<String> = ticket.sname.string.iter().map(gstring).collect();
        let realm = gstring(&ticket.realm);
        let mut candidates: Vec<_> = self
            .keytab
            .entries
            .iter()
            .filter(|e| e.realm == realm && e.components == sname && e.enctype == tkt_etype.number())
            .collect();
        if candidates.is_empty() {
            return Err(Error::NoKey(format!("{} ({:?})", service, tkt_etype)));
        }
        let kvno = ticket.enc_part.kvno;
        candidates.sort_by_key(|e| (Some(e.kvno) != kvno, std::cmp::Reverse(e.kvno)));
        let enc_ticket = candidates
            .iter()
            .find_map(|e| crypto::decrypt(tkt_etype, &e.key, KU_TICKET, &ticket.enc_part.cipher).ok())
            .ok_or(Error::Integrity)?;
        let tkt: EncTicketPart = rasn::der::decode(&enc_ticket).map_err(|e| Error::Asn1(e.to_string()))?;

        let start = tkt.start_time.as_ref().unwrap_or(&tkt.auth_time).0.timestamp();
        let end = tkt.end_time.0.timestamp();
        if start - CLOCK_SKEW_SECS > now {
            return Err(Error::Rejected("ticket not yet valid"));
        }
        if end + CLOCK_SKEW_SECS < now {
            return Err(Error::Rejected("ticket expired"));
        }

        // The authenticator is encrypted in the session key, whose
        // enctype is the key's own, not the ticket's.
        let sess_etype = Enctype::from_i32(tkt.key.r#type).ok_or(Error::UnsupportedEnctype(tkt.key.r#type))?;
        let sess_key = tkt.key.value.to_vec();
        let auth_bytes = crypto::decrypt(sess_etype, &sess_key, KU_AP_REQ_AUTH, &ap_req.authenticator.cipher)?;
        let auth: Authenticator = rasn::der::decode(&auth_bytes).map_err(|e| Error::Asn1(e.to_string()))?;
        if auth.crealm != tkt.crealm || auth.cname != tkt.cname {
            return Err(Error::Rejected("authenticator does not match the ticket"));
        }
        if (auth.ctime.0.timestamp() - now).abs() > CLOCK_SKEW_SECS {
            return Err(Error::Rejected("clock skew too great"));
        }
        let cksum = auth.cksum.as_ref().ok_or(Error::Rejected("no GSS checksum in the authenticator"))?;
        if cksum.r#type != GSS_CHECKSUM_TYPE || cksum.checksum.len() < 24 {
            return Err(Error::Rejected("malformed GSS checksum"));
        }
        let c = &cksum.checksum;
        let flags = u32::from_le_bytes([c[20], c[21], c[22], c[23]]);

        let client = principal_string(&tkt.cname, &tkt.crealm);
        {
            let mut key = client.clone().into_bytes();
            key.push(0);
            key.extend_from_slice(&auth_bytes);
            let mut cache = self.replay.lock().unwrap_or_else(|p| p.into_inner());
            if !cache.check_and_insert(key) {
                return Err(Error::Rejected("authenticator replayed"));
            }
        }

        // RFC 4121 §2: the initiator's subkey, if it sent one, is the
        // base key of the context; otherwise the session key.
        let key = match &auth.subkey {
            Some(sk) => ContextKey {
                enctype: Enctype::from_i32(sk.r#type).ok_or(Error::UnsupportedEnctype(sk.r#type))?,
                key: sk.value.to_vec(),
                acceptor_subkey: false,
            },
            None => ContextKey { enctype: sess_etype, key: sess_key.clone(), acceptor_subkey: false },
        };
        if key.key.len() != key.enctype.key_len() {
            return Err(Error::Rejected("subkey length does not match its enctype"));
        }
        let initiator_seq = auth.seq_number.unwrap_or(0) as u64;

        let mutual = flags & GSS_C_MUTUAL_FLAG != 0;
        let (acceptor_seq, output_token) = if mutual {
            let seq = u32::from_be_bytes(crypto::random_bytes(4)?.try_into().expect("4")) & 0x3fff_ffff;
            // RFC 4120 §3.2.4: echo the client's ctime/cusec; encrypted in
            // the ticket session key (never the subkey), usage 12.
            let part = EncApRepPart { ctime: auth.ctime.clone(), cusec: auth.cusec.clone(), subkey: None, seq_number: Some(seq) };
            let part = rasn::der::encode(&part).map_err(|e| Error::Asn1(e.to_string()))?;
            let cipher = crypto::encrypt(sess_etype, &sess_key, KU_AP_REP_ENCPART, &part)?;
            let rep = ApRep {
                pvno: 5.into(),
                msg_type: 15.into(),
                enc_part: EncryptedData { etype: sess_etype.number(), kvno: None, cipher: cipher.into() },
            };
            let rep = rasn::der::encode(&rep).map_err(|e| Error::Asn1(e.to_string()))?;
            (seq as u64, Some(token::build(token::TOK_ID_AP_REP, &rep)))
        } else {
            // Without mutual authentication both directions start at the
            // initiator's number (RFC 4121 §4.2.6.1 / RFC 1964 practice).
            (initiator_seq, None)
        };

        Ok(Accepted { client, service, key, flags, initiator_seq, acceptor_seq, expires: end, output_token })
    }
}
