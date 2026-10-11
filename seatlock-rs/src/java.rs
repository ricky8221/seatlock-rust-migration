//! Small re-implementations of JDK / Jackson / Spring behaviours that leak into seatlock's HTTP
//! contract. Each one was found by diffing against the Java service (see parity/), and each is a
//! place where the "obvious" Rust equivalent produces different bytes.

use std::cmp::Ordering;
use uuid::Uuid;

/// `java.util.UUID.compareTo`: compares the two 64-bit halves as *signed* longs. Rust's `Uuid`
/// (and PostgreSQL's `uuid`) order bytes as unsigned, so `ffffffff-…` sorts first in Java and last
/// in Rust. Used wherever Java iterates a `TreeSet<UUID>` into a response.
pub fn java_uuid_cmp(a: &Uuid, b: &Uuid) -> Ordering {
    let (am, al) = a.as_u64_pair();
    let (bm, bl) = b.as_u64_pair();
    (am as i64).cmp(&(bm as i64)).then((al as i64).cmp(&(bl as i64)))
}

/// `AbstractCollection.toString()` for a collection of UUIDs: `[a, b, c]`.
pub fn java_list(ids: &[Uuid]) -> String {
    let mut out = String::with_capacity(2 + ids.len() * 38);
    out.push('[');
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&id.hyphenated().to_string());
    }
    out.push(']');
    out
}

/// `String.trim()`: strips chars `<= ' '` (ASCII controls and space) only.
pub fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// Hibernate Validator's `@NotBlank`: `value.trim().length() > 0`, i.e. `String.trim()` semantics.
/// An em space (U+2003) is therefore *not* blank.
pub fn java_not_blank(s: &str) -> bool {
    !java_trim(s).is_empty()
}

/// `Character.isWhitespace(int)`: Unicode space separators *except* the no-break spaces, plus a
/// fixed set of ASCII controls. Rust's `char::is_whitespace` includes U+00A0, U+2007 and U+202F.
pub fn java_is_whitespace(c: char) -> bool {
    match c {
        '\u{00A0}' | '\u{2007}' | '\u{202F}' => false,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{001C}'..='\u{001F}' => true,
        ' ' | '\u{1680}' | '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200A}' => true,
        '\u{2028}' | '\u{2029}' | '\u{205F}' | '\u{3000}' => true,
        _ => false,
    }
}

/// `String.strip()`, which `Event.create` applies to name and venue.
pub fn java_strip(s: &str) -> &str {
    s.trim_matches(java_is_whitespace)
}

/// `String.isBlank()`: empty or only `Character.isWhitespace` chars.
pub fn java_is_blank(s: &str) -> bool {
    s.chars().all(java_is_whitespace)
}

/// `String.length()`: UTF-16 code units, which is what `@Size` counts. An emoji counts as 2.
pub fn java_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `Long.parseLong(s, 16)`: optional sign, at least one hex digit, must fit in a signed long.
fn parse_java_long_hex(s: &str) -> Option<i64> {
    let (neg, digits) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i128 = 0;
    for c in digits.chars() {
        value = value * 16 + i128::from(c.to_digit(16)?);
        if value > i128::from(i64::MAX) + 1 {
            return None;
        }
    }
    let value = if neg { -value } else { value };
    i64::try_from(value).ok()
}

/// `UUID.fromString` as Spring's `StringToUUIDConverter` calls it (after `String.trim()`). The JDK
/// is lenient outside the canonical 36-char form: `1-1-1-1-1` is
/// `00000001-0001-0001-0001-000000000001`. Path variables in the Java service accept that, so a
/// strict parser would turn its 404s into 400s.
pub fn java_uuid_from_str(input: &str) -> Option<Uuid> {
    let s = java_trim(input);
    if s.is_empty() || java_len(s) > 36 || !s.is_ascii() {
        return None;
    }
    if s.len() == 36 {
        if let Ok(u) = Uuid::try_parse(s) {
            if s.as_bytes()[8] == b'-' && s.as_bytes()[13] == b'-' {
                return Some(u);
            }
        }
    }
    let parts: Vec<&str> = s.split('-').collect();
    // indexOf-based: exactly four dashes; a fifth makes dash5 >= 0 -> invalid.
    if parts.len() != 5 {
        return None;
    }
    let p = |i: usize| parse_java_long_hex(parts[i]);
    let mut msb = p(0)? & 0xffff_ffff;
    msb = (msb << 16) | (p(1)? & 0xffff);
    msb = (msb << 16) | (p(2)? & 0xffff);
    let mut lsb = p(3)? & 0xffff;
    lsb = (lsb << 48) | (p(4)? & 0xffff_ffff_ffff);
    Some(Uuid::from_u64_pair(msb as u64, lsb as u64))
}

