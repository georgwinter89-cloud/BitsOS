//! Standard base64 (RFC 4648, with padding) for MCP image content. Scalar on purpose: kernel code has no
//! SSE/AVX (R10); a vector version belongs inside hal's guarded SIMD sections once they exist (KS-1).

use alloc::string::String;
use alloc::vec::Vec;

const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// Appends the base64 text of `data` to `out`.
pub fn encode_into(data: &[u8], out: &mut String) {
    let mut v: Vec<u8> = Vec::with_capacity(encoded_len(data.len()));
    let (chunks, rest) = data.as_chunks::<3>();
    for c in chunks {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        v.extend_from_slice(&[ABC[(n >> 18) as usize], ABC[(n >> 12 & 63) as usize], ABC[(n >> 6 & 63) as usize], ABC[(n & 63) as usize]]);
    }
    match *rest {
        [a] => {
            let n = (a as u32) << 16;
            v.extend_from_slice(&[ABC[(n >> 18) as usize], ABC[(n >> 12 & 63) as usize], b'=', b'=']);
        }
        [a, b] => {
            let n = (a as u32) << 16 | (b as u32) << 8;
            v.extend_from_slice(&[ABC[(n >> 18) as usize], ABC[(n >> 12 & 63) as usize], ABC[(n >> 6 & 63) as usize], b'=']);
        }
        _ => {}
    }
    // Only ASCII from ABC and '=' was written.
    out.push_str(core::str::from_utf8(&v).unwrap_or(""));
}

/// Decodes standard base64 (padding optional, no whitespace); None on any other byte. The ktests read MCP image
/// content back with it.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let t = text.trim_end_matches('=').as_bytes();
    let val = |c: u8| ABC.iter().position(|&a| a == c).map(|v| v as u32);
    let mut out = Vec::with_capacity(t.len() / 4 * 3 + 3);
    for c in t.chunks(4) {
        if c.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, &b) in c.iter().enumerate() {
            n |= val(b)? << (18 - 6 * i);
        }
        let b = n.to_be_bytes();
        out.extend_from_slice(&b[1..c.len()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc4648_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            let mut s = String::new();
            encode_into(i.as_bytes(), &mut s);
            assert_eq!(s, o);
            assert_eq!(encoded_len(i.len()), o.len());
        }
        let mut s = String::new();
        encode_into(&[0xFF, 0xFE, 0x00], &mut s);
        assert_eq!(s, "//4A");
        for i in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let mut s = String::new();
            encode_into(i.as_bytes(), &mut s);
            assert_eq!(decode(&s).as_deref(), Some(i.as_bytes()), "{s}");
        }
        assert_eq!(decode("//4A"), Some(alloc::vec![0xFF, 0xFE, 0x00]));
        assert_eq!(decode("Zg"), Some(alloc::vec![b'f']));
        assert_eq!(decode("Z"), None);
        assert_eq!(decode("Zm9v!"), None);
    }
}
