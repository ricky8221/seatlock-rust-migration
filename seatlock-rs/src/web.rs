//! HTTP layer: routes, request parsing in Spring's order (path variables, then query parameters,
//! then the body), and response DTOs whose field order matches the Java records.

use crate::domain::{Booking, Event, Hold, Seat, SeatStatus};
use crate::input;
use crate::java::{java_uuid_from_str, spring_parse_int};
use crate::problem::{ApiError, FieldError};
use crate::service::{self, Availability, BookingDetails, HoldDetails, Page, SectionCounts};
use crate::{instant, AppState};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

type AppResult = Result<Response, ApiError>;
type St = State<Arc<AppState>>;

/// Route templates and the methods each one maps, in Spring's terms. Needed outside the router:
/// 405 responses (Spring's `Allow: DELETE, GET`), `OPTIONS`, and the `uri` tag of the request
/// timer are produced by middleware that wraps the whole router, where axum's `MatchedPath` is
/// not available. (axum also rewrites `Allow` itself *after* per-route layers run.)
pub const ROUTES: &[(&str, &[&str])] = &[
    ("/api/v1/events", &["GET", "POST"]),
    ("/api/v1/events/{eventId}", &["GET"]),
    ("/api/v1/events/{eventId}/seats", &["GET"]),
    ("/api/v1/events/{eventId}/availability", &["GET"]),
    ("/api/v1/events/{eventId}/holds", &["POST"]),
    ("/api/v1/holds/{holdId}", &["DELETE", "GET"]),
    ("/api/v1/holds/{holdId}/confirm", &["POST"]),
    ("/api/v1/bookings/{bookingId}", &["GET"]),
    ("/api/v1/bookings/{bookingId}/cancel", &["POST"]),
    ("/actuator/health", &["GET"]),
    ("/actuator/health/liveness", &["GET"]),
    ("/actuator/health/readiness", &["GET"]),
    ("/actuator/info", &["GET"]),
    ("/actuator/prometheus", &["GET"]),
    ("/v3/api-docs", &["GET"]),
];

/// Matches a request path against [`ROUTES`] (one `{var}` per non-empty segment).
pub fn route_of(path: &str) -> Option<(&'static str, &'static [&'static str])> {
    let segs: Vec<&str> = path.split('/').collect();
    ROUTES.iter().copied().find(|(template, _)| {
        let t: Vec<&str> = template.split('/').collect();
        t.len() == segs.len()
            && t.iter().zip(&segs).all(|(a, b)| if a.starts_with('{') { !b.is_empty() } else { a == b })
    })
}

pub fn api_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/events", get(list_events).post(create_event))
        .route("/api/v1/events/{eventId}", get(get_event))
        .route("/api/v1/events/{eventId}/seats", get(list_seats))
        .route("/api/v1/events/{eventId}/availability", get(availability))
        .route("/api/v1/events/{eventId}/holds", post(place_hold))
        .route("/api/v1/holds/{holdId}", get(get_hold).delete(release_hold))
        .route("/api/v1/holds/{holdId}/confirm", post(confirm_hold))
        .route("/api/v1/bookings/{bookingId}", get(get_booking))
        .route("/api/v1/bookings/{bookingId}/cancel", post(cancel_booking))
}

// ---- argument conversion -----------------------------------------------------------------

fn path_uuid(name: &'static str, raw: &str) -> Result<Uuid, ApiError> {
    java_uuid_from_str(raw).ok_or_else(|| ApiError::TypeMismatch { name, value: raw.to_string() })
}

/// Query parameters as Spring binds them. A repeated parameter arrives as `String[]`: a `String`
/// target gets the values joined with `,`, any other type gets the *first* element
/// (`ArrayToObjectConverter`). An empty value counts as absent when the parameter has a default.
struct Params(Vec<(String, String)>);

