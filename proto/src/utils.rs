use std::{
    fmt,
    ops::{Deref, DerefMut},
};

use num_traits::{FromPrimitive, ToPrimitive};
use serde::{
    de::{self, SeqAccess, Visitor},
    ser::{SerializeSeq, SerializeStruct},
    Deserialize, Serialize, Serializer,
};
use tracing::debug;

use crate::nfs4_proto::Compound4args;

use super::{
    nfs4_proto::{Attrlist4, Fattr4, FileAttr, FileAttrValue, Getattr4resok, NfsResOp4, NfsStat4},
    rpc_proto::{AuthUnix, CallBody, OpaqueAuth},
};

pub fn write_argarray<T, S>(v: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: AsRef<[NfsResOp4]>,
    S: Serializer,
{
    let values = v.as_ref();
    if values.is_empty() {
        serializer.serialize_none()
    } else {
        values.serialize(serializer)
    }
}

impl Serialize for NfsStat4 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ToPrimitive::to_u32(self).unwrap().serialize(serializer)
    }
}

impl Serialize for Getattr4resok {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.status != NfsStat4::Nfs4Ok {
            debug!("status != NfsStat4::Nfs4Ok: {:?}", self.status);
            let mut seq = serializer.serialize_struct("Getattr4resok", 1)?;
            seq.serialize_field("status", &ToPrimitive::to_u32(&self.status).unwrap())?;
            seq.end()
        } else if let Some(ref attrs) = self.obj_attributes {
            let mut seq = serializer.serialize_struct("Getattr4resok", 2)?;
            seq.serialize_field("status", &ToPrimitive::to_u32(&self.status).unwrap())?;
            seq.serialize_field("obj_attributes", attrs)?;
            seq.end()
        } else {
            // Status is Ok but no attributes — serialize as empty response
            let mut seq = serializer.serialize_struct("Getattr4resok", 1)?;
            seq.serialize_field("status", &ToPrimitive::to_u32(&self.status).unwrap())?;
            seq.end()
        }
    }
}

impl<'de> Deserialize<'de> for CallBody {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CallBodyVisitor;

        impl<'de> Visitor<'de> for CallBodyVisitor {
            type Value = CallBody;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct CallBody")
            }

            fn visit_seq<V>(self, mut seq: V) -> Result<CallBody, V::Error>
            where
                V: SeqAccess<'de>,
            {
                let rpcvers = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let prog = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let vers = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let proc = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let cred = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let verf = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                // if proc == 0, then there are no args
                if proc == 0 {
                    // Procedure 0: NULL - No Operation
                    Ok(CallBody {
                        rpcvers,
                        prog,
                        vers,
                        proc,
                        cred,
                        verf,
                        args: None,
                    })
                } else {
                    // Procedure 1: COMPOUND - Compound Operations
                    let args: Compound4args = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                    Ok(CallBody {
                        rpcvers,
                        prog,
                        vers,
                        proc,
                        cred,
                        verf,
                        args: Some(args),
                    })
                }
            }
        }

        const FIELDS: &[&str] = &["rpcvers", "prog", "vers", "proc", "cred", "verf", "args"];
        deserializer.deserialize_struct("CallBody", FIELDS, CallBodyVisitor)
    }
}

/// Custom serializer for OpaqueAuth.
///
/// RFC 5531 opaque_auth = flavor (u32) + opaque body (u32 length + data + padding).
/// We serialize by writing the flavor, then the body as opaque bytes.
impl Serialize for OpaqueAuth {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (flavor, body_bytes) = match self {
            OpaqueAuth::AuthNull(data) => (0u32, data.clone()),
            OpaqueAuth::AuthUnix(auth) => {
                let mut bytes = Vec::new();
                serde_xdr::to_writer(&mut bytes, auth)
                    .map_err(serde::ser::Error::custom)?;
                (1u32, bytes)
            }
            OpaqueAuth::AuthShort => (2u32, Vec::new()),
            OpaqueAuth::AuthDes => (3u32, Vec::new()),
        };

        let mut seq = serializer.serialize_struct("OpaqueAuth", 2)?;
        seq.serialize_field("flavor", &flavor)?;
        seq.serialize_field("body", &serde_bytes::Bytes::new(&body_bytes))?;
        seq.end()
    }
}

