//! Business operations (`EventService`, `ReservationService`, `HoldExpiryJob`,
//! `IdempotencyCleanupJob`). Each write is one database transaction, as in Java.

use crate::domain::{
    self, seat_map_order, Booking, BookingStatus, DomainError, Event, Hold, HoldStatus, Seat, SeatStatus, SectionSpec,
};
use crate::input::{CreateEventRequest, CreateHoldRequest};
use crate::java::{java_list, java_strip, java_uuid_cmp};
use crate::problem::{is_unique_violation, ApiError};
use crate::repo::{self, SeatFilter, WriteError};
use crate::AppState;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub struct Page<T> {
    pub items: Vec<T>,
    pub page: i32,
    pub size: i32,
    pub total_items: i64,
}

pub struct HoldDetails {
    pub hold: Hold,
    pub seats: Vec<Seat>,
}

pub struct BookingDetails {
    pub booking: Booking,
    pub seats: Vec<Seat>,
}

pub struct SectionCounts {
    pub section: String,
    pub available: i64,
    pub held: i64,
    pub booked: i64,
}

pub struct Availability {
    pub event_id: Uuid,
    pub total: SectionCounts,
    pub sections: Vec<SectionCounts>,
}

fn not_found(resource: &'static str, id: Uuid) -> ApiError {
    DomainError::NotFound { resource, id }.into()
}

fn in_seat_map_order(mut seats: Vec<Seat>) -> Vec<Seat> {
    seats.sort_by(seat_map_order);
    seats
}

/// Spring Data's `PageableExecutionUtils.getPage`: skip the count query when the page itself
/// proves the total.
async fn page_total(
    len: usize,
    page: i32,
    size: i32,
    count: impl std::future::Future<Output = Result<i64, sqlx::Error>>,
) -> Result<i64, sqlx::Error> {
    let offset = i64::from(page) * i64::from(size);
    let len = len as i64;
    if offset == 0 {
        if i64::from(size) > len {
            return Ok(len);
        }
        return count.await;
    }
    if len != 0 && i64::from(size) > len {
        return Ok(offset + len);
    }
    count.await
}

// ---- events ------------------------------------------------------------------------------

pub async fn create_event(state: &AppState, req: CreateEventRequest) -> Result<Event, ApiError> {
    let specs = req
        .sections
        .into_iter()
        .map(|s| SectionSpec::new(s.name, s.rows, s.seats_per_row, s.price_cents))
        .collect::<Result<Vec<_>, _>>()?;
    let capacity = domain::validate_seat_map(&specs)?;
    let now = state.clock.now();
    if req.starts_at <= now {
        return Err(DomainError::InvalidRequest("startsAt must be in the future".into()).into());
    }
    if capacity < 1 {
        return Err(DomainError::InvalidRequest("An event needs at least one seat".into()).into());
    }
    let event = Event {
        id: Uuid::new_v4(),
        name: java_strip(&req.name).to_string(),
        venue: java_strip(&req.venue).to_string(),
        starts_at: req.starts_at,
        capacity: capacity as i32,
        created_at: now,
    };
    let mut tx = state.pool.begin().await?;
    repo::insert_event(&mut tx, &event).await?;
    repo::insert_seat_map(&mut tx, event.id, &specs).await?;
    tx.commit().await?;
    Ok(event)
}

pub async fn get_event(state: &AppState, id: Uuid) -> Result<Event, ApiError> {
    let mut c = state.pool.acquire().await?;
    repo::find_event(&mut c, id).await?.ok_or_else(|| not_found("Event", id))
}

pub async fn list_events(state: &AppState, page: i32, size: i32) -> Result<Page<Event>, ApiError> {
    let mut c = state.pool.acquire().await?;
    let items = repo::list_events(&mut c, size.into(), i64::from(page) * i64::from(size)).await?;
    let len = items.len();
    let total_items = page_total(len, page, size, repo::count_events(&mut c)).await?;
    Ok(Page { items, page, size, total_items })
}