impl Params {
    fn values(&self, name: &str) -> Vec<&str> {
        self.0.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    /// For `String` parameters.
    fn joined(&self, name: &str) -> Option<String> {
        let v = self.values(name);
        (!v.is_empty()).then(|| v.join(","))
    }

    /// For converted parameters: (first value, how Spring prints the raw value in errors). For a
    /// repeated parameter the error shows `String[].toString()`, e.g. `[Ljava.lang.String;@3af0288c`
    /// (an identity hash, so not reproducible even between two Java runs).
    fn first(&self, name: &str) -> Option<(String, String)> {
        let v = self.values(name);
        let first = v.first()?.to_string();
        let shown = if v.len() > 1 {
            format!("[Ljava.lang.String;@{:x}", (v.as_ptr() as usize >> 4) as u32)
        } else {
            first.clone()
        };
        Some((first, shown))
    }

    fn int(&self, name: &'static str, default: i32) -> Result<i32, ApiError> {
        match self.first(name) {
            None => Ok(default),
            Some((v, _)) if v.trim().is_empty() => Ok(default),
            Some((v, shown)) => spring_parse_int(&v).ok_or(ApiError::TypeMismatch { name, value: shown }),
        }
    }
}

fn check_page(page: i32, size: i32, max_size: i32) -> Result<(), ApiError> {
    const MAX_PAGE: i32 = 100_000;
    let mut errors = Vec::new();
    if page < 0 {
        errors.push(FieldError::new("page", "must be greater than or equal to 0"));
    }
    if page > MAX_PAGE {
        errors.push(FieldError::new("page", format!("must be less than or equal to {MAX_PAGE}")));
    }
    if size < 1 {
        errors.push(FieldError::new("size", "must be greater than or equal to 1"));
    }
    if size > max_size {
        errors.push(FieldError::new("size", format!("must be less than or equal to {max_size}")));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::ParamValidation(errors))
    }
}

fn created(location: String, body: impl Serialize) -> Response {
    let mut resp = (StatusCode::CREATED, Json(body)).into_response();
    resp.headers_mut().insert(header::LOCATION, HeaderValue::from_str(&location).expect("ascii path"));
    resp
}

// ---- DTOs --------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventDto<'a> {
    id: Uuid,
    name: &'a str,
    venue: &'a str,
    starts_at: String,
    capacity: i32,
    created_at: String,
}

