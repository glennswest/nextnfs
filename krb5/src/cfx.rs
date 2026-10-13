//! RFC 4121 §4.2 per-message tokens: MIC (`GSS_GetMIC`) and Wrap
//! (`GSS_Wrap`, integrity-only or sealed), for both directions.
//!
//! Header (16 bytes): TOK_ID (04 04 MIC / 05 04 Wrap), flags
//! (SentByAcceptor 0x01, Sealed 0x02, AcceptorSubkey 0x04), filler 0xFF
//! (MIC: five bytes; Wrap: one byte, then EC and RRC), SND_SEQ (u64 BE).
//!
//! Key usages (RFC 4121 §2): acceptor seal 22 / sign 23, initiator
//! seal 24 / sign 25. Incoming tokens with a non-zero RRC (right
//! rotation) are accepted; outgoing ones use RRC 0. Sealed tokens are
//! sent with EC 0 (CTS needs no padding); a peer's EC filler is removed.

use crate::crypto::{self, ct_eq, Enctype};
use crate::Error;

pub const KG_USAGE_ACCEPTOR_SEAL: u32 = 22;
pub const KG_USAGE_ACCEPTOR_SIGN: u32 = 23;
pub const KG_USAGE_INITIATOR_SEAL: u32 = 24;
pub const KG_USAGE_INITIATOR_SIGN: u32 = 25;

const FLAG_SENT_BY_ACCEPTOR: u8 = 0x01;
const FLAG_SEALED: u8 = 0x02;
const FLAG_ACCEPTOR_SUBKEY: u8 = 0x04;

const TOK_MIC: [u8; 2] = [0x04, 0x04];
const TOK_WRAP: [u8; 2] = [0x05, 0x04];

/// The per-message key of an established context.
#[derive(Clone)]
pub struct ContextKey {
    pub enctype: Enctype,
    pub key: Vec<u8>,
    /// The acceptor asserted a subkey in its AP-REP (never true for
    /// contexts this crate accepts; set by initiators that got one).
    pub acceptor_subkey: bool,
}

impl std::fmt::Debug for ContextKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContextKey({:?})", self.enctype)
    }
}

/// Which side sends the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sender {
    Initiator,
    Acceptor,
}

impl Sender {
    fn flag(self) -> u8 {
        match self {
            Sender::Acceptor => FLAG_SENT_BY_ACCEPTOR,
            Sender::Initiator => 0,
        }
    }
    fn sign_usage(self) -> u32 {
        match self {
            Sender::Acceptor => KG_USAGE_ACCEPTOR_SIGN,
            Sender::Initiator => KG_USAGE_INITIATOR_SIGN,
        }
    }
    fn seal_usage(self) -> u32 {
        match self {
            Sender::Acceptor => KG_USAGE_ACCEPTOR_SEAL,
            Sender::Initiator => KG_USAGE_INITIATOR_SEAL,
        }
    }
}

impl ContextKey {
    fn flags(&self, sender: Sender) -> u8 {
        sender.flag() | if self.acceptor_subkey { FLAG_ACCEPTOR_SUBKEY } else { 0 }
    }

    fn check_flags(&self, flags: u8, sender: Sender) -> Result<(), Error> {
        if flags & FLAG_SENT_BY_ACCEPTOR != sender.flag() {
            return Err(Error::Token("token sent in the wrong direction"));
        }
        if (flags & FLAG_ACCEPTOR_SUBKEY != 0) != self.acceptor_subkey {
            return Err(Error::Token("AcceptorSubkey flag does not match the context"));
        }
        Ok(())
    }

    /// `GSS_GetMIC` of `message`, sent by `sender` with sequence `seq`.
    pub fn get_mic(&self, sender: Sender, seq: u64, message: &[u8]) -> Result<Vec<u8>, Error> {
        let mut header = [0xffu8; 16];
        header[..2].copy_from_slice(&TOK_MIC);
        header[2] = self.flags(sender);
        header[8..].copy_from_slice(&seq.to_be_bytes());
        let cksum = crypto::checksum(self.enctype, &self.key, sender.sign_usage(), &[message, &header])?;
        let mut out = header.to_vec();
        out.extend_from_slice(&cksum);
        Ok(out)
    }

