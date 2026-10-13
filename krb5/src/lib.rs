//! Kerberos V5 GSS-API acceptor for nextnfs's RPCSEC_GSS (#106).
//!
//! Pure Rust (RustCrypto + `rasn-kerberos`), so the nextnfs golden stays
//! one static musl binary. Covers what an NFS server needs: the AES
//! enctypes ([`crypto`]), keytabs ([`keytab`]), accepting an AP-REQ and
//! answering with an AP-REP ([`acceptor`]), and RFC 4121 MIC and Wrap
//! tokens ([`cfx`]). [`initiator`] is the client side, for tests.
//!
//! Not supported: DES/RC4 enctypes, delegation (KRB-CRED), channel
//! bindings (RPCSEC_GSS does not use them), user-to-user tickets.

pub mod acceptor;
pub mod cfx;
pub mod crypto;
pub mod initiator;
pub mod keytab;
pub mod token;

pub use acceptor::{Accepted, Acceptor};
pub use cfx::{ContextKey, Sender};
pub use crypto::Enctype;
pub use keytab::{Keytab, KeytabEntry};

#[derive(Debug)]
pub enum Error {
    /// Malformed GSS token.
    Token(&'static str),
    /// DER encode/decode failure.
    Asn1(String),
    /// Checksum or MAC did not verify (wrong key, tampered data).
    Integrity,
    /// Enctype this crate does not implement.
    UnsupportedEnctype(i32),
    /// No keytab key for the ticket's service principal and enctype.
    NoKey(String),
    /// Authentication refused (expired ticket, skew, replay, ...).
    Rejected(&'static str),
    /// Keytab file problem.
    Keytab(String),
    /// Crypto primitive failure.
    Crypto(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Token(m) => write!(f, "GSS token: {}", m),
            Error::Asn1(m) => write!(f, "Kerberos DER: {}", m),
            Error::Integrity => write!(f, "integrity check failed"),
            Error::UnsupportedEnctype(e) => write!(f, "unsupported enctype {}", e),
            Error::NoKey(p) => write!(f, "no keytab key for {}", p),
            Error::Rejected(m) => write!(f, "rejected: {}", m),
            Error::Keytab(m) => write!(f, "keytab: {}", m),
            Error::Crypto(m) => write!(f, "crypto: {}", m),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::acceptor::{CLOCK_SKEW_SECS, GSS_C_MUTUAL_FLAG};
    use super::initiator::{issue_ticket, Initiator};
    use super::*;

    fn service_entry(enctype: Enctype, kvno: u32) -> KeytabEntry {
        KeytabEntry {
            realm: "EXAMPLE.LO".into(),
            components: vec!["nfs".into(), "nfs1.example.lo".into()],
            name_type: 3,
            timestamp: 0,
            kvno,
            enctype: enctype.number(),
            key: crypto::random_bytes(enctype.key_len()).unwrap(),
        }
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn accept_every_enctype_combination_with_mutual_auth() {
        for tkt_e in Enctype::ALL {
            for sess_e in Enctype::ALL {
                for sub_e in [None, Some(Enctype::Aes256CtsHmacSha1_96), Some(Enctype::Aes128CtsHmacSha256_128)] {
                    let svc = service_entry(tkt_e, 2);
                    let acc = Acceptor::new(Keytab { entries: vec![service_entry(tkt_e, 1), svc.clone()] });
                    let cred = issue_ticket(&svc, "alice", sess_e, now() - 60, now() + 3600).unwrap();
                    let (mut ini, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG | 32, sub_e, now()).unwrap();
                    let ctx = acc.accept(&tok).unwrap();
                    assert_eq!(ctx.client, "alice@EXAMPLE.LO");
                    assert_eq!(ctx.service, "nfs/nfs1.example.lo@EXAMPLE.LO");
                    assert_eq!(ctx.key.enctype, sub_e.unwrap_or(sess_e));
                    assert_eq!(ctx.key.key, ini.key.key);
                    assert_eq!(ctx.initiator_seq, ini.seq);
                    let acc_seq = ini.finish(ctx.output_token.as_ref().unwrap()).unwrap();
                    assert_eq!(acc_seq, ctx.acceptor_seq);
                    // Both sides now share a working per-message key.
                    let mic = ctx.key.get_mic(Sender::Acceptor, acc_seq, b"window").unwrap();
                    assert_eq!(ini.key.verify_mic(Sender::Acceptor, b"window", &mic).unwrap(), acc_seq);
                    let w = ini.key.wrap(Sender::Initiator, ini.seq, true, b"args").unwrap();
                    assert_eq!(ctx.key.unwrap(Sender::Initiator, &w).unwrap().0, b"args");
                }
            }
        }
    }

    #[test]
    fn no_output_token_without_mutual_flag() {
        let svc = service_entry(Enctype::Aes256CtsHmacSha1_96, 1);
        let acc = Acceptor::new(Keytab { entries: vec![svc.clone()] });
        let cred = issue_ticket(&svc, "bob", Enctype::Aes256CtsHmacSha1_96, now(), now() + 60).unwrap();
        let (ini, tok) = Initiator::start(&cred, 0, None, now()).unwrap();
        let ctx = acc.accept(&tok).unwrap();
        assert!(ctx.output_token.is_none());
        assert_eq!(ctx.acceptor_seq, ini.seq);
    }

    #[test]
    fn refuses_bad_tickets() {
        let svc = service_entry(Enctype::Aes256CtsHmacSha384_192, 1);
        let acc = Acceptor::new(Keytab { entries: vec![svc.clone()] });
        let e = Enctype::Aes256CtsHmacSha384_192;

        // Expired, not yet valid, skewed authenticator.
        let cred = issue_ticket(&svc, "u", e, now() - 7200, now() - 3600).unwrap();
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now()).unwrap();
        assert!(matches!(acc.accept(&tok), Err(Error::Rejected("ticket expired"))));
        let cred = issue_ticket(&svc, "u", e, now() + 3600, now() + 7200).unwrap();
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now()).unwrap();
        assert!(matches!(acc.accept(&tok), Err(Error::Rejected("ticket not yet valid"))));
        let cred = issue_ticket(&svc, "u", e, now(), now() + 3600).unwrap();
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now() - CLOCK_SKEW_SECS - 10).unwrap();
        assert!(matches!(acc.accept(&tok), Err(Error::Rejected("clock skew too great"))));

        // Replay of the same AP-REQ.
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now()).unwrap();
        assert!(acc.accept(&tok).is_ok());
        assert!(matches!(acc.accept(&tok), Err(Error::Rejected("authenticator replayed"))));

        // Ticket for a key the keytab does not hold (other key bytes).
        let mut other = svc.clone();
        other.key = crypto::random_bytes(32).unwrap();
        let cred = issue_ticket(&other, "u", e, now(), now() + 3600).unwrap();
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now()).unwrap();
        assert!(matches!(acc.accept(&tok), Err(Error::Integrity)));

