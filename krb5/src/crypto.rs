//! Kerberos V5 AES encryption types (RFC 3961, RFC 3962, RFC 8009) on
//! RustCrypto primitives.
//!
//! - RFC 3962 `aes128/256-cts-hmac-sha1-96` (etypes 17/18): RFC 3961
//!   `DK` key derivation (n-fold + AES), HMAC-SHA1 over the plaintext.
//! - RFC 8009 `aes128-cts-hmac-sha256-128` / `aes256-cts-hmac-sha384-192`
//!   (etypes 19/20): KDF-HMAC-SHA2 key derivation, HMAC over
//!   `IV | ciphertext` (encrypt-then-MAC).
//!
//! Both encrypt `confounder | plaintext` with AES-CBC and ciphertext
//! stealing (CS3: the last two blocks always swapped). Ported from
//! irondirectory's `iron_crypto::kerberos` (OpenSSL there) and checked
//! against the RFCs' published test vectors (see the tests).

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Aes256};
use hmac::{Hmac, Mac};

use crate::Error;

/// The AES Kerberos enctypes (IANA numbers). No DES/RC4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Enctype {
    Aes128CtsHmacSha1_96 = 17,
    Aes256CtsHmacSha1_96 = 18,
    Aes128CtsHmacSha256_128 = 19,
    Aes256CtsHmacSha384_192 = 20,
}

impl Enctype {
    pub const ALL: [Enctype; 4] = [
        Enctype::Aes128CtsHmacSha1_96,
        Enctype::Aes256CtsHmacSha1_96,
        Enctype::Aes128CtsHmacSha256_128,
        Enctype::Aes256CtsHmacSha384_192,
    ];

    pub fn from_i32(v: i32) -> Option<Enctype> {
        Enctype::ALL.into_iter().find(|e| *e as i32 == v)
    }

    pub fn number(self) -> i32 {
        self as i32
    }

    pub fn key_len(self) -> usize {
        match self {
            Enctype::Aes128CtsHmacSha1_96 | Enctype::Aes128CtsHmacSha256_128 => 16,
            Enctype::Aes256CtsHmacSha1_96 | Enctype::Aes256CtsHmacSha384_192 => 32,
        }
    }

    /// Length of the truncated HMAC (`h`): the encryption trailer and the
    /// checksum (MIC) length.
    pub fn hmac_len(self) -> usize {
        match self {
            Enctype::Aes128CtsHmacSha1_96 | Enctype::Aes256CtsHmacSha1_96 => 12,
            Enctype::Aes128CtsHmacSha256_128 => 16,
            Enctype::Aes256CtsHmacSha384_192 => 24,
        }
    }

    fn is_rfc8009(self) -> bool {
        matches!(self, Enctype::Aes128CtsHmacSha256_128 | Enctype::Aes256CtsHmacSha384_192)
    }

    /// RFC 8009 Ki / Kc length in bits (Ke is the AES key length).
    fn rfc8009_mac_bits(self) -> u32 {
        match self {
            Enctype::Aes256CtsHmacSha384_192 => 192,
            _ => 128,
        }
    }
}

// ------------------------------------------------------------ primitives

enum AesKey {
    A128(Box<Aes128>),
    A256(Box<Aes256>),
}

impl AesKey {
    fn new(key: &[u8]) -> Result<Self, Error> {
        match key.len() {
            16 => Ok(AesKey::A128(Box::new(Aes128::new(GenericArray::from_slice(key))))),
            32 => Ok(AesKey::A256(Box::new(Aes256::new(GenericArray::from_slice(key))))),
            n => Err(Error::Crypto(format!("bad AES key length {}", n))),
        }
    }

    fn encrypt(&self, block: &mut [u8; 16]) {
        let b = GenericArray::from_mut_slice(block);
        match self {
            AesKey::A128(k) => k.encrypt_block(b),
            AesKey::A256(k) => k.encrypt_block(b),
        }
    }