/// Jackson's `UUIDDeserializer` for request bodies: strict. 36 chars with dashes at 8/13/18/23 and
/// hex elsewhere, or 24 chars of Base64 for the 16 raw bytes. (Bodies are strict, path variables
/// lenient: the two Java parsers disagree, so the Rust port has two as well.)
pub fn jackson_uuid(s: &str) -> Option<Uuid> {
    let b = s.as_bytes();
    if b.len() == 36 {
        if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
            return None;
        }
        if !b.iter().enumerate().all(|(i, c)| matches!(i, 8 | 13 | 18 | 23) || c.is_ascii_hexdigit()) {
            return None;
        }
        return Uuid::try_parse(s).ok();
    }
    if b.len() == 24 {
        return base64_16(s).map(Uuid::from_bytes);
    }
    None
}

fn base64_16(s: &str) -> Option<[u8; 16]> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let b = s.as_bytes();
    if &b[22..] != b"==" {
        return None;
    }
    let mut out = [0u8; 16];
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut n = 0;
    for &c in &b[..22] {
        bits = (bits << 6) | val(c)?;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            if n < 16 {
                out[n] = (bits >> nbits) as u8;
                n += 1;
            }
        }
    }
    (n == 16).then_some(out)
}

/// Spring's `StringToNumberConverterFactory` -> `NumberUtils.parseNumber`: removes *all*
/// whitespace, accepts a sign, and decodes `0x`/`0X`/`#` hex prefixes. Returns None on any
/// failure, including overflow of the target type.
pub fn spring_parse_int(raw: &str) -> Option<i32> {
    let s: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let (neg, rest) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s.as_str()),
    };
    let (radix, digits) = if let Some(h) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        (16, h)
    } else if let Some(h) = rest.strip_prefix('#') {
        (16, h)
    } else {
        (10, rest)
    };
    if digits.is_empty() || digits.starts_with(['+', '-']) {
        return None;
    }
    let v = i64::from_str_radix(digits, radix).ok()?;
    i32::try_from(if neg { -v } else { v }).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    #[test]
    fn signed_uuid_order_matches_java_treeset() {
        // Java printed "[ffffffff-…-000000000001, 00000000-…-000000000001]" for these two.
        let mut ids = vec![
            u("00000000-0000-0000-0000-000000000001"),
            u("ffffffff-0000-0000-0000-000000000001"),
        ];
        ids.sort_by(java_uuid_cmp);
        assert_eq!(
            java_list(&ids),
            "[ffffffff-0000-0000-0000-000000000001, 00000000-0000-0000-0000-000000000001]"
        );
    }

    #[test]
    fn lenient_path_uuid_like_the_jdk() {
        assert_eq!(java_uuid_from_str("1-1-1-1-1").unwrap(), u("00000001-0001-0001-0001-000000000001"));
        assert_eq!(
            java_uuid_from_str("0000000A-0000-0000-0000-000000000000").unwrap(),
            u("0000000a-0000-0000-0000-000000000000")
        );
        assert_eq!(java_uuid_from_str(" 1-1-1-1-1 ").unwrap(), u("00000001-0001-0001-0001-000000000001"));
        assert!(java_uuid_from_str("not-a-uuid").is_none());
        assert!(java_uuid_from_str("1-1-1-1").is_none());
        assert!(java_uuid_from_str("1-1-1-1-1-1").is_none());
        assert!(java_uuid_from_str("").is_none());
    }

    #[test]
    fn strict_body_uuid_like_jackson() {
        assert!(jackson_uuid("1-1-1-1-1").is_none());
        assert!(jackson_uuid("00000000-0000-0000-0000-00000000000g").is_none());
        assert_eq!(jackson_uuid("AAAAAAAAAAAAAAAAAAAAAQ==").unwrap(), u("00000000-0000-0000-0000-000000000001"));
    }

    #[test]
    fn whitespace_rules_differ_between_trim_strip_and_rust() {
        assert!(java_not_blank("\u{2003}")); // trim() keeps the em space
        assert_eq!(java_strip("\u{2003}x\u{2003}"), "x");
        assert_eq!(java_strip("\u{00A0}x"), "\u{00A0}x"); // NBSP is not Java whitespace
        assert_eq!(java_len("😀"), 2);
    }

    #[test]
    fn spring_number_conversion() {
        assert_eq!(spring_parse_int(" 1 0 "), Some(10));
        assert_eq!(spring_parse_int("0x10"), Some(16));
        assert_eq!(spring_parse_int("-#f"), Some(-15));
        assert_eq!(spring_parse_int("010"), Some(10));
        assert_eq!(spring_parse_int("x"), None);
        assert_eq!(spring_parse_int("99999999999"), None);
    }
}