pub async fn list_seats(
    state: &AppState,
    event_id: Uuid,
    status: Option<SeatStatus>,
    section: Option<&str>,
    page: i32,
    size: i32,
) -> Result<Page<Seat>, ApiError> {
    let mut c = state.pool.acquire().await?;
    if !repo::event_exists(&mut c, event_id).await? {
        return Err(not_found("Event", event_id));
    }
    let filter = SeatFilter { event_id, status, section };
    let items = repo::list_seats(&mut c, &filter, size.into(), i64::from(page) * i64::from(size)).await?;
    let len = items.len();
    let total_items = page_total(len, page, size, repo::count_seats(&mut c, &filter)).await?;
    Ok(Page { items, page, size, total_items })
}

pub async fn availability(state: &AppState, event_id: Uuid) -> Result<Availability, ApiError> {
    let mut c = state.pool.acquire().await?;
    if !repo::event_exists(&mut c, event_id).await? {
        return Err(not_found("Event", event_id));
    }
    let empty = |section: String| SectionCounts { section, available: 0, held: 0, booked: 0 };
    let mut total = empty(String::new());
    let mut sections: Vec<SectionCounts> = Vec::new();
    for (section, status, n) in repo::count_by_section_and_status(&mut c, event_id).await? {
        // Rows arrive ordered by section; a LinkedHashMap keyed by section in Java.
        if sections.last().map(|s| s.section != section).unwrap_or(true) {
            sections.push(empty(section));
        }
        let counts = sections.last_mut().expect("just pushed");
        match SeatStatus::parse(&status) {
            Some(SeatStatus::AVAILABLE) => {
                counts.available += n;
                total.available += n;
            }
            Some(SeatStatus::HELD) => {
                counts.held += n;
                total.held += n;
            }
            Some(SeatStatus::BOOKED) => {
                counts.booked += n;
                total.booked += n;
            }
            None => {}
        }
    }
    Ok(Availability { event_id, total, sections })
}

// ---- holds -------------------------------------------------------------------------------

/// Places a hold on all requested seats or none of them.
///
/// As in Java, losing a race rolls the transaction back first, then a separate read finds out
/// which requested seats are actually taken (inside the failed transaction that is impossible).
pub async fn place_hold(state: &AppState, event_id: Uuid, req: CreateHoldRequest) -> Result<HoldDetails, ApiError> {
    // TreeSet<UUID>: Java's signed order, duplicates collapse.
    let mut seat_ids = req.seat_ids.clone();
    seat_ids.sort_by(java_uuid_cmp);
    seat_ids.dedup();
    if seat_ids.len() != req.seat_ids.len() {
        return Err(DomainError::InvalidRequest("seatIds contains duplicates".into()).into());
    }
    let max = state.seats.max_seats_per_hold;
    if seat_ids.len() > max {
        return Err(DomainError::InvalidRequest(format!("A hold may contain at most {max} seats")).into());
    }

    match claim_seats(state, event_id, &seat_ids, &req.customer_ref).await {
        Err(ClaimError::LostRace) => {
            state.metrics.hold_conflicts.inc();
            let mut c = state.pool.acquire().await?;
            let taken: Vec<Uuid> = repo::seats_of_event_by_ids(&mut c, event_id, &seat_ids)
                .await?
                .into_iter()
                .filter(|s| !s.is_available())
                .map(|s| s.id)
                .collect();
            let ids = if taken.is_empty() { seat_ids } else { taken };
            Err(DomainError::SeatUnavailable(ids).into())
        }
        Err(ClaimError::Api(e)) => Err(e),
        Ok(details) => {
            state.metrics.holds_placed.inc();
            Ok(details)
        }
    }
}

enum ClaimError {
    LostRace,
    Api(ApiError),
}

impl From<ApiError> for ClaimError {
    fn from(e: ApiError) -> Self {
        ClaimError::Api(e)
    }
}

impl From<DomainError> for ClaimError {
    fn from(e: DomainError) -> Self {
        ClaimError::Api(e.into())
    }
}

impl From<sqlx::Error> for ClaimError {
    fn from(e: sqlx::Error) -> Self {
        ClaimError::Api(e.into())
    }
}