    fn decrypt(&self, block: &mut [u8; 16]) {
        let b = GenericArray::from_mut_slice(block);
        match self {
            AesKey::A128(k) => k.decrypt_block(b),
            AesKey::A256(k) => k.decrypt_block(b),
        }
    }
}

fn xor16(a: &mut [u8; 16], b: &[u8]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= y;
    }
}

/// AES-CBC with ciphertext stealing (CS3), zero IV. Input must be at
/// least one block (Kerberos always has a 16-byte confounder).
fn cts_encrypt(key: &[u8], input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.len() < 16 {
        return Err(Error::Crypto("CTS input shorter than one block".into()));
    }
    let aes = AesKey::new(key)?;
    let n = input.len().div_ceil(16);
    // Plain CBC over the zero-padded input...
    let mut blocks: Vec<[u8; 16]> = Vec::with_capacity(n);
    let mut prev = [0u8; 16];
    for chunk in input.chunks(16) {
        let mut b = prev;
        xor16(&mut b, chunk);
        aes.encrypt(&mut b);
        blocks.push(b);
        prev = b;
    }
    // ...then swap the last two blocks and cut the (new) last one to the
    // length of the final input block.
    let mut out = Vec::with_capacity(input.len());
    if n == 1 {
        out.extend_from_slice(&blocks[0]);
        return Ok(out);
    }
    let last_len = input.len() - 16 * (n - 1);
    for b in &blocks[..n - 2] {
        out.extend_from_slice(b);
    }
    out.extend_from_slice(&blocks[n - 1]);
    out.extend_from_slice(&blocks[n - 2][..last_len]);
    Ok(out)
}

fn cts_decrypt(key: &[u8], input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.len() < 16 {
        return Err(Error::Crypto("CTS input shorter than one block".into()));
    }
    let aes = AesKey::new(key)?;
    let n = input.len().div_ceil(16);
    let mut out = Vec::with_capacity(input.len());
    let mut prev = [0u8; 16];
    for chunk in input.chunks(16).take(n.saturating_sub(2)) {
        let mut b: [u8; 16] = chunk.try_into().expect("full block");
        aes.decrypt(&mut b);
        xor16(&mut b, &prev);
        out.extend_from_slice(&b);
        prev = chunk.try_into().expect("full block");
    }
    if n == 1 {
        let mut b: [u8; 16] = input.try_into().expect("one block");
        aes.decrypt(&mut b);
        out.extend_from_slice(&b);
        return Ok(out);
    }
    let base = 16 * (n - 2);
    let last_len = input.len() - 16 * (n - 1);
    // The full block here is the CBC block of the final plaintext; the
    // partial tail is the head of the next-to-last CBC block.
    let swapped: [u8; 16] = input[base..base + 16].try_into().expect("full block");
    let tail = &input[base + 16..];
    let mut z = swapped;
    aes.decrypt(&mut z);
    let mut c_prev = [0u8; 16];
    c_prev[..last_len].copy_from_slice(tail);
    c_prev[last_len..].copy_from_slice(&z[last_len..]);
    let mut last_plain = z;
    xor16(&mut last_plain, &c_prev);
    let mut p = c_prev;
    aes.decrypt(&mut p);
    xor16(&mut p, &prev);
    out.extend_from_slice(&p);
    out.extend_from_slice(&last_plain[..last_len]);
    Ok(out)
}

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut m = <Hmac<sha1::Sha1> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().to_vec()
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().to_vec()
}

fn hmac_sha384(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut m = <Hmac<sha2::Sha384> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().to_vec()
}

fn hmac(enctype: Enctype, key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    match enctype {
        Enctype::Aes128CtsHmacSha1_96 | Enctype::Aes256CtsHmacSha1_96 => hmac_sha1(key, parts),
        Enctype::Aes128CtsHmacSha256_128 => hmac_sha256(key, parts),
        Enctype::Aes256CtsHmacSha384_192 => hmac_sha384(key, parts),
    }
}

