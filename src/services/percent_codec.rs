//! Tiny URL percent-encoding / decoding used by the OAuth flow.
//!
//! Avoids pulling in the `url` / `urlencoding` crate for a single query
//! string and a single callback parser. Matches RFC 3986 "unreserved"
//! characters on encode; tolerant decode (invalid escapes pass through).

/// Percent-encodes query values. Only characters in the RFC 3986
/// "unreserved" set are left as-is; everything else is `%XX` uppercase hex.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

/// Percent-decodes a query-string value. `+` → space, `%XX` → byte.
/// Malformed escapes are passed through literally rather than erroring.
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_bytes() {
        assert_eq!(encode("hello world"), "hello%20world");
        assert_eq!(
            encode("http://localhost:1455/auth/callback"),
            "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        );
        assert_eq!(encode("a.b_c-d~e"), "a.b_c-d~e");
    }

    #[test]
    fn decodes_percent_escapes_and_plus() {
        assert_eq!(decode("a%2Bb%3Dc"), "a+b=c");
        assert_eq!(decode("hello+world"), "hello world");
        assert_eq!(decode("nothing_to_decode"), "nothing_to_decode");
    }

    #[test]
    fn decode_tolerates_bad_escapes() {
        assert_eq!(decode("%ZZ"), "%ZZ");
        assert_eq!(decode("%2"), "%2");
    }

    #[test]
    fn roundtrip_preserves_arbitrary_bytes() {
        for s in ["", "abc", "a/b c&d=e", "ünïcödé"] {
            assert_eq!(decode(&encode(s)), s);
        }
    }
}