async fn claim_seats(state: &AppState, event_id: Uuid, seat_ids: &[Uuid], customer_ref: &str) -> Result<HoldDetails, ClaimError> {
    let mut tx = state.pool.begin().await?;
    let now = state.clock.now();
    let event = repo::find_event(&mut tx, event_id).await?.ok_or_else(|| not_found("Event", event_id))?;
    event.require_not_started_at(now)?;

    let mut requested = repo::seats_of_event_by_ids(&mut tx, event_id, seat_ids).await?;
    if requested.len() != seat_ids.len() {
        let found: std::collections::HashSet<Uuid> = requested.iter().map(|s| s.id).collect();
        let missing: Vec<Uuid> = seat_ids.iter().copied().filter(|id| !found.contains(id)).collect();
        return Err(DomainError::InvalidRequest(format!(
            "Seats do not belong to event {event_id}: {}",
            java_list(&missing)
        ))
        .into());
    }
    let taken: Vec<Uuid> = requested.iter().filter(|s| !s.is_available()).map(|s| s.id).collect();
    if !taken.is_empty() {
        state.metrics.hold_conflicts.inc();
        return Err(DomainError::SeatUnavailable(taken).into());
    }

    let ttl = Duration::try_from(state.seats.hold_ttl).expect("hold TTL fits");
    let hold = Hold {
        id: Uuid::new_v4(),
        event_id,
        customer_ref: customer_ref.to_string(),
        status: HoldStatus::ACTIVE,
        created_at: now,
        expires_at: now + ttl,
        seat_ids: seat_ids.to_vec(),
        version: 0,
    };
    // Hibernate's flush order: the hold INSERT, then the seat UPDATEs (sorted by id in Java's
    // order, so concurrent transactions lock shared seats in the same order), then the
    // collection rows.
    repo::insert_hold(&mut tx, &hold).await?;
    requested.sort_by(|a, b| java_uuid_cmp(&a.id, &b.id));
    for seat in &mut requested {
        seat.hold_for(hold.id)?;
        match repo::update_seat(&mut tx, seat).await {
            Ok(()) => {}
            Err(WriteError::Lost) => return Err(ClaimError::LostRace), // tx drops -> rollback
            Err(WriteError::Db(e)) => return Err(e.into()),
        }
    }
    repo::insert_hold_seats(&mut tx, hold.id, seat_ids).await?;
    tx.commit().await?;
    Ok(HoldDetails { hold, seats: in_seat_map_order(requested) })
}

pub async fn get_hold(state: &AppState, id: Uuid) -> Result<HoldDetails, ApiError> {
    let mut c = state.pool.acquire().await?;
    let hold = repo::find_hold(&mut c, id).await?.ok_or_else(|| not_found("Hold", id))?;
    let seats = repo::seats_by_ids(&mut c, &hold.seat_ids).await?;
    Ok(HoldDetails { hold, seats: in_seat_map_order(seats) })
}

/// Releases an ACTIVE hold. Releasing an already released hold is a no-op (DELETE semantics).
pub async fn release_hold(state: &AppState, id: Uuid) -> Result<HoldDetails, ApiError> {
    let mut tx = state.pool.begin().await?;
    let mut hold = repo::find_hold(&mut tx, id).await?.ok_or_else(|| not_found("Hold", id))?;
    if hold.status == HoldStatus::RELEASED {
        let seats = repo::seats_by_ids(&mut tx, &hold.seat_ids).await?;
        tx.commit().await?;
        return Ok(HoldDetails { hold, seats: in_seat_map_order(seats) });
    }
    hold.release()?;
    let mut seats = release_seats(&mut tx, &hold).await?;
    let conflict = || DomainError::InvalidState(format!("Hold {id} was modified concurrently; retry"));
    flush(repo::update_hold(&mut tx, &hold).await, conflict)?;
    for seat in &seats {
        flush(repo::update_seat(&mut tx, seat).await, conflict)?;
    }
    tx.commit().await?;
    state.metrics.holds_released.inc();
    seats.sort_by(seat_map_order);
    Ok(HoldDetails { hold, seats })
}