/// Constant-time comparison.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

// ---------------------------------------------------------------- n-fold

/// RFC 3961 §5.1 n-fold of `input` to `out_len` bytes.
pub fn n_fold(input: &[u8], out_len: usize) -> Vec<u8> {
    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    let in_len = input.len();
    let lcm = in_len / gcd(in_len, out_len) * out_len;
    let in_bits = in_len * 8;
    // Byte `i` of the repeated, rotated input: copy `k = i / in_len` of the
    // input rotated right by 13*k bits.
    let byte_at = |i: usize| -> u8 {
        let k = i / in_len;
        let pos = i % in_len;
        let rot = (13 * k) % in_bits;
        let bit = |b: usize| -> u8 {
            let src = (b + in_bits - rot) % in_bits;
            (input[src / 8] >> (7 - src % 8)) & 1
        };
        (0..8).fold(0u8, |acc, j| (acc << 1) | bit(pos * 8 + j))
    };
    let mut acc = vec![0u32; out_len];
    for i in 0..lcm {
        acc[i % out_len] += byte_at(i) as u32;
    }
    // Ones' complement addition: propagate carries (end-around) until
    // none are left.
    loop {
        let mut carry = 0u32;
        for i in (0..out_len).rev() {
            let v = acc[i] + carry;
            acc[i] = v & 0xff;
            carry = v >> 8;
        }
        if carry == 0 {
            break;
        }
        acc[out_len - 1] += carry;
    }
    acc.into_iter().map(|v| v as u8).collect()
}

// ---------------------------------------------------- key derivation

/// RFC 3961 `DK(key, constant)` for AES (random-to-key is the identity).
fn dk(key: &[u8], constant: &[u8]) -> Result<Vec<u8>, Error> {
    let aes = AesKey::new(key)?;
    let mut block: [u8; 16] = n_fold(constant, 16).try_into().expect("16 bytes");
    let mut out = Vec::with_capacity(key.len() + 16);
    while out.len() < key.len() {
        aes.encrypt(&mut block);
        out.extend_from_slice(&block);
    }
    out.truncate(key.len());
    Ok(out)
}

/// RFC 8009 §3 KDF-HMAC-SHA2(key, label, k bits).
fn kdf_hmac_sha2(enctype: Enctype, key: &[u8], label: &[u8], k_bits: u32) -> Vec<u8> {
    let mut out = hmac(enctype, key, &[&1u32.to_be_bytes(), label, &[0], &k_bits.to_be_bytes()]);
    out.truncate(k_bits as usize / 8);
    out
}

fn derive(enctype: Enctype, base: &[u8], usage: u32, tag: u8) -> Result<Vec<u8>, Error> {
    if base.len() != enctype.key_len() {
        return Err(Error::Crypto(format!(
            "key length {} does not match {:?}",
            base.len(),
            enctype
        )));
    }
    let mut label = usage.to_be_bytes().to_vec();
    label.push(tag);
    if enctype.is_rfc8009() {
        let bits = if tag == 0xAA { (enctype.key_len() * 8) as u32 } else { enctype.rfc8009_mac_bits() };
        Ok(kdf_hmac_sha2(enctype, base, &label, bits))
    } else {
        dk(base, &label)
    }
}

