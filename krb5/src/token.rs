//! GSS-API context token framing (RFC 2743 §3.1) for the Kerberos V5
//! mechanism (RFC 4121 §4.1): `0x60 <DER length> <mech OID> <TOK_ID>
//! <Kerberos message>`. RFC 4121 puts every context-establishment token
//! (AP-REQ, AP-REP, KRB-ERROR) in this framing.

use crate::Error;

/// DER of the Kerberos V5 mechanism OID 1.2.840.113554.1.2.2.
pub const KRB5_MECH_OID: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];

pub const TOK_ID_AP_REQ: [u8; 2] = [0x01, 0x00];
pub const TOK_ID_AP_REP: [u8; 2] = [0x02, 0x00];
pub const TOK_ID_ERROR: [u8; 2] = [0x03, 0x00];

fn read_len(buf: &[u8], pos: &mut usize) -> Result<usize, Error> {
    let first = *buf.get(*pos).ok_or(Error::Token("truncated"))?;
    *pos += 1;
    if first & 0x80 == 0 {
        return Ok(first as usize);
    }
    let n = (first & 0x7f) as usize;
    if n == 0 || n > 4 {
        return Err(Error::Token("bad length"));
    }
    let bytes = buf.get(*pos..*pos + n).ok_or(Error::Token("truncated"))?;
    *pos += n;
    Ok(bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize))
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = (len as u32).to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (4 - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
}

/// Splits a framed token into its TOK_ID and Kerberos message.
pub fn parse(buf: &[u8]) -> Result<([u8; 2], &[u8]), Error> {
    if buf.first() != Some(&0x60) {
        return Err(Error::Token("not a GSS initial context token"));
    }
    let mut pos = 1;
    let len = read_len(buf, &mut pos)?;
    let body = buf.get(pos..pos + len).ok_or(Error::Token("truncated"))?;
    let inner = body.strip_prefix(KRB5_MECH_OID).ok_or(Error::Token("not the Kerberos V5 mechanism"))?;
    let tok_id: [u8; 2] = inner.get(..2).ok_or(Error::Token("truncated"))?.try_into().expect("2");
    Ok((tok_id, &inner[2..]))
}

/// Frames `message` with `tok_id`.
pub fn build(tok_id: [u8; 2], message: &[u8]) -> Vec<u8> {
    let body_len = KRB5_MECH_OID.len() + 2 + message.len();
    let mut out = Vec::with_capacity(body_len + 6);
    out.push(0x60);
    write_len(&mut out, body_len);
    out.extend_from_slice(KRB5_MECH_OID);
    out.extend_from_slice(&tok_id);
    out.extend_from_slice(message);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_short_and_long() {
        for n in [0usize, 10, 200, 70_000] {
            let msg = vec![0xab; n];
            let tok = build(TOK_ID_AP_REQ, &msg);
            let (id, m) = parse(&tok).unwrap();
            assert_eq!(id, TOK_ID_AP_REQ);
            assert_eq!(m, &msg[..]);
        }
        // 200-byte message: body 213 bytes, long form 0x81 0xd5.
        assert_eq!(&build(TOK_ID_AP_REP, &[0; 200])[1..3], &[0x81, 0xd5]);
    }

    #[test]
    fn rejects_other_mechs_and_truncation() {
        assert!(parse(&[0x60, 0x07, 0x06, 0x03, 0x2a, 0x03, 0x04, 0xaa, 0xbb]).is_err());
        assert!(parse(&[]).is_err());
        assert!(parse(&[0x61, 0x00]).is_err());
        let tok = build(TOK_ID_AP_REQ, &[1, 2, 3]);
        assert!(parse(&tok[..tok.len() - 1]).is_err());
        assert!(parse(&[0x60, 0x85, 1, 1, 1, 1, 1]).is_err());
    }
}
