//! XDR (RFC 4506) encoding for serde, nextnfs's own (#111).
//!
//! Replaces the `serde-xdr` crate (GPL-3.0, refused by the licence policy)
//! with the same wire format and the same entry points (`to_bytes`,
//! `from_bytes`, `to_writer`, `from_reader`, `opaque_data::fixed_length`),
//! so the derived NFSv4/RPC types encode exactly as before:
//!
//! - `bool`, `i8`..`i32`, `u8`..`u32`, `char`: one 4-byte big-endian word;
//!   `i64`/`u64`: 8 bytes; `f32`/`f64`: IEEE big-endian.
//! - `str`/bytes: u32 length, data, zero padding to 4 bytes (opaque<>).
//!   Strings are written as their UTF-8 bytes and read back as UTF-8.
//! - `Option<T>`: u32 0/1, then the value (XDR optional-data).
//! - sequences: u32 count, then the elements (variable-length array);
//!   tuples, tuple structs, structs: the fields in order, no count.
//! - enums: the variant *index* as u32, then the variant's fields (the
//!   types that need real XDR discriminants implement Serialize by hand).
//! - maps, `deserialize_any`, identifiers: unsupported (no XDR form).

use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, Visitor};
use serde::ser::{self, Serialize};
use std::fmt;
use std::io::{Cursor, Read, Write};

/// XDR encode/decode error.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Message(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "XDR I/O error: {}", e),
            Error::Message(m) => write!(f, "XDR error: {}", m),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Message(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn padding(len: usize) -> usize {
    (4 - len % 4) % 4
}

fn length_u32(len: usize, what: &str) -> Result<u32> {
    u32::try_from(len).map_err(|_| Error::Message(format!("{} too long: {}", what, len)))
}

// ---------------------------------------------------------------- encoding

/// Encodes `value` as XDR into `writer`.
pub fn to_writer<W: Write, T: Serialize + ?Sized>(writer: &mut W, value: &T) -> Result<()> {
    value.serialize(&mut Serializer { writer })
}

/// Encodes `value` as XDR into a new buffer.
pub fn to_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    to_writer(&mut bytes, value)?;
    Ok(bytes)
}

/// Serde serializer writing XDR.
pub struct Serializer<'w, W: Write> {
    writer: &'w mut W,
}

impl<W: Write> Serializer<'_, W> {
    fn put_u32(&mut self, v: u32) -> Result<()> {
        Ok(self.writer.write_all(&v.to_be_bytes())?)
    }

    fn put_opaque(&mut self, data: &[u8]) -> Result<()> {
        self.put_u32(length_u32(data.len(), "opaque data")?)?;
        self.writer.write_all(data)?;
        self.writer.write_all(&[0u8; 3][..padding(data.len())])?;
        Ok(())
    }
}