/// Long-term key from a password (RFC 3962 §4 / RFC 8009 §4). Used to
/// build keys for tests and tools; servers read keys from a keytab.
pub fn string_to_key(enctype: Enctype, password: &[u8], salt: &[u8], iterations: Option<u32>) -> Result<Vec<u8>, Error> {
    let len = enctype.key_len();
    let mut tkey = vec![0u8; len];
    if enctype.is_rfc8009() {
        let name: &[u8] = match enctype {
            Enctype::Aes128CtsHmacSha256_128 => b"aes128-cts-hmac-sha256-128",
            _ => b"aes256-cts-hmac-sha384-192",
        };
        let mut saltp = name.to_vec();
        saltp.push(0);
        saltp.extend_from_slice(salt);
        let iter = iterations.unwrap_or(32768);
        if enctype == Enctype::Aes128CtsHmacSha256_128 {
            pbkdf2::pbkdf2::<Hmac<sha2::Sha256>>(password, &saltp, iter, &mut tkey)
        } else {
            pbkdf2::pbkdf2::<Hmac<sha2::Sha384>>(password, &saltp, iter, &mut tkey)
        }
        .map_err(|e| Error::Crypto(e.to_string()))?;
        Ok(kdf_hmac_sha2(enctype, &tkey, b"kerberos", (len * 8) as u32))
    } else {
        pbkdf2::pbkdf2::<Hmac<sha1::Sha1>>(password, salt, iterations.unwrap_or(4096), &mut tkey)
            .map_err(|e| Error::Crypto(e.to_string()))?;
        dk(&tkey, b"kerberos")
    }
}

// ------------------------------------------------------------ public API

/// Random bytes from the OS.
pub fn random_bytes(len: usize) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; len];
    getrandom::getrandom(&mut buf).map_err(|e| Error::Crypto(format!("getrandom: {}", e)))?;
    Ok(buf)
}

/// Encrypts `plaintext` for `usage` (RFC 4120 §7.5.1 / RFC 4121 §2) under
/// `base_key`; returns the `EncryptedData.cipher` bytes.
pub fn encrypt(enctype: Enctype, base_key: &[u8], usage: u32, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    let confounder: [u8; 16] = random_bytes(16)?.try_into().expect("16 bytes");
    encrypt_with_confounder(enctype, base_key, usage, plaintext, &confounder)
}

pub(crate) fn encrypt_with_confounder(
    enctype: Enctype,
    base_key: &[u8],
    usage: u32,
    plaintext: &[u8],
    confounder: &[u8; 16],
) -> Result<Vec<u8>, Error> {
    let ke = derive(enctype, base_key, usage, 0xAA)?;
    let ki = derive(enctype, base_key, usage, 0x55)?;
    let mut data = Vec::with_capacity(16 + plaintext.len());
    data.extend_from_slice(confounder);
    data.extend_from_slice(plaintext);
    let mut out = cts_encrypt(&ke, &data)?;
    let mac = if enctype.is_rfc8009() {
        hmac(enctype, &ki, &[&[0u8; 16], &out])
    } else {
        hmac(enctype, &ki, &[&data])
    };
    out.extend_from_slice(&mac[..enctype.hmac_len()]);
    Ok(out)
}

/// Decrypts and verifies `EncryptedData.cipher` bytes; returns the
/// plaintext without the confounder.
pub fn decrypt(enctype: Enctype, base_key: &[u8], usage: u32, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
    let h = enctype.hmac_len();
    if ciphertext.len() < 16 + h {
        return Err(Error::Integrity);
    }
    let (c, mac) = ciphertext.split_at(ciphertext.len() - h);
    let ke = derive(enctype, base_key, usage, 0xAA)?;
    let ki = derive(enctype, base_key, usage, 0x55)?;
    if enctype.is_rfc8009() {
        if !ct_eq(&hmac(enctype, &ki, &[&[0u8; 16], c])[..h], mac) {
            return Err(Error::Integrity);
        }
        let mut data = cts_decrypt(&ke, c)?;
        data.drain(..16);
        Ok(data)
    } else {
        let mut data = cts_decrypt(&ke, c)?;
        if !ct_eq(&hmac(enctype, &ki, &[&data])[..h], mac) {
            return Err(Error::Integrity);
        }
        data.drain(..16);
        Ok(data)
    }
}

/// Keyed checksum (`get_mic`, RFC 3961 §5.4 / RFC 8009 §6) of `message`
/// for `usage`.
pub fn checksum(enctype: Enctype, base_key: &[u8], usage: u32, message: &[&[u8]]) -> Result<Vec<u8>, Error> {
    let kc = derive(enctype, base_key, usage, 0x99)?;
    let mut mac = hmac(enctype, &kc, message);
    mac.truncate(enctype.hmac_len());
    Ok(mac)
}

