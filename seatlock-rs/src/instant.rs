//! `java.time.Instant` as seatlock's API sees it: formatting (Jackson / `Instant.toString`),
//! parsing (Jackson's `InstantDeserializer`), and the rounding PostgreSQL applies when the JDBC
//! driver stores one.

use std::fmt::Write as _;
use time::{Duration, OffsetDateTime, UtcOffset};

/// ISO-8601 the way `DateTimeFormatter.ISO_INSTANT` prints: no fraction when it is zero, otherwise
/// the shortest of 3, 6 or 9 digits that is exact. `2030-01-01T00:00:00.120Z`, never `.12Z`.
/// (RFC 3339 formatters in Rust trim trailing zeros or always print 9 digits; both differ.)
pub fn format(t: OffsetDateTime) -> String {
    let t = t.to_offset(UtcOffset::UTC);
    let mut out = String::with_capacity(30);
    let year = t.year();
    if year > 9999 {
        let _ = write!(out, "+{year}");
    } else if year < 0 {
        let _ = write!(out, "-{:04}", -year);
    } else {
        let _ = write!(out, "{year:04}");
    }
    let _ = write!(
        out,
        "-{:02}-{:02}T{:02}:{:02}:{:02}",
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    );
    let nanos = t.nanosecond();
    if nanos != 0 {
        if nanos % 1_000_000 == 0 {
            let _ = write!(out, ".{:03}", nanos / 1_000_000);
        } else if nanos % 1_000 == 0 {
            let _ = write!(out, ".{:06}", nanos / 1_000);
        } else {
            let _ = write!(out, ".{nanos:09}");
        }
    }
    out.push('Z');
    out
}

/// What PostgreSQL stores when pgjdbc sends an `Instant`: the driver sends text with all 9
/// fraction digits and the server rounds to microseconds with `rint()` (ties to even) on a
/// double. sqlx would instead *truncate* to microseconds, so `GET` after `POST` would differ from
/// Java by 1 µs about half the time. Rounding here before binding keeps stored values identical.
pub fn pg_round(t: OffsetDateTime) -> OffsetDateTime {
    let nanos = t.nanosecond();
    let frac: f64 = format!("0.{nanos:09}").parse().unwrap_or(0.0);
    let micros = (frac * 1_000_000.0).round_ties_even() as i64;
    t.replace_nanosecond(0).expect("0 is a valid nanosecond") + Duration::microseconds(micros)
}

/// Jackson's `InstantDeserializer` for a JSON string value. Accepts what `ISO_INSTANT` accepts
/// (`Z` or a numeric offset, mandatory seconds, 0-9 fraction digits, case-insensitive `T`/`Z`),
/// plus a plain decimal string, which Jackson treats as epoch seconds. Rejects a missing offset.
pub fn parse_jackson_string(raw: &str) -> Option<OffsetDateTime> {
    let s = raw.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'-') {
        if s.bytes().filter(|&b| b == b'.').count() <= 1 && s.bytes().skip(1).all(|b| b != b'-') {
            return from_decimal_seconds(s);
        }
    }
    parse_iso_instant(s)
}

/// Jackson's numeric timestamp handling: integer = epoch seconds, decimal = seconds.nanos.
pub fn from_decimal_seconds(s: &str) -> Option<OffsetDateTime> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let secs: i64 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let mut nanos: i64 = 0;
    for (i, c) in frac_part.chars().take(9).enumerate() {
        nanos += i64::from(c.to_digit(10)?) * 10_i64.pow(8 - i as u32);
    }
    let total = Duration::seconds(secs) + Duration::nanoseconds(nanos);
    let total = if neg { -total } else { total };
    OffsetDateTime::UNIX_EPOCH.checked_add(total)
}