    /// `GSS_VerifyMIC`: checks `token` (sent by `sender`) over `message`;
    /// returns the token's sequence number.
    pub fn verify_mic(&self, sender: Sender, message: &[u8], token: &[u8]) -> Result<u64, Error> {
        if token.len() < 16 || token[..2] != TOK_MIC || token[3..8] != [0xff; 5] {
            return Err(Error::Token("not a MIC token"));
        }
        self.check_flags(token[2], sender)?;
        let (header, cksum) = token.split_at(16);
        let want = crypto::checksum(self.enctype, &self.key, sender.sign_usage(), &[message, header])?;
        if !ct_eq(&want, cksum) {
            return Err(Error::Integrity);
        }
        Ok(u64::from_be_bytes(header[8..].try_into().expect("8")))
    }

    /// `GSS_Wrap` of `message`; `sealed` selects confidentiality.
    pub fn wrap(&self, sender: Sender, seq: u64, sealed: bool, message: &[u8]) -> Result<Vec<u8>, Error> {
        let mut header = [0u8; 16];
        header[..2].copy_from_slice(&TOK_WRAP);
        header[2] = self.flags(sender) | if sealed { FLAG_SEALED } else { 0 };
        header[3] = 0xff;
        header[8..].copy_from_slice(&seq.to_be_bytes());
        let mut out = Vec::with_capacity(16 + message.len() + 64);
        if sealed {
            // EC = 0, RRC = 0: E(message | header).
            let mut plain = Vec::with_capacity(message.len() + 16);
            plain.extend_from_slice(message);
            plain.extend_from_slice(&header);
            let body = crypto::encrypt(self.enctype, &self.key, sender.seal_usage(), &plain)?;
            out.extend_from_slice(&header);
            out.extend_from_slice(&body);
        } else {
            // Checksum over message | header with EC and RRC zero; the
            // sent header carries EC = checksum length.
            let cksum = crypto::checksum(self.enctype, &self.key, sender.seal_usage(), &[message, &header])?;
            header[4..6].copy_from_slice(&(cksum.len() as u16).to_be_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(message);
            out.extend_from_slice(&cksum);
        }
        Ok(out)
    }

    /// `GSS_Unwrap` of a token sent by `sender`; returns the message,
    /// whether it was sealed, and the sequence number.
    pub fn unwrap(&self, sender: Sender, token: &[u8]) -> Result<(Vec<u8>, bool, u64), Error> {
        if token.len() < 16 || token[..2] != TOK_WRAP || token[3] != 0xff {
            return Err(Error::Token("not a Wrap token"));
        }
        let flags = token[2];
        self.check_flags(flags, sender)?;
        let sealed = flags & FLAG_SEALED != 0;
        let ec = u16::from_be_bytes([token[4], token[5]]) as usize;
        let rrc = u16::from_be_bytes([token[6], token[7]]) as usize;
        let seq = u64::from_be_bytes(token[8..16].try_into().expect("8"));
        let rest = &token[16..];
        let data: Vec<u8> = if rest.is_empty() {
            Vec::new()
        } else {
            // Undo the right rotation.
            let r = rrc % rest.len();
            [&rest[r..], &rest[..r]].concat()
        };
        let mut header = [0u8; 16];
        header.copy_from_slice(&token[..16]);
        header[6..8].copy_from_slice(&[0, 0]);
        if sealed {
            let mut plain = crypto::decrypt(self.enctype, &self.key, sender.seal_usage(), &data)?;
            if plain.len() < 16 + ec {
                return Err(Error::Token("sealed Wrap token too short"));
            }
            let inner = plain.split_off(plain.len() - 16);
            if !ct_eq(&inner, &header) {
                return Err(Error::Integrity);
            }
            plain.truncate(plain.len() - ec);
            Ok((plain, true, seq))
        } else {
            let h = self.enctype.hmac_len();
            if ec != h || data.len() < h {
                return Err(Error::Token("bad Wrap checksum length"));
            }
            let (message, cksum) = data.split_at(data.len() - h);
            header[4..6].copy_from_slice(&[0, 0]);
            let want = crypto::checksum(self.enctype, &self.key, sender.seal_usage(), &[message, &header])?;
            if !ct_eq(&want, cksum) {
                return Err(Error::Integrity);
            }
            Ok((message.to_vec(), false, seq))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<ContextKey> {
        Enctype::ALL
            .into_iter()
            .map(|e| ContextKey { enctype: e, key: crypto::random_bytes(e.key_len()).unwrap(), acceptor_subkey: false })
            .collect()
    }

    #[test]
    fn mic_roundtrip_and_direction() {
        for k in keys() {
            let tok = k.get_mic(Sender::Acceptor, 42, b"hello").unwrap();
            assert_eq!(tok.len(), 16 + k.enctype.hmac_len());
            assert_eq!(k.verify_mic(Sender::Acceptor, b"hello", &tok).unwrap(), 42);
            assert!(k.verify_mic(Sender::Acceptor, b"hellO", &tok).is_err());
            assert!(k.verify_mic(Sender::Initiator, b"hello", &tok).is_err());
            let tok = k.get_mic(Sender::Initiator, 7, b"").unwrap();
            assert_eq!(k.verify_mic(Sender::Initiator, b"", &tok).unwrap(), 7);
        }
    }

    #[test]
    fn wrap_roundtrip_both_modes() {
        for k in keys() {
            for sealed in [false, true] {
                for len in [0usize, 1, 4, 100, 1000] {
                    let msg: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
                    let tok = k.wrap(Sender::Initiator, 9, sealed, &msg).unwrap();
                    let (back, s, seq) = k.unwrap(Sender::Initiator, &tok).unwrap();
                    assert_eq!((back, s, seq), (msg.clone(), sealed, 9));
                    if sealed && len >= 16 {
                        assert!(!tok.windows(len.min(16)).any(|w| w == &msg[..len.min(16)]), "sealed data is not in the clear");
                    }
                    assert!(k.unwrap(Sender::Acceptor, &tok).is_err(), "direction");
                    let mut bad = tok.clone();
                    let i = bad.len() - 1;
                    bad[i] ^= 0x80;
                    assert!(k.unwrap(Sender::Initiator, &bad).is_err(), "tamper");
                }
            }
        }
    }

    #[test]
    fn unwrap_handles_rotation_and_ec_filler() {
        for k in keys() {
            for sealed in [false, true] {
                let msg = b"rotated message body".to_vec();
                let mut tok = k.wrap(Sender::Initiator, 1, sealed, &msg).unwrap();
                let rrc = 28usize;
                let rest = tok[16..].to_vec();
                let n = rest.len();
                for (i, b) in rest.iter().enumerate() {
                    tok[16 + (i + rrc) % n] = *b;
                }
                tok[6..8].copy_from_slice(&(rrc as u16).to_be_bytes());
                assert_eq!(k.unwrap(Sender::Initiator, &tok).unwrap().0, msg);
            }
            // A sealed token with EC = 3 filler bytes (as some senders use).
            let mut header = [0u8; 16];
            header[..2].copy_from_slice(&TOK_WRAP);
            header[2] = FLAG_SEALED;
            header[3] = 0xff;
            header[4..6].copy_from_slice(&3u16.to_be_bytes());
            header[15] = 5;
            let mut plain = b"data".to_vec();
            plain.extend_from_slice(&[0xee; 3]);
            plain.extend_from_slice(&header);
            let body = crypto::encrypt(k.enctype, &k.key, KG_USAGE_INITIATOR_SEAL, &plain).unwrap();
            let tok = [&header[..], &body].concat();
            assert_eq!(k.unwrap(Sender::Initiator, &tok).unwrap(), (b"data".to_vec(), true, 5));
        }
    }

    #[test]
    fn subkey_flag_must_match() {
        let k = &keys()[1];
        let mut other = k.clone();
        other.acceptor_subkey = true;
        let tok = other.get_mic(Sender::Acceptor, 1, b"x").unwrap();
        assert!(k.verify_mic(Sender::Acceptor, b"x", &tok).is_err());
        assert!(other.verify_mic(Sender::Acceptor, b"x", &tok).is_ok());
    }
}
