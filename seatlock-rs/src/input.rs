//! Request bodies, read the way Jackson + Bean Validation read them in the Java service.
//!
//! `serde` with `#[derive(Deserialize)]` is the wrong tool here: Jackson coerces (`"rows":"2"` ->
//! 2, `2.7` -> 2, `1e1` -> 10, a missing or `null` primitive -> 0, `"name":5` -> "5"), and a
//! validation failure must list *every* bad field, while serde stops at the first type error with
//! a different status. So bodies are parsed into `serde_json::Value` and converted field by field
//! with Jackson's rules, then validated with Hibernate Validator's messages.

use crate::domain::MAX_PRICE_CENTS;
use crate::instant;
use crate::java::{jackson_uuid, java_len, java_not_blank};
use crate::problem::{ApiError, FieldError};
use axum::http::{header, HeaderMap};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

type Coerced<T> = Result<T, ApiError>;

/// `MappingJackson2HttpMessageConverter` reads `application/json` and `application/*+json`.
pub fn require_json(headers: &HeaderMap) -> Result<(), ApiError> {
    let raw = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return Err(ApiError::UnsupportedMediaType("application/octet-stream".into()));
    };
    let essence = raw.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let ok = essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"));
    if ok {
        return Ok(());
    }
    // Spring echoes the parsed media type; text/* gets the default charset appended.
    let mut shown = raw.trim().to_string();
    if essence.starts_with("text/") && !shown.to_ascii_lowercase().contains("charset") {
        shown.push_str(";charset=UTF-8");
    }
    Err(ApiError::UnsupportedMediaType(shown))
}

pub fn parse_object(body: &[u8]) -> Coerced<Map<String, Value>> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(ApiError::Unreadable),
    }
}

// ---- Jackson scalar coercions -------------------------------------------------------------

fn j_string(v: Option<&Value>) -> Coerced<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(_) => Err(ApiError::Unreadable),
    }
}

/// Integral primitive (`int`/`long`): missing/null -> 0, float truncated, numeric string parsed,
/// out-of-range or boolean -> unreadable.
fn j_integral(v: Option<&Value>, min: i64, max: i64) -> Coerced<i64> {
    let in_range = |x: i64| if (min..=max).contains(&x) { Ok(x) } else { Err(ApiError::Unreadable) };
    match v {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                in_range(i)
            } else if n.is_u64() {
                Err(ApiError::Unreadable)
            } else {
                let f = n.as_f64().ok_or(ApiError::Unreadable)?;
                if !f.is_finite() || f.trunc() < min as f64 || f.trunc() > max as f64 {
                    return Err(ApiError::Unreadable);
                }
                Ok(f.trunc() as i64)
            }
        }
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                return Ok(0);
            }
            in_range(t.parse::<i64>().map_err(|_| ApiError::Unreadable)?)
        }
        Some(_) => Err(ApiError::Unreadable),
    }
}

fn j_instant(v: Option<&Value>) -> Coerced<Option<OffsetDateTime>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => instant::parse_jackson_string(s).map(Some).ok_or(ApiError::Unreadable),
        Some(Value::Number(n)) => instant::from_decimal_seconds(&n.to_string()).map(Some).ok_or(ApiError::Unreadable),
        Some(_) => Err(ApiError::Unreadable),
    }
}

fn j_uuid(v: &Value) -> Coerced<Option<Uuid>> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.is_empty() => Ok(None),
        Value::String(s) => jackson_uuid(s).map(Some).ok_or(ApiError::Unreadable),
        _ => Err(ApiError::Unreadable),
    }
}

fn j_array(v: Option<&Value>) -> Coerced<Option<&Vec<Value>>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => Ok(Some(a)),
        Some(_) => Err(ApiError::Unreadable),
    }
}

// ---- Hibernate Validator messages ---------------------------------------------------------

const NOT_BLANK: &str = "must not be blank";
const NOT_NULL: &str = "must not be null";
const NOT_EMPTY: &str = "must not be empty";
const FUTURE: &str = "must be a future date";

fn size_max(max: usize) -> String {
    format!("size must be between 0 and {max}")
}
fn min_msg(v: i64) -> String {
    format!("must be greater than or equal to {v}")
}
fn max_msg(v: i64) -> String {
    format!("must be less than or equal to {v}")
}
fn pattern_msg(re: &str) -> String {
    format!("must match \"{re}\"")
}

/// `@NotBlank @Size(max)` on a String.
fn check_text(errors: &mut Vec<FieldError>, field: &str, value: Option<&str>, max: usize) {
    match value {
        Some(s) => {
            if !java_not_blank(s) {
                errors.push(FieldError::new(field, NOT_BLANK));
            }
            if java_len(s) > max {
                errors.push(FieldError::new(field, size_max(max)));
            }
        }
        None => errors.push(FieldError::new(field, NOT_BLANK)),
    }
}

