//! Base64URL (`A–Z a–z 0–9 - _`), no padding, fixed width: `n` bytes are
//! always `ceil(4n/3)` characters, so a row of encoded bytes is seekable.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encoded length of `n` bytes.
pub const fn encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4 - match n % 3 {
        0 => 0,
        1 => 2,
        _ => 1,
    }
}

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(encoded_len(bytes.len()));
    encode_into(bytes, &mut out);
    out
}

pub fn encode_into(bytes: &[u8], out: &mut String) {
    let (triples, rest) = bytes.as_chunks::<3>();
    for c in triples {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        for shift in [18, 12, 6, 0] {
            out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
        }
    }
    match rest.len() {
        1 => {
            let n = u32::from(rest[0]) << 16;
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        }
        2 => {
            let n = (u32::from(rest[0]) << 16) | (u32::from(rest[1]) << 8);
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        }
        _ => {}
    }
}

fn value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => u32::from(c - b'A'),
        b'a'..=b'z' => u32::from(c - b'a') + 26,
        b'0'..=b'9' => u32::from(c - b'0') + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    })
}

/// Decode; `None` on a character outside the alphabet or an impossible
/// length (one trailing character can encode nothing).
pub fn decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    decode_into(text, &mut out).then_some(out)
}

pub fn decode_into(text: &[u8], out: &mut Vec<u8>) -> bool {
    let (quads, rest) = text.as_chunks::<4>();
    for q in quads {
        let (Some(a), Some(b), Some(c), Some(d)) = (value(q[0]), value(q[1]), value(q[2]), value(q[3])) else {
            return false;
        };
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    match rest.len() {
        0 => true,
        2 => {
            let (Some(a), Some(b)) = (value(rest[0]), value(rest[1])) else { return false };
            out.push(((a << 2) | (b >> 4)) as u8);
            true
        }
        3 => {
            let (Some(a), Some(b), Some(c)) = (value(rest[0]), value(rest[1]), value(rest[2])) else {
                return false;
            };
            out.push(((a << 2) | (b >> 4)) as u8);
            out.push(((b << 4) | (c >> 2)) as u8);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_length() {
        for n in 0..70 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let s = encode(&bytes);
            assert_eq!(s.len(), encoded_len(n), "length for {n}");
            assert_eq!(decode(s.as_bytes()).unwrap(), bytes, "round trip for {n}");
        }
    }

    #[test]
    fn fixed_widths_the_format_relies_on() {
        assert_eq!(encoded_len(384), 512);
        assert_eq!(encoded_len(64), 86);
        assert_eq!(encoded_len(32), 43);
    }

    #[test]
    fn alphabet_is_the_url_safe_one() {
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(encode(b"hello"), "aGVsbG8");
        assert!(decode(b"aGVs+A").is_none());
        assert!(decode(b"a").is_none());
    }
}
