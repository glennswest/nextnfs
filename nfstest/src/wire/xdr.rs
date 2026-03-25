//! XDR (RFC 4506) encoder and decoder.
//!
//! All NFS protocols use XDR for wire encoding. This module provides the low-level
//! primitives for reading and writing XDR-encoded data.

#![allow(dead_code)]

use bytes::{BufMut, BytesMut};

/// XDR encoder — writes values into a byte buffer.
pub struct XdrEncoder {
    buf: BytesMut,
}

impl XdrEncoder {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(4096),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: BytesMut::with_capacity(cap),
        }
    }

    /// Encode a 32-bit unsigned integer.
    pub fn put_u32(&mut self, val: u32) {
        self.buf.put_u32(val);
    }

    /// Encode a 32-bit signed integer.
    pub fn put_i32(&mut self, val: i32) {
        self.buf.put_i32(val);
    }

    /// Encode a 64-bit unsigned integer (hyper).
    pub fn put_u64(&mut self, val: u64) {
        self.buf.put_u64(val);
    }

    /// Encode a 64-bit signed integer.
    pub fn put_i64(&mut self, val: i64) {
        self.buf.put_i64(val);
    }

    /// Encode a boolean (XDR bool = 4 bytes).
    pub fn put_bool(&mut self, val: bool) {
        self.put_u32(if val { 1 } else { 0 });
    }

    /// Encode an opaque fixed-length byte array (padded to 4-byte boundary).
    pub fn put_opaque_fixed(&mut self, data: &[u8]) {
        self.buf.put_slice(data);
        let pad = (4 - (data.len() % 4)) % 4;
        for _ in 0..pad {
            self.buf.put_u8(0);
        }
    }

    /// Encode a variable-length opaque (length-prefixed, padded).
    pub fn put_opaque(&mut self, data: &[u8]) {
        self.put_u32(data.len() as u32);
        self.put_opaque_fixed(data);
    }

    /// Encode a string (same as variable opaque).
    pub fn put_string(&mut self, s: &str) {
        self.put_opaque(s.as_bytes());
    }

    /// Consume the encoder and return the buffer.
    pub fn finish(self) -> BytesMut {
        self.buf
    }

    /// Get the current encoded length.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Get a reference to the underlying bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

/// XDR decoder — reads values from a byte slice.
pub struct XdrDecoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> XdrDecoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { buf: data, pos: 0 }
    }

    /// Remaining bytes.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Decode a 32-bit unsigned integer.
    pub fn get_u32(&mut self) -> anyhow::Result<u32> {
        if self.remaining() < 4 {
            anyhow::bail!("XDR underflow: need 4 bytes, have {}", self.remaining());
        }
        let val = u32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(val)
    }

    /// Decode a 32-bit signed integer.
    pub fn get_i32(&mut self) -> anyhow::Result<i32> {
        Ok(self.get_u32()? as i32)
    }

    /// Decode a 64-bit unsigned integer.
    pub fn get_u64(&mut self) -> anyhow::Result<u64> {
        if self.remaining() < 8 {
            anyhow::bail!("XDR underflow: need 8 bytes, have {}", self.remaining());
        }
        let hi = self.get_u32()? as u64;
        let lo = self.get_u32()? as u64;
        Ok((hi << 32) | lo)
    }

    /// Decode a 64-bit signed integer.
    pub fn get_i64(&mut self) -> anyhow::Result<i64> {
        Ok(self.get_u64()? as i64)
    }

    /// Decode a boolean.
    pub fn get_bool(&mut self) -> anyhow::Result<bool> {
        Ok(self.get_u32()? != 0)
    }

    /// Decode a fixed-length opaque.
    pub fn get_opaque_fixed(&mut self, len: usize) -> anyhow::Result<Vec<u8>> {
        let padded = len + (4 - (len % 4)) % 4;
        if self.remaining() < padded {
            anyhow::bail!(
                "XDR underflow: need {} bytes (padded {}), have {}",
                len,
                padded,
                self.remaining()
            );
        }
        let data = self.buf[self.pos..self.pos + len].to_vec();
        self.pos += padded;
        Ok(data)
    }

    /// Decode a variable-length opaque.
    pub fn get_opaque(&mut self) -> anyhow::Result<Vec<u8>> {
        let len = self.get_u32()? as usize;
        self.get_opaque_fixed(len)
    }

    /// Decode a string.
    pub fn get_string(&mut self) -> anyhow::Result<String> {
        let data = self.get_opaque()?;
        Ok(String::from_utf8(data)?)
    }

    /// Skip n bytes (padded to 4-byte boundary).
    pub fn skip(&mut self, n: usize) -> anyhow::Result<()> {
        let padded = n + (4 - (n % 4)) % 4;
        if self.remaining() < padded {
            anyhow::bail!("XDR underflow on skip");
        }
        self.pos += padded;
        Ok(())
    }

    /// Current position in the buffer.
    pub fn position(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_u32() {
        let mut enc = XdrEncoder::new();
        enc.put_u32(42);
        enc.put_u32(0);
        enc.put_u32(u32::MAX);

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(&bytes);
        assert_eq!(dec.get_u32().unwrap(), 42);
        assert_eq!(dec.get_u32().unwrap(), 0);
        assert_eq!(dec.get_u32().unwrap(), u32::MAX);
    }

    #[test]
    fn roundtrip_u64() {
        let mut enc = XdrEncoder::new();
        enc.put_u64(0xDEAD_BEEF_CAFE_BABE);

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(&bytes);
        assert_eq!(dec.get_u64().unwrap(), 0xDEAD_BEEF_CAFE_BABE);
    }

    #[test]
    fn roundtrip_string() {
        let mut enc = XdrEncoder::new();
        enc.put_string("hello");
        enc.put_string("");
        enc.put_string("test with padding!!");

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(&bytes);
        assert_eq!(dec.get_string().unwrap(), "hello");
        assert_eq!(dec.get_string().unwrap(), "");
        assert_eq!(dec.get_string().unwrap(), "test with padding!!");
    }

    #[test]
    fn roundtrip_opaque() {
        let data = vec![1, 2, 3, 4, 5];
        let mut enc = XdrEncoder::new();
        enc.put_opaque(&data);

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(&bytes);
        assert_eq!(dec.get_opaque().unwrap(), data);
    }

    #[test]
    fn xdr_padding() {
        // 5 bytes of opaque should be padded to 8 (4 len + 5 data + 3 pad)
        let mut enc = XdrEncoder::new();
        enc.put_opaque(&[1, 2, 3, 4, 5]);
        assert_eq!(enc.len(), 4 + 8); // 4 (length) + 8 (5 data + 3 padding)
    }

    #[test]
    fn roundtrip_bool() {
        let mut enc = XdrEncoder::new();
        enc.put_bool(true);
        enc.put_bool(false);

        let bytes = enc.finish();
        let mut dec = XdrDecoder::new(&bytes);
        assert!(dec.get_bool().unwrap());
        assert!(!dec.get_bool().unwrap());
    }
}