fn check_range(errors: &mut Vec<FieldError>, field: &str, v: i64, min: i64, max: i64, min_message: Option<String>) {
    if v < min {
        errors.push(FieldError::new(field, min_message.unwrap_or_else(|| min_msg(min))));
    }
    if v > max {
        errors.push(FieldError::new(field, max_msg(max)));
    }
}

// ---- CreateEventRequest ------------------------------------------------------------------

#[derive(Debug)]
pub struct SectionRequest {
    pub name: String,
    pub rows: i32,
    pub seats_per_row: i32,
    pub price_cents: i64,
}

#[derive(Debug)]
pub struct CreateEventRequest {
    pub name: String,
    pub venue: String,
    pub starts_at: OffsetDateTime,
    pub sections: Vec<SectionRequest>,
}

/// Section names: `@Pattern(regexp = "[A-Za-z0-9_-]+")`.
fn is_section_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Reads and validates a `CreateEventRequest`. `real_now` is the *system* time: Hibernate's
/// `@Future` uses its own clock, not the service's injectable `Clock`.
pub fn create_event(body: &[u8], real_now: OffsetDateTime) -> Result<CreateEventRequest, ApiError> {
    let obj = parse_object(body)?;
    // Deserialize everything first (any type error is a 400 "Failed to read request")...
    let name = j_string(obj.get("name"))?;
    let venue = j_string(obj.get("venue"))?;
    let starts_at = j_instant(obj.get("startsAt"))?;
    let sections_raw = j_array(obj.get("sections"))?;
    let mut sections: Option<Vec<Option<(Option<String>, i64, i64, i64)>>> = None;
    if let Some(items) = sections_raw {
        let mut parsed = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Value::Null => parsed.push(None),
                Value::Object(m) => parsed.push(Some((
                    j_string(m.get("name"))?,
                    j_integral(m.get("rows"), i32::MIN.into(), i32::MAX.into())?,
                    j_integral(m.get("seatsPerRow"), i32::MIN.into(), i32::MAX.into())?,
                    j_integral(m.get("priceCents"), i64::MIN, i64::MAX)?,
                ))),
                _ => return Err(ApiError::Unreadable),
            }
        }
        sections = Some(parsed);
    }

    // ...then validate every constraint, like a single `@Valid` pass.
    let mut errors = Vec::new();
    check_text(&mut errors, "name", name.as_deref(), 200);
    check_text(&mut errors, "venue", venue.as_deref(), 200);
    match starts_at {
        None => errors.push(FieldError::new("startsAt", NOT_NULL)),
        Some(t) if t <= real_now => errors.push(FieldError::new("startsAt", FUTURE)),
        Some(_) => {}
    }
    match &sections {
        None => errors.push(FieldError::new("sections", NOT_EMPTY)),
        Some(list) => {
            if list.is_empty() {
                errors.push(FieldError::new("sections", NOT_EMPTY));
            }
            if list.len() > 20 {
                errors.push(FieldError::new("sections", size_max(20)));
            }
            for (i, item) in list.iter().enumerate() {
                let prefix = format!("sections[{i}]");
                let Some((sname, rows, spr, price)) = item else {
                    errors.push(FieldError::new(prefix, NOT_NULL));
                    continue;
                };
                let field = format!("{prefix}.name");
                check_text(&mut errors, &field, sname.as_deref(), 40);
                if let Some(s) = sname {
                    if !is_section_name(s) {
                        errors.push(FieldError::new(&field, pattern_msg("[A-Za-z0-9_-]+")));
                    }
                }
                check_range(&mut errors, &format!("{prefix}.rows"), *rows, 1, 200, None);
                check_range(&mut errors, &format!("{prefix}.seatsPerRow"), *spr, 1, 500, None);
                check_range(&mut errors, &format!("{prefix}.priceCents"), *price, 0, MAX_PRICE_CENTS, None);
            }
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::BodyValidation(errors));
    }

    Ok(CreateEventRequest {
        name: name.expect("validated"),
        venue: venue.expect("validated"),
        starts_at: starts_at.expect("validated"),
        sections: sections
            .expect("validated")
            .into_iter()
            .map(|s| {
                let (name, rows, spr, price) = s.expect("validated");
                SectionRequest {
                    name: name.expect("validated"),
                    rows: rows as i32,
                    seats_per_row: spr as i32,
                    price_cents: price,
                }
            })
            .collect(),
    })
}

// ---- CreateHoldRequest -------------------------------------------------------------------

#[derive(Debug)]
pub struct CreateHoldRequest {
    pub seat_ids: Vec<Uuid>,
    pub customer_ref: String,
}