impl<W: Write> ser::Serializer for &mut Serializer<'_, W> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = ser::Impossible<(), Error>;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, v: bool) -> Result<()> {
        self.put_u32(v as u32)
    }
    fn serialize_i8(self, v: i8) -> Result<()> {
        self.serialize_i32(v as i32)
    }
    fn serialize_i16(self, v: i16) -> Result<()> {
        self.serialize_i32(v as i32)
    }
    fn serialize_i32(self, v: i32) -> Result<()> {
        Ok(self.writer.write_all(&v.to_be_bytes())?)
    }
    fn serialize_i64(self, v: i64) -> Result<()> {
        Ok(self.writer.write_all(&v.to_be_bytes())?)
    }
    fn serialize_u8(self, v: u8) -> Result<()> {
        self.put_u32(v as u32)
    }
    fn serialize_u16(self, v: u16) -> Result<()> {
        self.put_u32(v as u32)
    }
    fn serialize_u32(self, v: u32) -> Result<()> {
        self.put_u32(v)
    }
    fn serialize_u64(self, v: u64) -> Result<()> {
        Ok(self.writer.write_all(&v.to_be_bytes())?)
    }
    fn serialize_f32(self, v: f32) -> Result<()> {
        Ok(self.writer.write_all(&v.to_be_bytes())?)
    }
    fn serialize_f64(self, v: f64) -> Result<()> {
        Ok(self.writer.write_all(&v.to_be_bytes())?)
    }
    fn serialize_char(self, v: char) -> Result<()> {
        self.put_u32(v as u32)
    }
    fn serialize_str(self, v: &str) -> Result<()> {
        self.put_opaque(v.as_bytes())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<()> {
        self.put_opaque(v)
    }
    fn serialize_none(self) -> Result<()> {
        self.put_u32(0)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<()> {
        self.put_u32(1)?;
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_variant(self, _name: &'static str, index: u32, _variant: &'static str) -> Result<()> {
        self.put_u32(index)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _name: &'static str, value: &T) -> Result<()> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<()> {
        self.put_u32(index)?;
        value.serialize(self)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self> {
        let len = len.ok_or_else(|| Error::Message("sequence with unknown length".into()))?;
        self.put_u32(length_u32(len, "sequence")?)?;
        Ok(self)
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _name: &'static str, _len: usize) -> Result<Self> {
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self> {
        self.put_u32(index)?;
        Ok(self)
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap> {
        Err(Error::Message("maps have no XDR encoding".into()))
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self> {
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self> {
        self.put_u32(index)?;
        Ok(self)
    }
}

macro_rules! compound_ser {
    ($trait:ident, $method:ident $(, $key:ident)?) => {
        impl<W: Write> ser::$trait for &mut Serializer<'_, W> {
            type Ok = ();
            type Error = Error;
            fn $method<T: Serialize + ?Sized>(&mut self, $($key: &'static str,)? value: &T) -> Result<()> {
                $(let _ = $key;)?
                value.serialize(&mut **self)
            }
            fn end(self) -> Result<()> {
                Ok(())
            }
        }
    };
}

compound_ser!(SerializeSeq, serialize_element);
compound_ser!(SerializeTuple, serialize_element);
compound_ser!(SerializeTupleStruct, serialize_field);
compound_ser!(SerializeTupleVariant, serialize_field);
compound_ser!(SerializeStruct, serialize_field, key);
compound_ser!(SerializeStructVariant, serialize_field, key);

// ---------------------------------------------------------------- decoding

/// Decodes one XDR value of type `T` from `reader`.
pub fn from_reader<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T> {
    T::deserialize(&mut Deserializer { reader })
}

/// Decodes one XDR value of type `T` from the front of `bytes`.
pub fn from_bytes<B: AsRef<[u8]>, T: DeserializeOwned>(bytes: B) -> Result<T> {
    from_reader(&mut Cursor::new(bytes.as_ref()))
}

/// Serde deserializer reading XDR.
pub struct Deserializer<'r, R: Read> {
    reader: &'r mut R,
}

impl<R: Read> Deserializer<'_, R> {
    fn get<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn get_u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.get()?))
    }

    fn get_i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.get()?))
    }

    /// Reads opaque<> data. The buffer grows as data arrives, so a forged
    /// length can not make it allocate more than the input holds.
    fn get_opaque(&mut self) -> Result<Vec<u8>> {
        let len = self.get_u32()? as u64;
        let mut data = Vec::new();
        (&mut *self.reader).take(len).read_to_end(&mut data)?;
        if (data.len() as u64) < len {
            return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
        }
        let mut pad = [0u8; 3];
        self.reader.read_exact(&mut pad[..padding(data.len())])?;
        Ok(data)
    }

}

fn ranged<T: TryFrom<i64>>(v: i64, what: &str) -> Result<T> {
    T::try_from(v).map_err(|_| Error::Message(format!("{} out of range: {}", what, v)))
}