impl<'a> From<&'a Event> for EventDto<'a> {
    fn from(e: &'a Event) -> Self {
        Self {
            id: e.id,
            name: &e.name,
            venue: &e.venue,
            starts_at: instant::format(e.starts_at),
            capacity: e.capacity,
            created_at: instant::format(e.created_at),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeatDto<'a> {
    id: Uuid,
    section: &'a str,
    row: &'a str,
    number: i32,
    label: String,
    price_cents: i64,
    status: SeatStatus,
}

impl<'a> From<&'a Seat> for SeatDto<'a> {
    fn from(s: &'a Seat) -> Self {
        Self {
            id: s.id,
            section: &s.section,
            row: &s.row_label,
            number: s.seat_number,
            label: s.label(),
            price_cents: s.price_cents,
            status: s.status,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HoldDto<'a> {
    id: Uuid,
    event_id: Uuid,
    customer_ref: &'a str,
    status: crate::domain::HoldStatus,
    created_at: String,
    expires_at: String,
    seats: Vec<SeatDto<'a>>,
}

fn hold_dto<'a>(h: &'a Hold, seats: &'a [Seat]) -> HoldDto<'a> {
    HoldDto {
        id: h.id,
        event_id: h.event_id,
        customer_ref: &h.customer_ref,
        status: h.status,
        created_at: instant::format(h.created_at),
        expires_at: instant::format(h.expires_at),
        seats: seats.iter().map(SeatDto::from).collect(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BookingDto<'a> {
    id: Uuid,
    hold_id: Uuid,
    event_id: Uuid,
    customer_ref: &'a str,
    status: crate::domain::BookingStatus,
    total_cents: i64,
    created_at: String,
    /// `default-property-inclusion: non_null`: absent, not `null`, until cancelled.
    #[serde(skip_serializing_if = "Option::is_none")]
    cancelled_at: Option<String>,
    seats: Vec<SeatDto<'a>>,
}

fn booking_dto<'a>(b: &'a Booking, seats: &'a [Seat]) -> BookingDto<'a> {
    BookingDto {
        id: b.id,
        hold_id: b.hold_id,
        event_id: b.event_id,
        customer_ref: &b.customer_ref,
        status: b.status,
        total_cents: b.total_cents,
        created_at: instant::format(b.created_at),
        cancelled_at: b.cancelled_at.map(instant::format),
        seats: seats.iter().map(SeatDto::from).collect(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PageDto<T> {
    items: Vec<T>,
    page: i32,
    size: i32,
    total_items: i64,
    total_pages: i32,
}

fn page_dto<'a, S, T: 'a>(p: &'a Page<S>, f: impl Fn(&'a S) -> T) -> PageDto<T> {
    let size = i64::from(p.size);
    let total_pages = if size == 0 { 1 } else { (p.total_items + size - 1) / size } as i32;
    PageDto { items: p.items.iter().map(f).collect(), page: p.page, size: p.size, total_items: p.total_items, total_pages }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailabilityDto<'a> {
    event_id: Uuid,
    total: i64,
    available: i64,
    held: i64,
    booked: i64,
    sections: Vec<SectionDto<'a>>,
}

#[derive(Serialize)]
struct SectionDto<'a> {
    section: &'a str,
    total: i64,
    available: i64,
    held: i64,
    booked: i64,
}

fn section_dto(s: &SectionCounts) -> SectionDto<'_> {
    SectionDto {
        section: &s.section,
        total: s.available + s.held + s.booked,
        available: s.available,
        held: s.held,
        booked: s.booked,
    }
}

fn availability_dto(a: &Availability) -> AvailabilityDto<'_> {
    AvailabilityDto {
        event_id: a.event_id,
        total: a.total.available + a.total.held + a.total.booked,
        available: a.total.available,
        held: a.total.held,
        booked: a.total.booked,
        sections: a.sections.iter().map(section_dto).collect(),
    }
}

// ---- handlers ----------------------------------------------------------------------------

async fn create_event(State(state): St, headers: HeaderMap, body: Bytes) -> AppResult {
    input::require_json(&headers)?;
    let req = input::create_event(&body, time::OffsetDateTime::now_utc())?;
    let event = service::create_event(&state, req).await?;
    Ok(created(format!("/api/v1/events/{}", event.id), EventDto::from(&event)))
}

async fn list_events(State(state): St, Query(q): Query<Vec<(String, String)>>) -> AppResult {
    let q = Params(q);
    let page = q.int("page", 0)?;
    let size = q.int("size", 20)?;
    check_page(page, size, 100)?;
    let p = service::list_events(&state, page, size).await?;
    Ok(Json(page_dto(&p, EventDto::from)).into_response())
}

async fn get_event(State(state): St, Path(event_id): Path<String>) -> AppResult {
    let id = path_uuid("eventId", &event_id)?;
    let event = service::get_event(&state, id).await?;
    Ok(Json(EventDto::from(&event)).into_response())
}

async fn list_seats(State(state): St, Path(event_id): Path<String>, Query(q): Query<Vec<(String, String)>>) -> AppResult {
    let id = path_uuid("eventId", &event_id)?;
    let q = Params(q);
    let status = match q.first("status") {
        None => None,
        Some((v, _)) if v.is_empty() => None,
        Some((v, shown)) => Some(
            SeatStatus::parse(crate::java::java_trim(&v)).ok_or(ApiError::TypeMismatch { name: "status", value: shown })?,
        ),
    };
    let section = q.joined("section");
    let page = q.int("page", 0)?;
    let size = q.int("size", 50)?;
    check_page(page, size, 500)?;
    let p = service::list_seats(&state, id, status, section.as_deref(), page, size).await?;
    Ok(Json(page_dto(&p, SeatDto::from)).into_response())
}

async fn availability(State(state): St, Path(event_id): Path<String>) -> AppResult {
    let id = path_uuid("eventId", &event_id)?;
    let a = service::availability(&state, id).await?;
    Ok(Json(availability_dto(&a)).into_response())
}

async fn place_hold(State(state): St, Path(event_id): Path<String>, headers: HeaderMap, body: Bytes) -> AppResult {
    let id = path_uuid("eventId", &event_id)?;
    input::require_json(&headers)?;
    let req = input::create_hold(&body)?;
    let HoldDetails { hold, seats } = service::place_hold(&state, id, req).await?;
    Ok(created(format!("/api/v1/holds/{}", hold.id), hold_dto(&hold, &seats)))
}

async fn get_hold(State(state): St, Path(hold_id): Path<String>) -> AppResult {
    let id = path_uuid("holdId", &hold_id)?;
    let HoldDetails { hold, seats } = service::get_hold(&state, id).await?;
    Ok(Json(hold_dto(&hold, &seats)).into_response())
}

async fn release_hold(State(state): St, Path(hold_id): Path<String>) -> AppResult {
    let id = path_uuid("holdId", &hold_id)?;
    let HoldDetails { hold, seats } = service::release_hold(&state, id).await?;
    Ok(Json(hold_dto(&hold, &seats)).into_response())
}

async fn confirm_hold(State(state): St, Path(hold_id): Path<String>) -> AppResult {
    let id = path_uuid("holdId", &hold_id)?;
    let BookingDetails { booking, seats } = service::confirm_hold(&state, id).await?;
    Ok(created(format!("/api/v1/bookings/{}", booking.id), booking_dto(&booking, &seats)))
}

async fn get_booking(State(state): St, Path(booking_id): Path<String>) -> AppResult {
    let id = path_uuid("bookingId", &booking_id)?;
    let BookingDetails { booking, seats } = service::get_booking(&state, id).await?;
    Ok(Json(booking_dto(&booking, &seats)).into_response())
}

async fn cancel_booking(State(state): St, Path(booking_id): Path<String>) -> AppResult {
    let id = path_uuid("bookingId", &booking_id)?;
    let BookingDetails { booking, seats } = service::cancel_booking(&state, id).await?;
    Ok(Json(booking_dto(&booking, &seats)).into_response())
}
