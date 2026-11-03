//! Standard base64 (RFC 4648 with padding) for data that comes from programs as text (DevTools screenshots).

pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits, mut pad) = (0u32, 0u32, 0usize);
    for (i, c) in s.bytes().enumerate() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            b'\n' | b'\r' => continue,
            _ => return Err(format!("not base64 at byte {i}")),
        };
        if pad > 0 {
            return Err("data after the padding".into());
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if pad > 2 || bits >= 6 {
        return Err("truncated base64".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_values() {
        assert_eq!(super::decode("").unwrap(), b"");
        assert_eq!(super::decode("Zg==").unwrap(), b"f");
        assert_eq!(super::decode("Zm8=").unwrap(), b"fo");
        assert_eq!(super::decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(super::decode("iVBORw0KGgo=").unwrap(), b"\x89PNG\r\n\x1a\n");
        assert!(super::decode("Zm9v!").is_err());
        assert!(super::decode("Z").is_err());
        assert!(super::decode("Zg==Zg").is_err());
    }
}