/// Custom deserializer for OpaqueAuth.
///
/// RFC 5531 defines opaque_auth as:
///   struct opaque_auth {
///       auth_flavor flavor;
///       opaque body<400>;
///   };
///
/// The body is a variable-length opaque containing the serialized auth
/// credentials. serde_xdr's enum deserialization would treat this as a
/// discriminated union (reading variant fields directly), but the wire
/// format wraps the body in an XDR opaque (u32 length + data + padding).
/// We must read the opaque wrapper and then parse the body by flavor.
impl<'de> Deserialize<'de> for OpaqueAuth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct OpaqueAuthVisitor;

        impl<'de> Visitor<'de> for OpaqueAuthVisitor {
            type Value = OpaqueAuth;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct opaque_auth (flavor + opaque body)")
            }

            fn visit_seq<V>(self, mut seq: V) -> Result<OpaqueAuth, V::Error>
            where
                V: SeqAccess<'de>,
            {
                // Read flavor (u32)
                let flavor: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;

                // Read body as opaque bytes (XDR variable-length opaque)
                let body: serde_bytes::ByteBuf = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;

                match flavor {
                    0 => {
                        // AUTH_NULL — body is typically empty
                        Ok(OpaqueAuth::AuthNull(body.into_vec()))
                    }
                    1 => {
                        // AUTH_SYS — parse body as AuthUnix (authsys_parms)
                        let body_bytes = body.into_vec();
                        let mut cursor = std::io::Cursor::new(&body_bytes);
                        let auth: AuthUnix =
                            serde_xdr::from_reader(&mut cursor).map_err(|e| {
                                de::Error::custom(format!(
                                    "failed to parse AUTH_SYS body: {:?}",
                                    e
                                ))
                            })?;
                        Ok(OpaqueAuth::AuthUnix(auth))
                    }
                    _ => {
                        // Unsupported auth flavor — store as raw bytes in AuthNull
                        debug!("unsupported auth flavor {}, treating as null", flavor);
                        Ok(OpaqueAuth::AuthNull(body.into_vec()))
                    }
                }
            }
        }

        const FIELDS: &[&str] = &["flavor", "body"];
        deserializer.deserialize_struct("OpaqueAuth", FIELDS, OpaqueAuthVisitor)
    }
}

// deserialization helper for Fattr4
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FattrRaw {
    attrmask: Vec<u32>,
    #[serde(with = "serde_bytes")]
    attr_vals: Vec<u8>,
}
impl FattrRaw {
    fn to_fileattrs(&self) -> Attrlist4<FileAttr> {
        let mut attrmask = Attrlist4::<FileAttr>::new(None);
        for (idx, segment) in self.attrmask.iter().enumerate() {
            for n in 0..32 {
                let bit = (segment >> n) & 1;
                if bit == 1 {
                    let attr: Option<FileAttr> = FromPrimitive::from_u32((idx * 32 + n) as u32);
                    if let Some(attr) = attr {
                        attrmask.push(attr);
                    }
                }
            }
        }
        attrmask
    }