fn parse_iso_instant(s: &str) -> Option<OffsetDateTime> {
    let b = s.as_bytes();
    let mut i = 0;
    let num = |i: &mut usize, width: usize| -> Option<u32> {
        let end = *i + width;
        if end > b.len() || !b[*i..end].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v = s[*i..end].parse().ok()?;
        *i = end;
        Some(v)
    };
    let expect = |i: &mut usize, c: u8| -> Option<()> {
        (b.get(*i).map(u8::to_ascii_uppercase) == Some(c)).then(|| *i += 1)
    };
    let year = num(&mut i, 4)? as i32;
    expect(&mut i, b'-')?;
    let month = num(&mut i, 2)?;
    expect(&mut i, b'-')?;
    let day = num(&mut i, 2)?;
    expect(&mut i, b'T')?;
    let hour = num(&mut i, 2)?;
    expect(&mut i, b':')?;
    let minute = num(&mut i, 2)?;
    // ISO_INSTANT's parser requires seconds (unlike ISO_LOCAL_TIME): "19:00Z" is rejected.
    expect(&mut i, b':')?;
    let second = num(&mut i, 2)?;
    let mut nanos = 0u32;
    {
        if b.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &s[start..i];
            if digits.len() > 9 {
                return None;
            }
            for (k, c) in digits.chars().enumerate() {
                nanos += c.to_digit(10)? * 10_u32.pow(8 - k as u32);
            }
        }
    }
    let offset_secs: i32 = match b.get(i).map(u8::to_ascii_uppercase) {
        Some(b'Z') => {
            i += 1;
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            i += 1;
            let oh = num(&mut i, 2)? as i32;
            let mut om = 0;
            let mut os = 0;
            if b.get(i) == Some(&b':') {
                i += 1;
                om = num(&mut i, 2)? as i32;
                if b.get(i) == Some(&b':') {
                    i += 1;
                    os = num(&mut i, 2)? as i32;
                }
            }
            if oh > 18 || om > 59 || os > 59 {
                return None;
            }
            let total = oh * 3600 + om * 60 + os;
            if sign == b'-' {
                -total
            } else {
                total
            }
        }
        _ => return None,
    };
    if i != b.len() {
        return None;
    }
    let date = time::Date::from_calendar_date(year, time::Month::try_from(month as u8).ok()?, day as u8).ok()?;
    let clock = time::Time::from_hms_nano(hour as u8, minute as u8, second as u8, nanos).ok()?;
    let offset = UtcOffset::from_whole_seconds(offset_secs).ok()?;
    Some(date.with_time(clock).assume_offset(offset).to_offset(UtcOffset::UTC))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn formats_like_iso_instant() {
        assert_eq!(format(datetime!(2030-01-01 00:00:00 UTC)), "2030-01-01T00:00:00Z");
        assert_eq!(format(datetime!(2030-01-01 00:00:00.12 UTC)), "2030-01-01T00:00:00.120Z");
        assert_eq!(format(datetime!(2030-01-01 00:00:00.5622 UTC)), "2030-01-01T00:00:00.562200Z");
        assert_eq!(format(datetime!(2026-10-11 01:45:27.562261559 UTC)), "2026-10-11T01:45:27.562261559Z");
    }

    #[test]
    fn rounds_like_postgres() {
        // Observed on the Java service: .562261559 stored as .562262, .305563545 as .305564.
        assert_eq!(format(pg_round(datetime!(2026-10-11 01:45:27.562261559 UTC))), "2026-10-11T01:45:27.562262Z");
        assert_eq!(format(pg_round(datetime!(2026-10-11 01:45:17.305563545 UTC))), "2026-10-11T01:45:17.305564Z");
        assert_eq!(format(pg_round(datetime!(2026-10-11 01:45:59.999999800 UTC))), "2026-10-11T01:46:00Z");
    }

    #[test]
    fn parses_what_jackson_accepts() {
        let p = |s| parse_jackson_string(s).map(format);
        assert_eq!(p("2030-01-01T00:00:00+02:00").unwrap(), "2029-12-31T22:00:00Z");
        assert_eq!(p("2030-01-01t00:00:00z").unwrap(), "2030-01-01T00:00:00Z");
        assert_eq!(p("1893456000.5").unwrap(), "2030-01-01T00:00:00.500Z");
        assert!(p("2030-01-01T00:00Z").is_none()); // Java: 400, seconds are mandatory
        assert!(p("2030-01-01T00:00:00").is_none());
        assert!(p("2030-01-01T00:00:00.1234567891Z").is_none());
        assert!(p("2030-02-30T00:00:00Z").is_none());
    }
}