/// `@Pattern(regexp = "[A-Za-z0-9._-]{1,64}")`.
fn is_customer_ref(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn create_hold(body: &[u8]) -> Result<CreateHoldRequest, ApiError> {
    let obj = parse_object(body)?;
    let seat_ids: Option<Vec<Option<Uuid>>> = match j_array(obj.get("seatIds"))? {
        None => None,
        Some(items) => Some(items.iter().map(j_uuid).collect::<Result<_, _>>()?),
    };
    let customer_ref = j_string(obj.get("customerRef"))?;

    let mut errors = Vec::new();
    match &seat_ids {
        None => errors.push(FieldError::new("seatIds", NOT_EMPTY)),
        Some(ids) => {
            if ids.is_empty() {
                errors.push(FieldError::new("seatIds", NOT_EMPTY));
            }
            if ids.len() > 100 {
                errors.push(FieldError::new("seatIds", size_max(100)));
            }
            for (i, id) in ids.iter().enumerate() {
                if id.is_none() {
                    errors.push(FieldError::new(format!("seatIds[{i}]"), NOT_NULL));
                }
            }
        }
    }
    match &customer_ref {
        None => errors.push(FieldError::new("customerRef", NOT_NULL)),
        Some(r) if !is_customer_ref(r) => {
            errors.push(FieldError::new("customerRef", pattern_msg("[A-Za-z0-9._-]{1,64}")))
        }
        Some(_) => {}
    }
    if !errors.is_empty() {
        return Err(ApiError::BodyValidation(errors));
    }
    Ok(CreateHoldRequest {
        seat_ids: seat_ids.expect("validated").into_iter().map(|id| id.expect("validated")).collect(),
        customer_ref: customer_ref.expect("validated"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-10-11 00:00 UTC);

    fn errs(r: Result<CreateEventRequest, ApiError>) -> Vec<(String, String)> {
        match r {
            Err(ApiError::BodyValidation(e)) => {
                let mut v: Vec<_> = e.into_iter().map(|f| (f.field, f.message)).collect();
                v.sort();
                v
            }
            other => panic!("expected validation errors, got {other:?}"),
        }
    }

    #[test]
    fn jackson_coercions() {
        let body = br#"{"name":5,"venue":true,"startsAt":1893456000.5,
            "sections":[{"name":"S","rows":"2","seatsPerRow":2.7,"priceCents":1e1}]}"#;
        let r = create_event(body, NOW).unwrap();
        assert_eq!(r.name, "5");
        assert_eq!(r.venue, "true");
        assert_eq!(instant::format(r.starts_at), "2030-01-01T00:00:00.500Z");
        assert_eq!((r.sections[0].rows, r.sections[0].seats_per_row, r.sections[0].price_cents), (2, 2, 10));
    }

    #[test]
    fn unreadable_bodies() {
        for body in [
            &b"{bad"[..],
            b"[]",
            b"null",
            b"",
            br#"{"sections":[{"rows":true}]}"#,
            br#"{"sections":[{"rows":"x"}]}"#,
            br#"{"sections":[{"rows":99999999999}]}"#,
            br#"{"startsAt":"2030-01-01T00:00:00"}"#,
        ] {
            assert!(matches!(create_event(body, NOW), Err(ApiError::Unreadable)), "{}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn validation_matches_the_java_capture() {
        // Captured from the Java service for this body.
        let body = br#"{"name":"","venue":"Hall","startsAt":"2020-01-01T00:00:00Z",
            "sections":[{"name":"b c","rows":0,"priceCents":-1}]}"#;
        let expected = vec![
            ("name", "must not be blank"),
            ("sections[0].name", "must match \"[A-Za-z0-9_-]+\""),
            ("sections[0].priceCents", "must be greater than or equal to 0"),
            ("sections[0].rows", "must be greater than or equal to 1"),
            ("sections[0].seatsPerRow", "must be greater than or equal to 1"),
            ("startsAt", "must be a future date"),
        ];
        let expected: Vec<_> = expected.into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        assert_eq!(errs(create_event(body, NOW)), expected);
    }

    #[test]
    fn hold_validation() {
        match create_hold(br#"{"seatIds":[null],"customerRef":null}"#) {
            Err(ApiError::BodyValidation(e)) => {
                let fields: Vec<_> = e.iter().map(|f| f.field.as_str()).collect();
                assert_eq!(fields, ["seatIds[0]", "customerRef"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(create_hold(br#"{"seatIds":["1-1-1-1-1"],"customerRef":"c"}"#), Err(ApiError::Unreadable)));
    }

    #[test]
    fn media_types() {
        let mut h = HeaderMap::new();
        assert!(matches!(require_json(&h), Err(ApiError::UnsupportedMediaType(ref s)) if s == "application/octet-stream"));
        h.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(matches!(require_json(&h), Err(ApiError::UnsupportedMediaType(ref s)) if s == "text/plain;charset=UTF-8"));
        h.insert(header::CONTENT_TYPE, "application/json; charset=utf-8".parse().unwrap());
        assert!(require_json(&h).is_ok());
        h.insert(header::CONTENT_TYPE, "application/problem+json".parse().unwrap());
        assert!(require_json(&h).is_ok());
    }
}
