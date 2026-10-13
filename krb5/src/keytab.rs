//! MIT keytab files (format version 0x0502, what `ktutil`, `kadmin` and
//! irondirectory's `iron-kdc-ctl` write). Version 0x0501 (host byte
//! order, pre-1998) is refused.
//!
//! Layout: `05 02`, then entries, each `i32 size` (negative = a deleted
//! slot of that many bytes) and a body: `u16 n_components`, realm and
//! each component as `u16 len + bytes`, `u32 name_type`, `u32 timestamp`,
//! `u8 kvno`, `u16 enctype`, `u16 keylen + key`, optionally `u32 kvno`
//! (overrides the 8-bit one when present and non-zero).

use std::path::Path;

use crate::crypto::Enctype;
use crate::Error;

#[derive(Clone)]
pub struct KeytabEntry {
    pub realm: String,
    pub components: Vec<String>,
    pub name_type: u32,
    pub timestamp: u32,
    pub kvno: u32,
    /// Raw enctype number; entries with enctypes this crate does not
    /// implement are kept but never match a ticket.
    pub enctype: i32,
    pub key: Vec<u8>,
}

impl std::fmt::Debug for KeytabEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        write!(f, "{}@{} kvno {} enctype {}", self.components.join("/"), self.realm, self.kvno, self.enctype)
    }
}

impl KeytabEntry {
    /// `primary/instance@REALM`.
    pub fn principal(&self) -> String {
        format!("{}@{}", self.components.join("/"), self.realm)
    }

    pub fn enctype(&self) -> Option<Enctype> {
        Enctype::from_i32(self.enctype)
    }
}

/// A parsed keytab.
#[derive(Clone, Debug, Default)]
pub struct Keytab {
    pub entries: Vec<KeytabEntry>,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.buf.len()).ok_or(Error::Keytab("truncated entry".into()))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().expect("2")))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("4")))
    }
    fn data(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u16()? as usize;
        self.take(n)
    }
    fn string(&mut self) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(self.data()?).into_owned())
    }
}

impl Keytab {
    pub fn load(path: &Path) -> Result<Keytab, Error> {
        let bytes = std::fs::read(path).map_err(|e| Error::Keytab(format!("{}: {}", path.display(), e)))?;
        Keytab::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Keytab, Error> {
        match bytes.get(..2) {
            Some([5, 2]) => {}
            Some([5, 1]) => return Err(Error::Keytab("keytab version 0x0501 is not supported".into())),
            _ => return Err(Error::Keytab("not a keytab (bad magic)".into())),
        }
        let mut entries = Vec::new();
        let mut pos = 2;
        while pos + 4 <= bytes.len() {
            let size = i32::from_be_bytes(bytes[pos..pos + 4].try_into().expect("4"));
            pos += 4;
            let len = size.unsigned_abs() as usize;
            if len == 0 {
                break;
            }
            let body = bytes.get(pos..pos + len).ok_or(Error::Keytab("truncated entry".into()))?;
            pos += len;
            if size < 0 {
                continue; // deleted slot
            }
            let mut r = Reader { buf: body, pos: 0 };
            let n = r.u16()? as usize;
            let realm = r.string()?;
            let components = (0..n).map(|_| r.string()).collect::<Result<Vec<_>, _>>()?;
            let name_type = r.u32()?;
            let timestamp = r.u32()?;
            let kvno8 = r.u8()?;
            let enctype = r.u16()? as i32;
            let key = r.data()?.to_vec();
            let kvno = match r.u32() {
                Ok(v) if v != 0 => v,
                _ => kvno8 as u32,
            };
            entries.push(KeytabEntry { realm, components, name_type, timestamp, kvno, enctype, key });
        }
        Ok(Keytab { entries })
    }

    /// Encodes the keytab (format 0x0502). Used by tests and tooling.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![5u8, 2];
        for e in &self.entries {
            let mut body = Vec::new();
            body.extend_from_slice(&(e.components.len() as u16).to_be_bytes());
            for s in std::iter::once(&e.realm).chain(&e.components) {
                body.extend_from_slice(&(s.len() as u16).to_be_bytes());
                body.extend_from_slice(s.as_bytes());
            }
            body.extend_from_slice(&e.name_type.to_be_bytes());
            body.extend_from_slice(&e.timestamp.to_be_bytes());
            body.push(e.kvno as u8);
            body.extend_from_slice(&(e.enctype as u16).to_be_bytes());
            body.extend_from_slice(&(e.key.len() as u16).to_be_bytes());
            body.extend_from_slice(&e.key);
            body.extend_from_slice(&e.kvno.to_be_bytes());
            out.extend_from_slice(&(body.len() as i32).to_be_bytes());
            out.extend_from_slice(&body);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kvno: u32, enctype: i32) -> KeytabEntry {
        KeytabEntry {
            realm: "EXAMPLE.LO".into(),
            components: vec!["nfs".into(), "server.example.lo".into()],
            name_type: 2,
            timestamp: 1_700_000_000,
            kvno,
            enctype,
            key: vec![0x11; 32],
        }
    }

    #[test]
    fn roundtrip_and_layout() {
        let kt = Keytab { entries: vec![entry(3, 18), entry(300, 20)] };
        let bytes = kt.to_bytes();
        assert_eq!(&bytes[..2], &[5, 2]);
        let back = Keytab::parse(&bytes).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].principal(), "nfs/server.example.lo@EXAMPLE.LO");
        assert_eq!(back.entries[0].kvno, 3);
        assert_eq!(back.entries[1].kvno, 300, "32-bit kvno overrides the 8-bit one");
        assert_eq!(back.entries[1].enctype(), Some(Enctype::Aes256CtsHmacSha384_192));
        assert_eq!(back.entries[0].key, vec![0x11; 32]);
    }

    #[test]
    fn entry_without_32bit_kvno_and_deleted_slots() {
        let mut bytes = Keytab { entries: vec![entry(7, 17)] }.to_bytes();
        // Drop the trailing 32-bit kvno: shrink the entry by 4.
        let size = i32::from_be_bytes(bytes[2..6].try_into().unwrap()) - 4;
        bytes[2..6].copy_from_slice(&size.to_be_bytes());
        bytes.truncate(bytes.len() - 4);
        // Append a deleted 8-byte slot.
        bytes.extend_from_slice(&(-8i32).to_be_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        let kt = Keytab::parse(&bytes).unwrap();
        assert_eq!(kt.entries.len(), 1);
        assert_eq!(kt.entries[0].kvno, 7);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Keytab::parse(&[]).is_err());
        assert!(Keytab::parse(&[5, 1, 0, 0]).is_err());
        assert!(Keytab::parse(&[1, 2]).is_err());
        let bytes = Keytab { entries: vec![entry(1, 18)] }.to_bytes();
        assert!(Keytab::parse(&bytes[..bytes.len() - 10]).is_err());
        assert!(!format!("{:?}", entry(1, 18)).contains("17"), "no key bytes in Debug");
    }
}