#[cfg(test)]
pub(crate) fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3961_n_fold() {
        assert_eq!(n_fold(b"012345", 8), hex("be072631276b1955"));
        assert_eq!(n_fold(b"password", 7), hex("78a07b6caf85fa"));
        assert_eq!(n_fold(b"Rough Consensus, and Running Code", 8), hex("bb6ed30870b7f0e0"));
        assert_eq!(n_fold(b"password", 21), hex("59e4a8ca7c0385c3c37b3f6d2000247cb6e6bd5b3e"));
        assert_eq!(
            n_fold(b"MASSACHVSETTS INSTITVTE OF TECHNOLOGY", 24),
            hex("db3b0d8f0b061e603282b308a50841229ad798fab9540c1b")
        );
        assert_eq!(n_fold(b"Q", 21), hex("518a54a215a8452a518a54a215a8452a518a54a215"));
        assert_eq!(n_fold(b"ba", 21), hex("fb25d531ae8974499f52fd92ea9857c4ba24cf297e"));
        assert_eq!(n_fold(b"kerberos", 8), hex("6b65726265726f73"));
        assert_eq!(n_fold(b"kerberos", 16), hex("6b65726265726f737b9b5b2b93132b93"));
        assert_eq!(
            n_fold(b"kerberos", 32),
            hex("6b65726265726f737b9b5b2b93132b935c9bdcdad95c9899c4cae4dee6d6cae4")
        );
    }

    #[test]
    fn rfc3962_string_to_key() {
        let salt = b"ATHENA.MIT.EDUraeburn";
        let e128 = Enctype::Aes128CtsHmacSha1_96;
        let e256 = Enctype::Aes256CtsHmacSha1_96;
        assert_eq!(string_to_key(e128, b"password", salt, Some(1)).unwrap(), hex("42263c6e89f4fc28b8df68ee09799f15"));
        assert_eq!(
            string_to_key(e256, b"password", salt, Some(1)).unwrap(),
            hex("fe697b52bc0d3ce14432ba036a92e65bbb52280990a2fa27883998d72af30161")
        );
        assert_eq!(string_to_key(e128, b"password", salt, Some(1200)).unwrap(), hex("4c01cd46d632d01e6dbe230a01ed642a"));
        assert_eq!(
            string_to_key(e256, b"password", salt, Some(1200)).unwrap(),
            hex("55a6ac740ad17b4846941051e1e8b0a7548d93b0ab30a8bc3ff16280382b8c2a")
        );
        assert_eq!(
            string_to_key(e256, &[0xf0, 0x9d, 0x84, 0x9e], b"EXAMPLE.COMpianist", Some(50)).unwrap(),
            hex("4b6d9839f84406df1f09cc166db4b83c571848b784a3d6bdc346589a3e393f9e")
        );
    }

    #[test]
    fn rfc3962_cts_vectors() {
        let key = hex("636869636b656e207465726979616b69");
        let input = b"I would like the General Gau's Chicken, please, and wonton soup.";
        let cases: [(usize, &str); 6] = [
            (17, "c6353568f2bf8cb4d8a580362da7ff7f97"),
            (31, "fc00783e0efdb2c1d445d4c8eff7ed2297687268d6ecccc0c07b25e25ecfe5"),
            (32, "39312523a78662d5be7fcbcc98ebf5a897687268d6ecccc0c07b25e25ecfe584"),
            (47, "97687268d6ecccc0c07b25e25ecfe584b3fffd940c16a18c1b5549d2f838029e39312523a78662d5be7fcbcc98ebf5"),
            (48, "97687268d6ecccc0c07b25e25ecfe5849dad8bbb96c4cdc03bc103e1a194bbd839312523a78662d5be7fcbcc98ebf5a8"),
            (
                64,
                "97687268d6ecccc0c07b25e25ecfe58439312523a78662d5be7fcbcc98ebf5a84807efe836ee89a526730dbc2f7bc8409dad8bbb96c4cdc03bc103e1a194bbd8",
            ),
        ];
        for (len, want) in cases {
            let ct = cts_encrypt(&key, &input[..len]).unwrap();
            assert_eq!(ct, hex(want), "encrypt {}", len);
            assert_eq!(cts_decrypt(&key, &ct).unwrap(), &input[..len], "decrypt {}", len);
        }
    }

    #[test]
    fn rfc8009_string_to_key() {
        let mut salt = hex("10df9dd783e5bc8acea1730e74355f61");
        salt.extend_from_slice(b"ATHENA.MIT.EDUraeburn");
        assert_eq!(
            string_to_key(Enctype::Aes128CtsHmacSha256_128, b"password", &salt, None).unwrap(),
            hex("089bca48b105ea6ea77ca5d2f39dc5e7")
        );
        assert_eq!(
            string_to_key(Enctype::Aes256CtsHmacSha384_192, b"password", &salt, None).unwrap(),
            hex("45bd806dbf6a833a9cffc1c94589a222367a79bc21c4137189 06e9f578a78467")
        );
    }

    #[test]
    fn rfc8009_key_derivation() {
        let e = Enctype::Aes128CtsHmacSha256_128;
        let base = hex("3705d96080c17728a0e800eab6e0d23c");
        assert_eq!(derive(e, &base, 2, 0x99).unwrap(), hex("b31a018a48f54776f403e9a396325dc3"));
        assert_eq!(derive(e, &base, 2, 0xAA).unwrap(), hex("9b197dd1e8c5609d6e67c3e37c62c72e"));
        assert_eq!(derive(e, &base, 2, 0x55).unwrap(), hex("9fda0e56ab2d85e1569a688696c26a6c"));
        let e = Enctype::Aes256CtsHmacSha384_192;
        let base = hex("6d404d37faf79f9df0d33568d320669800eb4836472ea8a026d16b7182460c52");
        assert_eq!(derive(e, &base, 2, 0x99).unwrap(), hex("ef5718be86cc84963d8bbb5031e9f5c4ba41f28faf69e73d"));
        assert_eq!(
            derive(e, &base, 2, 0xAA).unwrap(),
            hex("56ab22bee63d82d7bc5227f6773f8ea7a5eb1c825160c38312980c442e5c7e49")
        );
        assert_eq!(derive(e, &base, 2, 0x55).unwrap(), hex("69b16514e3cd8e56b82010d5c73012b622c4d00ffc23ed1f"));
    }

    #[test]
    fn rfc8009_encryption_vectors() {
        let base128 = hex("3705d96080c17728a0e800eab6e0d23c");
        let base256 = hex("6d404d37faf79f9df0d33568d320669800eb4836472ea8a026d16b7182460c52");
        let pt21 = hex("000102030405060708090a0b0c0d0e0f1011121314");
        // (enctype, base key, plaintext, confounder, ciphertext)
        type Case<'a> = (Enctype, &'a [u8], &'a [u8], &'a str, &'a str);
        let cases: [Case; 8] = [
            (Enctype::Aes128CtsHmacSha256_128, &base128, &[], "7e5895eaf2672435bad817f545a37148",
             "ef85fb890bb8472f4dab20394dca781dad877eda39d50c870c0d5a0a8e48c718"),
            (Enctype::Aes128CtsHmacSha256_128, &base128, &pt21[..6], "7bca285e2fd4130fb55b1a5c83bc5b24",
             "84d7f30754ed987bab0bf3506beb09cfb55402cef7e6877ce99e247e52d16ed4421dfdf8976c"),
            (Enctype::Aes128CtsHmacSha256_128, &base128, &pt21[..16], "56ab21713ff62c0a1457200f6fa9948f",
             "3517d640f50ddc8ad3628722b3569d2ae07493fa8263254080ea65c1008e8fc295fb4852e7d83e1e7c48c37eebe6b0d3"),
            (Enctype::Aes128CtsHmacSha256_128, &base128, &pt21, "a7a4e29a4728ce10664fb64e49ad3fac",
             "720f73b18d9859cd6ccb4346115cd336c70f58edc0c4437c5573544c31c813bce1e6d072c186b39a413c2f92ca9b8334a287ffcbfc"),
            (Enctype::Aes256CtsHmacSha384_192, &base256, &[], "f764e9fa15c276478b2c7d0c4e5f58e4",
             "41f53fa5bfe7026d91faf9be959195a058707273a96a40f0a01960621ac612748b9bbfbe7eb4ce3c"),
            (Enctype::Aes256CtsHmacSha384_192, &base256, &pt21[..6], "b80d3251c1f6471494256ffe712d0b9a",
             "4ed7b37c2bcac8f74f23c1cf07e62bc7b75fb3f637b9f559c7f664f69eab7b6092237526ea0d1f61cb20d69d10f2"),
            (Enctype::Aes256CtsHmacSha384_192, &base256, &pt21[..16], "53bf8a0d105265d4e276428624ce5e63",
             "bc47ffec7998eb91e8115cf8d19dac4bbbe2e163e87dd37f49beca92027764f68cf51f14d798c2273f35df574d1f932e40c4ff255b36a266"),
            (Enctype::Aes256CtsHmacSha384_192, &base256, &pt21, "763e65367e864f02f55153c7e3b58af1",
             "40013e2df58e8751957d2878bcd2d6fe101ccfd556cb1eae79db3c3ee86429f2b2a602ac86fef6ecb647d6295fae077a1feb517508d2c16b4192e01f62"),
        ];
        for (e, base, pt, conf, want) in cases {
            let conf: [u8; 16] = hex(conf).try_into().unwrap();
            let ct = encrypt_with_confounder(e, base, 2, pt, &conf).unwrap();
            assert_eq!(ct, hex(want), "{:?} len {}", e, pt.len());
            assert_eq!(decrypt(e, base, 2, &ct).unwrap(), pt);
        }
    }

    #[test]
    fn rfc8009_checksum_vectors() {
        let pt = hex("000102030405060708090a0b0c0d0e0f1011121314");
        let base128 = hex("3705d96080c17728a0e800eab6e0d23c");
        let base256 = hex("6d404d37faf79f9df0d33568d320669800eb4836472ea8a026d16b7182460c52");
        assert_eq!(
            checksum(Enctype::Aes128CtsHmacSha256_128, &base128, 2, &[&pt]).unwrap(),
            hex("d78367186643d67b411cba9139fc1dee")
        );
        assert_eq!(
            checksum(Enctype::Aes256CtsHmacSha384_192, &base256, 2, &[&pt]).unwrap(),
            hex("45ee791567eefca37f4ac1e0222de80d43c3bfa06699672a")
        );
    }

    #[test]
    fn roundtrip_all_enctypes_and_tamper() {
        for e in Enctype::ALL {
            let key = random_bytes(e.key_len()).unwrap();
            for len in [0usize, 1, 15, 16, 17, 31, 32, 33, 100, 4096] {
                let pt: Vec<u8> = (0..len).map(|i| i as u8).collect();
                let ct = encrypt(e, &key, 3, &pt).unwrap();
                assert_eq!(ct.len(), 16 + len + e.hmac_len());
                assert_eq!(decrypt(e, &key, 3, &ct).unwrap(), pt, "{:?} {}", e, len);
                assert!(decrypt(e, &key, 4, &ct).is_err(), "wrong usage {:?}", e);
                let mut bad = ct.clone();
                bad[len / 2] ^= 1;
                assert!(decrypt(e, &key, 3, &bad).is_err(), "tampered {:?} {}", e, len);
            }
            assert!(decrypt(e, &key, 3, &[0u8; 20]).is_err());
        }
    }
}
