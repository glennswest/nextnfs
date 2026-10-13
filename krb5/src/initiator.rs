//! A minimal Kerberos initiator plus ticket issuing (the KDC's half),
//! for tests: nextnfs has no KDC of its own and never initiates. It
//! issues a service ticket directly from the service's keytab key,
//! builds the AP-REQ a client's `gss_init_sec_context` would send, and
//! checks the acceptor's AP-REP.

use rasn::types::{BitString, GeneralString, OctetString};
use rasn_kerberos::{
    ApOptions, ApRep, ApReq, Authenticator, Checksum, EncApRepPart, EncTicketPart, EncryptedData, EncryptionKey, PrincipalName,
    Ticket, TicketFlags, TransitedEncoding,
};

use crate::acceptor::{kerberos_time, GSS_C_MUTUAL_FLAG, KU_AP_REP_ENCPART, KU_AP_REQ_AUTH, KU_TICKET};
use crate::cfx::ContextKey;
use crate::crypto::{self, Enctype};
use crate::keytab::KeytabEntry;
use crate::{token, Error};

fn gs(s: &str) -> GeneralString {
    GeneralString::from_bytes(s.as_bytes()).expect("ASCII principal")
}

fn principal(name: &str) -> PrincipalName {
    let parts: Vec<GeneralString> = name.split('/').map(gs).collect();
    PrincipalName { r#type: if parts.len() > 1 { 2 } else { 1 }, string: parts }
}

fn der<T: rasn::Encode>(v: &T) -> Vec<u8> {
    rasn::der::encode(v).expect("DER encode")
}

/// A service ticket and its session key, as a client holds them after a
/// TGS exchange.
pub struct Credential {
    pub client: String,
    pub client_realm: String,
    pub ticket: Ticket,
    pub session_enctype: Enctype,
    pub session_key: Vec<u8>,
}

/// Issues a ticket for the service in `service_key` to `client` (`name`
/// or `name/instance`, same realm), valid from `start` to `end` (epoch
/// seconds), with a session key of `session_enctype`.
pub fn issue_ticket(service_key: &KeytabEntry, client: &str, session_enctype: Enctype, start: i64, end: i64) -> Result<Credential, Error> {
    let etype = service_key.enctype().ok_or(Error::UnsupportedEnctype(service_key.enctype))?;
    let session_key = crypto::random_bytes(session_enctype.key_len())?;
    let part = EncTicketPart {
        flags: TicketFlags(BitString::repeat(false, 32)),
        key: EncryptionKey { r#type: session_enctype.number(), value: OctetString::from(session_key.clone()) },
        crealm: gs(&service_key.realm),
        cname: principal(client),
        transited: TransitedEncoding { r#type: 1, contents: OctetString::new() },
        auth_time: kerberos_time(start),
        start_time: Some(kerberos_time(start)),
        end_time: kerberos_time(end),
        renew_till: None,
        caddr: None,
        authorization_data: None,
    };
    let cipher = crypto::encrypt(etype, &service_key.key, KU_TICKET, &der(&part))?;
    let ticket = Ticket {
        tkt_vno: 5.into(),
        realm: gs(&service_key.realm),
        sname: PrincipalName {
            r#type: service_key.name_type as i32,
            string: service_key.components.iter().map(|c| gs(c)).collect(),
        },
        enc_part: EncryptedData { etype: etype.number(), kvno: Some(service_key.kvno), cipher: cipher.into() },
    };
    Ok(Credential { client: client.into(), client_realm: service_key.realm.clone(), ticket, session_enctype, session_key })
}

/// The initiator's half of a context being established.
pub struct Initiator {
    pub key: ContextKey,
    pub seq: u64,
    ctime: i64,
    session_enctype: Enctype,
    session_key: Vec<u8>,
}

impl Initiator {
    /// Builds the framed AP-REQ token. `subkey` picks the enctype of an
    /// initiator subkey (what MIT and Heimdal send); `None` uses the
    /// session key. `ctime` is the authenticator time (epoch seconds).
    pub fn start(cred: &Credential, flags: u32, subkey: Option<Enctype>, ctime: i64) -> Result<(Initiator, Vec<u8>), Error> {
        let mut cksum = vec![0u8; 24];
        cksum[..4].copy_from_slice(&16u32.to_le_bytes());
        cksum[20..24].copy_from_slice(&flags.to_le_bytes());
        let key = match subkey {
            Some(e) => ContextKey { enctype: e, key: crypto::random_bytes(e.key_len())?, acceptor_subkey: false },
            None => ContextKey { enctype: cred.session_enctype, key: cred.session_key.clone(), acceptor_subkey: false },
        };
        let seq = (u32::from_be_bytes(crypto::random_bytes(4)?.try_into().expect("4")) & 0x3fff_ffff) as u64;
        let auth = Authenticator {
            authenticator_vno: 5.into(),
            crealm: gs(&cred.client_realm),
            cname: principal(&cred.client),
            cksum: Some(Checksum { r#type: 0x8003, checksum: cksum.into() }),
            cusec: 123_456.into(),
            ctime: kerberos_time(ctime),
            subkey: subkey.map(|_| EncryptionKey { r#type: key.enctype.number(), value: OctetString::from(key.key.clone()) }),
            seq_number: Some(seq as u32),
            authorization_data: None,
        };
        let cipher = crypto::encrypt(cred.session_enctype, &cred.session_key, KU_AP_REQ_AUTH, &der(&auth))?;
        let mut options = BitString::repeat(false, 32);
        if flags & GSS_C_MUTUAL_FLAG != 0 {
            options.set(2, true);
        }
        let req = ApReq {
            pvno: 5.into(),
            msg_type: 14.into(),
            ap_options: ApOptions(options),
            ticket: cred.ticket.clone(),
            authenticator: EncryptedData { etype: cred.session_enctype.number(), kvno: None, cipher: cipher.into() },
        };
        let tok = token::build(token::TOK_ID_AP_REQ, &der(&req));
        Ok((Initiator { key, seq, ctime, session_enctype: cred.session_enctype, session_key: cred.session_key.clone() }, tok))
    }

    /// Checks the acceptor's AP-REP; returns the acceptor's first
    /// sequence number.
    pub fn finish(&mut self, ap_rep_token: &[u8]) -> Result<u64, Error> {
        let (id, msg) = token::parse(ap_rep_token)?;
        if id != token::TOK_ID_AP_REP {
            return Err(Error::Token("expected an AP-REP"));
        }
        let rep: ApRep = rasn::der::decode(msg).map_err(|e| Error::Asn1(e.to_string()))?;
        let plain = crypto::decrypt(self.session_enctype, &self.session_key, KU_AP_REP_ENCPART, &rep.enc_part.cipher)?;
        let part: EncApRepPart = rasn::der::decode(&plain).map_err(|e| Error::Asn1(e.to_string()))?;
        if part.ctime.0.timestamp() != self.ctime {
            return Err(Error::Rejected("AP-REP ctime does not echo the authenticator"));
        }
        if let Some(sk) = part.subkey {
            self.key = ContextKey {
                enctype: Enctype::from_i32(sk.r#type).ok_or(Error::UnsupportedEnctype(sk.r#type))?,
                key: sk.value.to_vec(),
                acceptor_subkey: true,
            };
        }
        Ok(part.seq_number.unwrap_or(0) as u64)
    }
}