    fn attrvalues_from_bytes(&self, fileattrs: &[FileAttr]) -> Attrlist4<FileAttrValue> {
        let mut attr_vals = Attrlist4::<FileAttrValue>::new(None);
        let mut offset = 0;
        let buf = &self.attr_vals;
        for attr in fileattrs.iter() {
            if offset > buf.len() {
                break;
            }
            match attr {
                FileAttr::Type => {
                    if offset + 4 > buf.len() { break; }
                    let val = u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap());
                    use crate::nfs4_proto::NfsFtype4;
                    let ftype = match val {
                        1 => NfsFtype4::Nf4reg,
                        2 => NfsFtype4::Nf4dir,
                        3 => NfsFtype4::Nf4blk,
                        4 => NfsFtype4::Nf4chr,
                        5 => NfsFtype4::Nf4lnk,
                        6 => NfsFtype4::Nf4sock,
                        7 => NfsFtype4::Nf4fifo,
                        8 => NfsFtype4::Nf4attrdir,
                        9 => NfsFtype4::Nf4namedattr,
                        _ => NfsFtype4::Nf4Undef,
                    };
                    attr_vals.push(FileAttrValue::Type(ftype));
                    offset += 4;
                }
                FileAttr::Change => {
                    if offset + 8 > buf.len() { break; }
                    let val = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
                    attr_vals.push(FileAttrValue::Change(val));
                    offset += 8;
                }
                FileAttr::Size => {
                    if offset + 8 > buf.len() { break; }
                    let val = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
                    attr_vals.push(FileAttrValue::Size(val));
                    offset += 8;
                }
                FileAttr::Mode => {
                    if offset + 4 > buf.len() { break; }
                    let val = u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap());
                    attr_vals.push(FileAttrValue::Mode(val));
                    offset += 4;
                }
                FileAttr::Numlinks => {
                    if offset + 4 > buf.len() { break; }
                    let val = u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap());
                    attr_vals.push(FileAttrValue::Numlinks(val));
                    offset += 4;
                }
                FileAttr::SpaceUsed => {
                    if offset + 8 > buf.len() { break; }
                    let val = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
                    attr_vals.push(FileAttrValue::SpaceUsed(val));
                    offset += 8;
                }
                FileAttr::MountedOnFileid => {
                    if offset + 8 > buf.len() { break; }
                    let val = u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
                    attr_vals.push(FileAttrValue::MountedOnFileid(val));
                    offset += 8;
                }
                FileAttr::TimeAccess => {
                    if offset + 12 > buf.len() { break; }
                    let seconds = i64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
                    let nseconds = u32::from_be_bytes(buf[offset + 8..offset + 12].try_into().unwrap());
                    attr_vals.push(FileAttrValue::TimeAccess(crate::nfs4_proto::Nfstime4 { seconds, nseconds }));
                    offset += 12;
                }
                FileAttr::TimeModify => {
                    if offset + 12 > buf.len() { break; }
                    let seconds = i64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
                    let nseconds = u32::from_be_bytes(buf[offset + 8..offset + 12].try_into().unwrap());
                    attr_vals.push(FileAttrValue::TimeModify(crate::nfs4_proto::Nfstime4 { seconds, nseconds }));
                    offset += 12;
                }
                FileAttr::TimeMetadata => {
                    if offset + 12 > buf.len() { break; }
                    let seconds = i64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap());
                    let nseconds = u32::from_be_bytes(buf[offset + 8..offset + 12].try_into().unwrap());
                    attr_vals.push(FileAttrValue::TimeMetadata(crate::nfs4_proto::Nfstime4 { seconds, nseconds }));
                    offset += 12;
                }
                FileAttr::Owner => {
                    if offset + 4 > buf.len() { break; }
                    let len = u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    if offset + len > buf.len() { break; }
                    let val = String::from_utf8_lossy(&buf[offset..offset + len]).to_string();
                    attr_vals.push(FileAttrValue::Owner(val));
                    offset += len + (4 - (len % 4)) % 4; // XDR padding to 4-byte boundary
                }
                FileAttr::OwnerGroup => {
                    if offset + 4 > buf.len() { break; }
                    let len = u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    if offset + len > buf.len() { break; }
                    let val = String::from_utf8_lossy(&buf[offset..offset + len]).to_string();
                    attr_vals.push(FileAttrValue::OwnerGroup(val));
                    offset += len + (4 - (len % 4)) % 4; // XDR padding to 4-byte boundary
                }
                _ => {
                    // Unknown attribute — can't determine wire size, stop parsing
                    debug!("skipping unhandled attr {:?} in SETATTR deserialization", attr);
                    break;
                }
            }
        }
        attr_vals
    }
}

impl<'de> Deserialize<'de> for Fattr4 {
    fn deserialize<D>(deserializer: D) -> Result<Fattr4, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fattr_raw = <FattrRaw as serde::Deserialize>::deserialize(deserializer)?;
        let attrmask = fattr_raw.to_fileattrs();
        let attr_vals = fattr_raw.attrvalues_from_bytes(&attrmask);

        Ok(Fattr4 {
            attrmask,
            attr_vals,
        })
    }
}

impl<T> Deref for Attrlist4<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Attrlist4<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Iterator for Attrlist4<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop()
    }
}

impl Attrlist4<FileAttr> {
    pub fn new(list: Option<Vec<FileAttr>>) -> Self {
        match list {
            Some(list) => Self(list),
            None => Self(Vec::new()),
        }
    }
    fn file_attrs_to_bitmap(&self) -> Result<Vec<u32>, anyhow::Error> {
        let mut attrs = Vec::new();
        let mut idxs = self
            .iter()
            .map(|attr| ToPrimitive::to_u32(attr).unwrap())
            .collect::<Vec<u32>>();

        idxs.reverse();
        let mut segment = 0_u32;
        while let Some(idx) = idxs.pop() {
            if (idx.div_ceil(31) as i16) - 1 > attrs.len() as i16 {
                attrs.push(segment);
                segment = 0_u32;
            }
            segment += 2_u32.pow(idx % 32);
        }
        attrs.push(segment);

        Ok(attrs)
    }