impl<'de, R: Read> de::Deserializer<'de> for &mut Deserializer<'_, R> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, _v: V) -> Result<V::Value> {
        Err(Error::Message("XDR is not self-describing".into()))
    }
    fn deserialize_bool<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        match self.get_u32()? {
            0 => v.visit_bool(false),
            1 => v.visit_bool(true),
            n => Err(Error::Message(format!("invalid bool: {}", n))),
        }
    }
    fn deserialize_i8<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.get_i32()? as i64;
        v.visit_i8(ranged(n, "i8")?)
    }
    fn deserialize_i16<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.get_i32()? as i64;
        v.visit_i16(ranged(n, "i16")?)
    }
    fn deserialize_i32<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_i32(self.get_i32()?)
    }
    fn deserialize_i64<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_i64(i64::from_be_bytes(self.get()?))
    }
    fn deserialize_u8<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.get_u32()? as i64;
        v.visit_u8(ranged(n, "u8")?)
    }
    fn deserialize_u16<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.get_u32()? as i64;
        v.visit_u16(ranged(n, "u16")?)
    }
    fn deserialize_u32<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_u32(self.get_u32()?)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_u64(u64::from_be_bytes(self.get()?))
    }
    fn deserialize_f32<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_f32(f32::from_be_bytes(self.get()?))
    }
    fn deserialize_f64<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_f64(f64::from_be_bytes(self.get()?))
    }
    fn deserialize_char<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.get_u32()?;
        v.visit_char(char::from_u32(n).ok_or_else(|| Error::Message(format!("invalid char: {}", n)))?)
    }
    fn deserialize_str<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let s = String::from_utf8(self.get_opaque()?)
            .map_err(|e| Error::Message(format!("invalid UTF-8 string: {}", e)))?;
        v.visit_string(s)
    }
    fn deserialize_string<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        self.deserialize_str(v)
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_byte_buf(self.get_opaque()?)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        self.deserialize_bytes(v)
    }
    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        match self.get_u32()? {
            0 => v.visit_none(),
            1 => v.visit_some(self),
            n => Err(Error::Message(format!("invalid optional-data flag: {}", n))),
        }
    }
    fn deserialize_unit<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_unit()
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(self, _name: &'static str, v: V) -> Result<V::Value> {
        v.visit_unit()
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, v: V) -> Result<V::Value> {
        v.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let len = self.get_u32()? as usize;
        v.visit_seq(Elements { de: self, left: len })
    }
    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value> {
        v.visit_seq(Elements { de: self, left: len })
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(self, _name: &'static str, len: usize, v: V) -> Result<V::Value> {
        v.visit_seq(Elements { de: self, left: len })
    }
    fn deserialize_map<V: Visitor<'de>>(self, _v: V) -> Result<V::Value> {
        Err(Error::Message("maps have no XDR encoding".into()))
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        v: V,
    ) -> Result<V::Value> {
        v.visit_seq(Elements { de: self, left: fields.len() })
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        v: V,
    ) -> Result<V::Value> {
        let index = self.get_u32()?;
        if index as usize >= variants.len() {
            return Err(Error::Message(format!("invalid {} variant: {}", name, index)));
        }
        v.visit_enum(Variant { de: self, index })
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, _v: V) -> Result<V::Value> {
        Err(Error::Message("identifiers have no XDR encoding".into()))
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, _v: V) -> Result<V::Value> {
        Err(Error::Message("XDR can not skip a value of unknown type".into()))
    }
    fn is_human_readable(&self) -> bool {
        false
    }
}

/// Fixed number of consecutive values: array elements, tuple/struct fields.
struct Elements<'a, 'r, R: Read> {
    de: &'a mut Deserializer<'r, R>,
    left: usize,
}

impl<'de, R: Read> de::SeqAccess<'de> for Elements<'_, '_, R> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        // A forged count must not pre-allocate: serde caps the hint, but
        // keep it small anyway — elements are read one at a time.
        Some(self.left.min(1024))
    }
}

struct Variant<'a, 'r, R: Read> {
    de: &'a mut Deserializer<'r, R>,
    index: u32,
}

impl<'de, R: Read> de::EnumAccess<'de> for Variant<'_, '_, R> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<(T::Value, Self)> {
        let index: de::value::U32Deserializer<Error> = self.index.into_deserializer();
        Ok((seed.deserialize(index)?, self))
    }
}

impl<'de, R: Read> de::VariantAccess<'de> for Variant<'_, '_, R> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        seed.deserialize(&mut *self.de)
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value> {
        v.visit_seq(Elements { de: self.de, left: len })
    }
    fn struct_variant<V: Visitor<'de>>(self, fields: &'static [&'static str], v: V) -> Result<V::Value> {
        v.visit_seq(Elements { de: self.de, left: fields.len() })
    }
}

/// Helpers for `#[serde(with = "...")]` on byte fields.
pub mod opaque_data {
    /// Fixed-length opaque (`opaque name[N]`): the N bytes and zero padding
    /// to a multiple of 4, with no length word.
    pub mod fixed_length {
        use serde::de::{self, SeqAccess, Visitor};
        use serde::ser::SerializeTuple;
        use serde::{Deserializer, Serializer};
        use std::fmt;

        pub fn serialize<S: Serializer, const N: usize>(bytes: &[u8; N], serializer: S) -> Result<S::Ok, S::Error> {
            // One u32 per 4-byte block, so it works through any serializer
            // the same way, big-endian, last block zero-padded.
            let mut tuple = serializer.serialize_tuple(N.div_ceil(4))?;
            for block in bytes.chunks(4) {
                let mut word = [0u8; 4];
                word[..block.len()].copy_from_slice(block);
                tuple.serialize_element(&u32::from_be_bytes(word))?;
            }
            tuple.end()
        }

        pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error> {
            struct Blocks<const N: usize>;

            impl<'de, const N: usize> Visitor<'de> for Blocks<N> {
                type Value = [u8; N];

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "fixed-length opaque data of {} bytes", N)
                }

                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; N], A::Error> {
                    let mut out = [0u8; N];
                    for (i, chunk) in out.chunks_mut(4).enumerate() {
                        let word: u32 = seq
                            .next_element()?
                            .ok_or_else(|| de::Error::invalid_length(i * 4, &self))?;
                        chunk.copy_from_slice(&word.to_be_bytes()[..chunk.len()]);
                    }
                    Ok(out)
                }
            }

            deserializer.deserialize_tuple(N.div_ceil(4), Blocks::<N>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Shape {
        Empty,
        Circle(u32),
        Pair(u8, i16),
        Named { id: u64, tag: String },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        flag: bool,
        small: i8,
        word: u32,
        hyper: i64,
        name: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        list: Vec<u16>,
        opt: Option<u32>,
        none: Option<u32>,
        #[serde(with = "super::opaque_data::fixed_length")]
        verf: [u8; 8],
        #[serde(with = "super::opaque_data::fixed_length")]
        other: [u8; 6],
        shapes: Vec<Shape>,
    }

    fn sample() -> Record {
        Record {
            flag: true,
            small: -2,
            word: 0xdead_beef,
            hyper: -3,
            name: "abcde".into(),
            data: vec![1, 2, 3],
            list: vec![7, 8],
            opt: Some(9),
            none: None,
            verf: [1, 2, 3, 4, 5, 6, 7, 8],
            other: [9, 10, 11, 12, 13, 14],
            shapes: vec![
                Shape::Empty,
                Shape::Circle(5),
                Shape::Pair(1, -1),
                Shape::Named { id: 2, tag: "x".into() },
            ],
        }
    }

    #[rustfmt::skip]
    fn sample_wire() -> Vec<u8> {
        vec![
            0, 0, 0, 1,                         // flag
            0xff, 0xff, 0xff, 0xfe,             // small (-2 as int)
            0xde, 0xad, 0xbe, 0xef,             // word
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfd, // hyper
            0, 0, 0, 5, b'a', b'b', b'c', b'd', b'e', 0, 0, 0, // name
            0, 0, 0, 3, 1, 2, 3, 0,             // data
            0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 8, // list
            0, 0, 0, 1, 0, 0, 0, 9,             // opt
            0, 0, 0, 0,                         // none
            1, 2, 3, 4, 5, 6, 7, 8,             // verf: no length word
            9, 10, 11, 12, 13, 14, 0, 0,        // other: padded
            0, 0, 0, 4,                         // shapes count
            0, 0, 0, 0,                         // Empty
            0, 0, 0, 1, 0, 0, 0, 5,             // Circle(5)
            0, 0, 0, 2, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, // Pair(1, -1)
            0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 1, b'x', 0, 0, 0, // Named
        ]
    }

    #[test]
    fn encodes_rfc4506_wire_format() {
        assert_eq!(to_bytes(&sample()).unwrap(), sample_wire());
    }

    #[test]
    fn decodes_rfc4506_wire_format() {
        let decoded: Record = from_bytes(sample_wire()).unwrap();
        assert_eq!(decoded, sample());
    }

    #[test]
    fn non_ascii_string_roundtrip() {
        let bytes = to_bytes("filé_日本").unwrap();
        assert_eq!(bytes.len() % 4, 0);
        let s: String = from_bytes(&bytes).unwrap();
        assert_eq!(s, "filé_日本");
    }

    #[test]
    fn rejects_bad_bool_option_and_variant() {
        assert!(from_bytes::<_, bool>([0u8, 0, 0, 2]).is_err());
        assert!(from_bytes::<_, Option<u32>>([0u8, 0, 0, 2, 0, 0, 0, 0]).is_err());
        assert!(from_bytes::<_, Shape>([0u8, 0, 0, 4]).is_err());
        assert!(from_bytes::<_, u8>([0u8, 0, 1, 0]).is_err());
        assert!(from_bytes::<_, i8>([0xffu8, 0xff, 0xff, 0x7f]).is_err());
    }

    #[test]
    fn rejects_truncated_input() {
        let wire = sample_wire();
        for cut in [0, 3, 21, 40, wire.len() - 1] {
            assert!(from_bytes::<_, Record>(&wire[..cut]).is_err(), "cut at {}", cut);
        }
    }

    #[test]
    fn forged_lengths_do_not_allocate() {
        // opaque<> and array<> claiming 4 GiB with a few bytes behind them
        assert!(from_bytes::<_, serde_bytes::ByteBuf>([0xffu8, 0xff, 0xff, 0xff, 1, 2]).is_err());
        assert!(from_bytes::<_, Vec<u64>>([0xffu8, 0xff, 0xff, 0xff, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn maps_are_refused() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(1u32, 2u32);
        assert!(to_bytes(&m).is_err());
    }
}