/// Turns an unexpired ACTIVE hold into a booking, if the event has not started yet.
pub async fn confirm_hold(state: &AppState, id: Uuid) -> Result<BookingDetails, ApiError> {
    let mut tx = state.pool.begin().await?;
    let mut hold = repo::find_hold(&mut tx, id).await?.ok_or_else(|| not_found("Hold", id))?;
    let now = state.clock.now();
    let event = repo::find_event(&mut tx, hold.event_id)
        .await?
        .ok_or_else(|| not_found("Event", hold.event_id))?;
    event.require_not_started_at(now)?;
    hold.confirm(now)?;
    let mut held = repo::seats_by_ids(&mut tx, &hold.seat_ids).await?;
    let mut total: i64 = 0;
    for seat in &mut held {
        seat.book(id)?;
        total = total
            .checked_add(seat.price_cents)
            .ok_or_else(|| DomainError::InvalidRequest("Booking total is too large".into()))?;
    }
    let booking = Booking {
        id: Uuid::new_v4(),
        hold_id: hold.id,
        event_id: hold.event_id,
        customer_ref: hold.customer_ref.clone(),
        status: BookingStatus::CONFIRMED,
        total_cents: total,
        created_at: now,
        cancelled_at: None,
        version: 0,
    };
    let conflict = || DomainError::InvalidState(format!("Hold {id} was confirmed or expired concurrently"));
    // Hibernate flushes inserts before updates: the booking first. A parallel confirm blocks on
    // booking.hold_id's UNIQUE index and then fails with 23505.
    match repo::insert_booking(&mut tx, &booking).await {
        Ok(()) => {}
        Err(e) if is_unique_violation(&e) => return Err(conflict().into()),
        Err(e) => return Err(e.into()),
    }
    flush(repo::update_hold(&mut tx, &hold).await, conflict)?;
    held.sort_by(|a, b| java_uuid_cmp(&a.id, &b.id));
    for seat in &held {
        flush(repo::update_seat(&mut tx, seat).await, conflict)?;
    }
    tx.commit().await?;
    state.metrics.bookings_confirmed.inc();
    Ok(BookingDetails { booking, seats: in_seat_map_order(held) })
}

pub async fn get_booking(state: &AppState, id: Uuid) -> Result<BookingDetails, ApiError> {
    let mut c = state.pool.acquire().await?;
    let booking = repo::find_booking(&mut c, id).await?.ok_or_else(|| not_found("Booking", id))?;
    let hold = repo::find_hold(&mut c, booking.hold_id)
        .await?
        .ok_or_else(|| not_found("Hold", booking.hold_id))?;
    let seats = repo::seats_by_ids(&mut c, &hold.seat_ids).await?;
    Ok(BookingDetails { booking, seats: in_seat_map_order(seats) })
}

/// Cancels a confirmed booking and returns its seats to AVAILABLE, unless the event started.
pub async fn cancel_booking(state: &AppState, id: Uuid) -> Result<BookingDetails, ApiError> {
    let mut tx = state.pool.begin().await?;
    let mut booking = repo::find_booking(&mut tx, id).await?.ok_or_else(|| not_found("Booking", id))?;
    let now = state.clock.now();
    let event = repo::find_event(&mut tx, booking.event_id)
        .await?
        .ok_or_else(|| not_found("Event", booking.event_id))?;
    event.require_not_started_at(now)?;
    booking.cancel(now)?;
    let hold = repo::find_hold(&mut tx, booking.hold_id)
        .await?
        .ok_or_else(|| not_found("Hold", booking.hold_id))?;
    let mut seats = release_seats(&mut tx, &hold).await?;
    let conflict = || DomainError::InvalidState(format!("Booking {id} was modified concurrently; retry"));
    flush(repo::update_booking(&mut tx, &booking).await, conflict)?;
    for seat in &seats {
        flush(repo::update_seat(&mut tx, seat).await, conflict)?;
    }
    tx.commit().await?;
    state.metrics.bookings_cancelled.inc();
    seats.sort_by(seat_map_order);
    Ok(BookingDetails { booking, seats })
}

/// Loads the hold's seats and returns them released (not yet written), sorted in Java's update
/// order. Fails if any seat is missing or not claimed by this hold.
async fn release_seats(c: &mut sqlx::PgConnection, hold: &Hold) -> Result<Vec<Seat>, ApiError> {
    let mut claimed = repo::seats_by_ids(c, &hold.seat_ids).await?;
    for seat in &mut claimed {
        seat.release_from(hold.id)?;
    }
    if claimed.len() != hold.seat_ids.len() {
        return Err(DomainError::InvalidState(format!("Hold {} references missing seats", hold.id)).into());
    }
    claimed.sort_by(|a, b| java_uuid_cmp(&a.id, &b.id));
    Ok(claimed)
}