    pub fn from_u32(raw: Vec<u32>) -> Attrlist4<FileAttr> {
        let mut attrmask = Attrlist4::<FileAttr>::new(None);
        for (idx, segment) in raw.iter().enumerate() {
            for n in 0..32 {
                let bit = (segment >> n) & 1;
                if bit == 1 {
                    let attr: Option<FileAttr> = FromPrimitive::from_u32((idx * 32 + n) as u32);
                    if let Some(attr) = attr {
                        attrmask.push(attr);
                    }
                }
            }
        }
        attrmask
    }
}

impl Attrlist4<FileAttrValue> {
    pub fn new(list: Option<Vec<FileAttrValue>>) -> Self {
        match list {
            Some(list) => Self(list),
            None => Self(Vec::new()),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buffer: Vec<u8> = Vec::new();
        for val in &self.0 {
            match val {
                FileAttrValue::Type(v) => {
                    buffer
                        .extend_from_slice(ToPrimitive::to_u32(v).unwrap().to_be_bytes().as_ref());
                }
                FileAttrValue::LeaseTime(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::SupportedAttrs(v) => {
                    let attrs = Attrlist4::<FileAttr>::file_attrs_to_bitmap(v).unwrap();
                    buffer.extend_from_slice((attrs.len() as u32).to_be_bytes().as_ref());
                    attrs.iter().for_each(|attr| {
                        buffer.extend_from_slice(attr.to_be_bytes().as_ref());
                    });
                }
                FileAttrValue::FhExpireType(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::Change(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::Size(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::LinkSupport(v) => {
                    buffer.extend_from_slice((*v as u32).to_be_bytes().as_ref());
                }
                FileAttrValue::SymlinkSupport(v) => {
                    buffer.extend_from_slice((*v as u32).to_be_bytes().as_ref());
                }
                FileAttrValue::NamedAttr(v) => {
                    buffer.extend_from_slice((*v as u32).to_be_bytes().as_ref());
                }
                FileAttrValue::Fsid(v) => {
                    buffer.extend_from_slice(v.major.to_be_bytes().as_ref());
                    buffer.extend_from_slice(v.minor.to_be_bytes().as_ref());
                }
                FileAttrValue::UniqueHandles(v) => {
                    buffer.extend_from_slice((*v as u32).to_be_bytes().as_ref());
                }
                FileAttrValue::RdattrError(v) => {
                    buffer
                        .extend_from_slice(ToPrimitive::to_u32(v).unwrap().to_be_bytes().as_ref());
                }
                FileAttrValue::Fileid(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::AclSupport(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::Mode(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::TimeAccess(v) => {
                    buffer.extend_from_slice(v.seconds.to_be_bytes().as_ref());
                    buffer.extend_from_slice(v.nseconds.to_be_bytes().as_ref());
                }
                FileAttrValue::TimeModify(v) => {
                    buffer.extend_from_slice(v.seconds.to_be_bytes().as_ref());
                    buffer.extend_from_slice(v.nseconds.to_be_bytes().as_ref());
                }
                FileAttrValue::TimeMetadata(v) => {
                    buffer.extend_from_slice(v.seconds.to_be_bytes().as_ref());
                    buffer.extend_from_slice(v.nseconds.to_be_bytes().as_ref());
                }
                FileAttrValue::MountedOnFileid(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::Owner(v) => {
                    buffer.extend_from_slice((v.len() as u32).to_be_bytes().as_ref());
                    buffer.extend_from_slice(v.as_bytes());
                }
                FileAttrValue::OwnerGroup(v) => {
                    buffer.extend_from_slice((v.len() as u32).to_be_bytes().as_ref());
                    buffer.extend_from_slice(v.as_bytes());
                }
                FileAttrValue::SpaceUsed(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                FileAttrValue::Numlinks(v) => {
                    buffer.extend_from_slice(v.to_be_bytes().as_ref());
                }
                _ => {}
            }
        }
        buffer
    }
}

impl Serialize for Attrlist4<FileAttr> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let attrs = self.file_attrs_to_bitmap().unwrap();
        let mut seq = serializer.serialize_seq(Some(attrs.len()))?;
        for attr in attrs {
            let _ = seq.serialize_element(&attr);
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Attrlist4<FileAttr> {
    fn deserialize<D>(deserializer: D) -> Result<Attrlist4<FileAttr>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let attrs_raw = <Vec<u32> as serde::Deserialize>::deserialize(deserializer)?;
        let attrs_list = Attrlist4::from_u32(attrs_raw);
        Ok(attrs_list)
    }
}

impl Serialize for Attrlist4<FileAttrValue> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let attr_values = self.to_bytes();
        serializer.serialize_bytes(&attr_values)
    }
}