        // Ticket for another service principal.
        let mut host = svc.clone();
        host.components = vec!["host".into(), "x".into()];
        let cred = issue_ticket(&host, "u", e, now(), now() + 3600).unwrap();
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now()).unwrap();
        assert!(matches!(acc.accept(&tok), Err(Error::NoKey(_))));

        // Garbage and truncation.
        assert!(acc.accept(&[0x60, 0x01, 0x00]).is_err());
        let cred = issue_ticket(&svc, "u", e, now(), now() + 3600).unwrap();
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now()).unwrap();
        assert!(acc.accept(&tok[..tok.len() - 5]).is_err());
    }

    #[test]
    fn keytab_file_roundtrip_through_acceptor() {
        let svc = service_entry(Enctype::Aes128CtsHmacSha1_96, 4);
        let bytes = Keytab { entries: vec![svc.clone()] }.to_bytes();
        let acc = Acceptor::new(Keytab::parse(&bytes).unwrap());
        let cred = issue_ticket(&svc, "carol/admin", Enctype::Aes128CtsHmacSha1_96, now(), now() + 60).unwrap();
        let (_, tok) = Initiator::start(&cred, GSS_C_MUTUAL_FLAG, None, now()).unwrap();
        assert_eq!(acc.accept(&tok).unwrap().client, "carol/admin@EXAMPLE.LO");
    }
}