/// `flushOrConflict`: a lost optimistic lock or unique violation is a 409 with `message`; any
/// other database error (CHECK, FK) is a bug and becomes a 500.
fn flush(result: Result<(), WriteError>, conflict: impl Fn() -> DomainError) -> Result<(), ApiError> {
    match result {
        Ok(()) => Ok(()),
        Err(WriteError::Lost) => Err(conflict().into()),
        Err(WriteError::Db(e)) if is_unique_violation(&e) => Err(conflict().into()),
        Err(WriteError::Db(e)) => Err(ApiError::Internal(e.into())),
    }
}

// ---- background jobs ---------------------------------------------------------------------

const MAX_BATCHES_PER_RUN: usize = 20;

pub enum ExpireOutcome {
    Expired,
    NothingToDo,
    /// A confirm or release won the race (optimistic lock); skipped silently.
    LostRace,
}

/// Expires one hold and frees its seats, in its own transaction.
pub async fn expire_hold(state: &AppState, id: Uuid, now: OffsetDateTime) -> Result<ExpireOutcome, ApiError> {
    let mut tx = state.pool.begin().await?;
    let Some(mut hold) = repo::find_hold(&mut tx, id).await? else {
        return Ok(ExpireOutcome::NothingToDo);
    };
    if hold.status != HoldStatus::ACTIVE || !hold.is_expired_at(now) {
        return Ok(ExpireOutcome::NothingToDo);
    }
    hold.expire(now)?;
    let seats = release_seats(&mut tx, &hold).await?;
    match repo::update_hold(&mut tx, &hold).await {
        Ok(()) => {}
        Err(WriteError::Lost) => return Ok(ExpireOutcome::LostRace),
        Err(WriteError::Db(e)) => return Err(e.into()),
    }
    for seat in &seats {
        match repo::update_seat(&mut tx, seat).await {
            Ok(()) => {}
            Err(WriteError::Lost) => return Ok(ExpireOutcome::LostRace),
            Err(WriteError::Db(e)) => return Err(e.into()),
        }
    }
    tx.commit().await?;
    state.metrics.holds_expired.inc();
    Ok(ExpireOutcome::Expired)
}

/// `HoldExpiryJob.sweep`: expires holds due now, at most 20 keyset pages per call; one failing
/// hold never blocks the ones behind it.
pub async fn sweep_expired_holds(state: &AppState) -> Result<u64, ApiError> {
    let now = state.clock.now();
    let batch = state.seats.expiry_batch_size;
    let mut expired = 0;
    let mut cursor: Option<(OffsetDateTime, Uuid)> = None;
    for _ in 0..MAX_BATCHES_PER_RUN {
        let due = {
            let mut c = state.pool.acquire().await?;
            repo::find_expired_holds(&mut c, now, cursor, batch).await?
        };
        for (id, _) in &due {
            match expire_hold(state, *id, now).await {
                Ok(ExpireOutcome::Expired) => expired += 1,
                Ok(ExpireOutcome::NothingToDo) => {}
                Ok(ExpireOutcome::LostRace) => {
                    tracing::debug!(hold = %id, "Hold changed while expiring (confirmed or released); skipped")
                }
                Err(e) => {
                    state.metrics.expiry_failures.inc();
                    tracing::warn!(hold = %id, error = ?e, "Could not expire hold; will retry on the next run");
                }
            }
        }
        if (due.len() as i64) < batch {
            break;
        }
        cursor = due.last().map(|(id, at)| (*at, *id));
    }
    Ok(expired)
}

pub async fn purge_idempotency_records(state: &AppState) -> Result<u64, ApiError> {
    let retention = Duration::try_from(state.seats.idempotency_retention).expect("retention fits");
    let mut c = state.pool.acquire().await?;
    Ok(repo::delete_keys_created_before(&mut c, state.clock.now() - retention).await?)
}

/// Database-backed tests. They need a migrated PostgreSQL and are skipped unless
/// `TEST_DATABASE_URL` is set (e.g. `postgres://seats:seats@localhost:5433/seats`).
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::input::{CreateEventRequest, CreateHoldRequest, SectionRequest};
    use std::sync::Arc;

    async fn state() -> Option<Arc<AppState>> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(&url).await.expect("connect");
        Some(Arc::new(AppState {
            pool,
            clock: crate::clock::Clock::System,
            seats: crate::config::SeatsConfig {
                hold_ttl: std::time::Duration::from_secs(120),
                max_seats_per_hold: 10,
                expiry_batch_size: 100,
                expiry_sweep_interval: std::time::Duration::from_secs(5),
                idempotency_retention: std::time::Duration::from_secs(86_400),
            },
            metrics: Default::default(),
            openapi: "",
            test_sweeper: Default::default(),
        }))
    }

    async fn event_with_hold(s: &AppState) -> (Uuid, Vec<Uuid>, Uuid) {
        let event = create_event(
            s,
            CreateEventRequest {
                name: "stale".into(),
                venue: "test".into(),
                starts_at: OffsetDateTime::now_utc() + Duration::days(30),
                sections: vec![SectionRequest { name: "S".into(), rows: 1, seats_per_row: 2, price_cents: 100 }],
            },
        )
        .await
        .unwrap_or_else(|e| panic!("create event: {e:?}"));
        let seats = list_seats(s, event.id, None, None, 0, 10).await.unwrap_or_else(|e| panic!("{e:?}")).items;
        let ids: Vec<Uuid> = seats.iter().map(|x| x.id).collect();
        let hold = place_hold(s, event.id, CreateHoldRequest { seat_ids: ids.clone(), customer_ref: "stale".into() })
            .await
            .unwrap_or_else(|e| panic!("hold: {e:?}"));
        (event.id, ids, hold.hold.id)
    }

    /// Rust twin of `ConcurrencyIT.expiryWorkingFromAStaleHoldIsRejectedByTheHoldVersion` (the
    /// one reference test that cannot run over HTTP): an expiry transaction reads the hold while
    /// it is ACTIVE, a confirm commits on another connection, then the expiry carries on with its
    /// stale copy. Its seat reads would be fresh; only the hold's version stops it.
    #[tokio::test]
    async fn expiry_working_from_a_stale_hold_is_rejected_by_the_hold_version() {
        let Some(s) = state().await else { return };
        let (_, seat_ids, hold_id) = event_with_hold(&s).await;

        let mut sweeper = s.pool.begin().await.unwrap();
        let mut stale = repo::find_hold(&mut sweeper, hold_id).await.unwrap().unwrap();
        assert_eq!(stale.status, HoldStatus::ACTIVE);

        confirm_hold(&s, hold_id).await.unwrap_or_else(|e| panic!("confirm: {e:?}"));

        let past_ttl = OffsetDateTime::now_utc() + Duration::hours(1);
        stale.expire(past_ttl).unwrap();
        assert!(matches!(repo::update_hold(&mut sweeper, &stale).await, Err(WriteError::Lost)));
        sweeper.rollback().await.unwrap();

        let mut c = s.pool.acquire().await.unwrap();
        assert_eq!(repo::find_hold(&mut c, hold_id).await.unwrap().unwrap().status, HoldStatus::CONFIRMED);
        for seat in repo::seats_by_ids(&mut c, &seat_ids).await.unwrap() {
            assert_eq!(seat.status, SeatStatus::BOOKED);
        }
    }

    /// The SQL row-label expression matches `SeatMapLayout.rowLabel` for every row count the
    /// API allows (and beyond, into three letters).
    #[tokio::test]
    async fn sql_row_labels_match_the_java_algorithm() {
        let Some(s) = state().await else { return };
        let labels: Vec<(i32, String)> = sqlx::query_as(
            "SELECT r, CASE WHEN r <= 26 THEN chr(64 + r) \
                            WHEN r <= 702 THEN chr(64 + (r - 1) / 26) || chr(65 + (r - 1) % 26) \
                            ELSE chr(64 + (r - 27) / 676) || chr(65 + ((r - 27) / 26) % 26) || chr(65 + (r - 1) % 26) END \
             FROM generate_series(1, 2000) r",
        )
        .fetch_all(&s.pool)
        .await
        .unwrap();
        for (r, label) in labels {
            assert_eq!(label, domain::row_label(r), "row {r}");
        }
    }
}
