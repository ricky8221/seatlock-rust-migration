//! SQL. One function per statement the Java service issues through Spring Data JPA / Hibernate,
//! with the same predicates and ordering, so query plans and row-level locking match.
//!
//! Optimistic locking is explicit here: every state change is
//! `UPDATE … SET …, version = version + 1 WHERE id = $1 AND version = $2`, and zero affected rows
//! is reported as [`WriteError::Lost`], exactly where Hibernate would throw `OptimisticLockException`.

use crate::domain::{Booking, BookingStatus, Event, Hold, HoldStatus, Seat, SeatStatus, SectionSpec};
use crate::instant::pg_round;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// A versioned write failed: `Lost` = the optimistic-locking UPDATE matched no row (another
/// transaction changed it first).
#[derive(Debug)]
pub enum WriteError {
    Lost,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for WriteError {
    fn from(e: sqlx::Error) -> Self {
        WriteError::Db(e)
    }
}

const SEAT_COLUMNS: &str =
    "id, event_id, section, row_index, row_label, seat_number, price_cents, status, hold_id, version";

fn seat_from_row(r: &sqlx::postgres::PgRow) -> Result<Seat, sqlx::Error> {
    let status: String = r.try_get("status")?;
    Ok(Seat {
        id: r.try_get("id")?,
        event_id: r.try_get("event_id")?,
        section: r.try_get("section")?,
        row_index: r.try_get("row_index")?,
        row_label: r.try_get("row_label")?,
        seat_number: r.try_get("seat_number")?,
        price_cents: r.try_get("price_cents")?,
        status: SeatStatus::parse(&status).ok_or_else(|| decode_err("seat.status", &status))?,
        hold_id: r.try_get("hold_id")?,
        version: r.try_get("version")?,
    })
}

fn decode_err(column: &str, value: &str) -> sqlx::Error {
    sqlx::Error::Decode(format!("unexpected {column} value {value:?}").into())
}

// ---- events ------------------------------------------------------------------------------

pub async fn insert_event(c: &mut PgConnection, e: &Event) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO event (id, name, venue, starts_at, capacity, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(e.id)
    .bind(&e.name)
    .bind(&e.venue)
    .bind(pg_round(e.starts_at))
    .bind(e.capacity)
    .bind(pg_round(e.created_at))
    .execute(c)
    .await?;
    Ok(())
}

/// Generates the whole seat map inside PostgreSQL from the section specs. Java builds up to
/// 10,000 `Seat` entities in the heap and batch-inserts them; here nothing per-seat is ever
/// allocated in the service. Row labels follow `SeatMapLayout.rowLabel` (A..Z, AA..ZZ, AAA..).
pub async fn insert_seat_map(c: &mut PgConnection, event_id: Uuid, sections: &[SectionSpec]) -> Result<u64, sqlx::Error> {
    let names: Vec<&str> = sections.iter().map(|s| s.name.as_str()).collect();
    let rows: Vec<i32> = sections.iter().map(|s| s.rows).collect();
    let per_row: Vec<i32> = sections.iter().map(|s| s.seats_per_row).collect();
    let prices: Vec<i64> = sections.iter().map(|s| s.price_cents).collect();
    let done = sqlx::query(
        r#"
        INSERT INTO seat (id, event_id, section, row_index, row_label, seat_number, price_cents, status, hold_id, version)
        SELECT gen_random_uuid(), $1, s.name, r,
               CASE WHEN r <= 26 THEN chr(64 + r)
                    WHEN r <= 702 THEN chr(64 + (r - 1) / 26) || chr(65 + (r - 1) % 26)
                    ELSE chr(64 + (r - 27) / 676) || chr(65 + ((r - 27) / 26) % 26) || chr(65 + (r - 1) % 26)
               END,
               n, s.price, 'AVAILABLE', NULL, 0
        FROM unnest($2::text[], $3::int[], $4::int[], $5::bigint[]) AS s(name, nrows, per_row, price)
        CROSS JOIN LATERAL generate_series(1, s.nrows) AS r
        CROSS JOIN LATERAL generate_series(1, s.per_row) AS n
        "#,
    )
    .bind(event_id)
    .bind(&names)
    .bind(&rows)
    .bind(&per_row)
    .bind(&prices)
    .execute(c)
    .await?;
    Ok(done.rows_affected())
}

fn event_from_row(r: &sqlx::postgres::PgRow) -> Result<Event, sqlx::Error> {
    Ok(Event {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        venue: r.try_get("venue")?,
        starts_at: r.try_get("starts_at")?,
        capacity: r.try_get("capacity")?,
        created_at: r.try_get("created_at")?,
    })
}

pub async fn find_event(c: &mut PgConnection, id: Uuid) -> Result<Option<Event>, sqlx::Error> {
    sqlx::query("SELECT id, name, venue, starts_at, capacity, created_at FROM event WHERE id = $1")
        .bind(id)
        .fetch_optional(c)
        .await?
        .map(|r| event_from_row(&r))
        .transpose()
}

pub async fn event_exists(c: &mut PgConnection, id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM event WHERE id = $1)").bind(id).fetch_one(c).await
}

pub async fn list_events(c: &mut PgConnection, limit: i64, offset: i64) -> Result<Vec<Event>, sqlx::Error> {
    sqlx::query(
        "SELECT id, name, venue, starts_at, capacity, created_at FROM event ORDER BY starts_at, id LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(c)
    .await?
    .iter()
    .map(event_from_row)
    .collect()
}

pub async fn count_events(c: &mut PgConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM event").fetch_one(c).await
}

// ---- seats -------------------------------------------------------------------------------

/// The seat listing's filters (`SeatRepository.matching`); `None` means "any".
pub struct SeatFilter<'a> {
    pub event_id: Uuid,
    pub status: Option<SeatStatus>,
    pub section: Option<&'a str>,
}

pub async fn list_seats(c: &mut PgConnection, f: &SeatFilter<'_>, limit: i64, offset: i64) -> Result<Vec<Seat>, sqlx::Error> {
    let sql = format!(
        "SELECT {SEAT_COLUMNS} FROM seat WHERE event_id = $1 \
         AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR section = $3) \
         ORDER BY section, row_index, seat_number LIMIT $4 OFFSET $5"
    );
    sqlx::query(&sql)
        .bind(f.event_id)
        .bind(f.status.map(SeatStatus::as_str))
        .bind(f.section)
        .bind(limit)
        .bind(offset)
        .fetch_all(c)
        .await?
        .iter()
        .map(seat_from_row)
        .collect()
}

pub async fn count_seats(c: &mut PgConnection, f: &SeatFilter<'_>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM seat WHERE event_id = $1 \
         AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR section = $3)",
    )
    .bind(f.event_id)
    .bind(f.status.map(SeatStatus::as_str))
    .bind(f.section)
    .fetch_one(c)
    .await
}

/// `countBySectionAndStatus`: one row per (section, status), ordered by section.
pub async fn count_by_section_and_status(c: &mut PgConnection, event_id: Uuid) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT section, status, count(*) FROM seat WHERE event_id = $1 GROUP BY section, status ORDER BY section",
    )
    .bind(event_id)
    .fetch_all(c)
    .await
}

/// `findByEventIdAndIdInOrderByIdAsc` (ORDER BY id is PostgreSQL's unsigned uuid order).
pub async fn seats_of_event_by_ids(c: &mut PgConnection, event_id: Uuid, ids: &[Uuid]) -> Result<Vec<Seat>, sqlx::Error> {
    let sql = format!("SELECT {SEAT_COLUMNS} FROM seat WHERE event_id = $1 AND id = ANY($2) ORDER BY id");
    sqlx::query(&sql).bind(event_id).bind(ids).fetch_all(c).await?.iter().map(seat_from_row).collect()
}

/// `findByIdInOrderByIdAsc` / `findAllById`.
pub async fn seats_by_ids(c: &mut PgConnection, ids: &[Uuid]) -> Result<Vec<Seat>, sqlx::Error> {
    let sql = format!("SELECT {SEAT_COLUMNS} FROM seat WHERE id = ANY($1) ORDER BY id");
    sqlx::query(&sql).bind(ids).fetch_all(c).await?.iter().map(seat_from_row).collect()
}

pub async fn update_seat(c: &mut PgConnection, s: &Seat) -> Result<(), WriteError> {
    let done = sqlx::query("UPDATE seat SET status = $1, hold_id = $2, version = version + 1 WHERE id = $3 AND version = $4")
        .bind(s.status.as_str())
        .bind(s.hold_id)
        .bind(s.id)
        .bind(s.version)
        .execute(c)
        .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(WriteError::Lost)
    }
}

// ---- holds -------------------------------------------------------------------------------

/// Loads a hold with its seat ids (the EAGER `@ElementCollection` on `Hold.seatIds`).
pub async fn find_hold(c: &mut PgConnection, id: Uuid) -> Result<Option<Hold>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT h.id, h.event_id, h.customer_ref, h.status, h.created_at, h.expires_at, h.version, \
                ARRAY(SELECT seat_id FROM hold_seat WHERE hold_id = h.id) AS seat_ids \
         FROM seat_hold h WHERE h.id = $1",
    )
    .bind(id)
    .fetch_optional(c)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let status: String = r.try_get("status")?;
    Ok(Some(Hold {
        id: r.try_get("id")?,
        event_id: r.try_get("event_id")?,
        customer_ref: r.try_get("customer_ref")?,
        status: HoldStatus::parse(&status).ok_or_else(|| decode_err("seat_hold.status", &status))?,
        created_at: r.try_get("created_at")?,
        expires_at: r.try_get("expires_at")?,
        seat_ids: r.try_get("seat_ids")?,
        version: r.try_get("version")?,
    }))
}

pub async fn insert_hold(c: &mut PgConnection, h: &Hold) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO seat_hold (id, event_id, customer_ref, status, created_at, expires_at, version) \
         VALUES ($1, $2, $3, $4, $5, $6, 0)",
    )
    .bind(h.id)
    .bind(h.event_id)
    .bind(&h.customer_ref)
    .bind(h.status.as_str())
    .bind(pg_round(h.created_at))
    .bind(pg_round(h.expires_at))
    .execute(c)
    .await?;
    Ok(())
}

pub async fn insert_hold_seats(c: &mut PgConnection, hold_id: Uuid, seat_ids: &[Uuid]) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO hold_seat (hold_id, seat_id) SELECT $1, unnest($2::uuid[])")
        .bind(hold_id)
        .bind(seat_ids)
        .execute(c)
        .await?;
    Ok(())
}

pub async fn update_hold(c: &mut PgConnection, h: &Hold) -> Result<(), WriteError> {
    let done = sqlx::query("UPDATE seat_hold SET status = $1, version = version + 1 WHERE id = $2 AND version = $3")
        .bind(h.status.as_str())
        .bind(h.id)
        .bind(h.version)
        .execute(c)
        .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(WriteError::Lost)
    }
}

/// One keyset page of due ACTIVE holds ordered by `(expires_at, id)` (uses the partial index
/// `ix_hold_active_expiry`).
pub async fn find_expired_holds(
    c: &mut PgConnection,
    now: OffsetDateTime,
    after: Option<(OffsetDateTime, Uuid)>,
    limit: i64,
) -> Result<Vec<(Uuid, OffsetDateTime)>, sqlx::Error> {
    let now = pg_round(now);
    match after {
        None => {
            sqlx::query_as(
                "SELECT id, expires_at FROM seat_hold WHERE status = 'ACTIVE' AND expires_at <= $1 \
                 ORDER BY expires_at, id LIMIT $2",
            )
            .bind(now)
            .bind(limit)
            .fetch_all(c)
            .await
        }
        Some((after_expires, after_id)) => {
            sqlx::query_as(
                "SELECT id, expires_at FROM seat_hold WHERE status = 'ACTIVE' AND expires_at <= $1 \
                 AND (expires_at > $2 OR (expires_at = $2 AND id > $3)) \
                 ORDER BY expires_at, id LIMIT $4",
            )
            .bind(now)
            .bind(after_expires)
            .bind(after_id)
            .bind(limit)
            .fetch_all(c)
            .await
        }
    }
}

// ---- bookings ----------------------------------------------------------------------------

pub async fn find_booking(c: &mut PgConnection, id: Uuid) -> Result<Option<Booking>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, hold_id, event_id, customer_ref, status, total_cents, created_at, cancelled_at, version \
         FROM booking WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(c)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let status: String = r.try_get("status")?;
    Ok(Some(Booking {
        id: r.try_get("id")?,
        hold_id: r.try_get("hold_id")?,
        event_id: r.try_get("event_id")?,
        customer_ref: r.try_get("customer_ref")?,
        status: BookingStatus::parse(&status).ok_or_else(|| decode_err("booking.status", &status))?,
        total_cents: r.try_get("total_cents")?,
        created_at: r.try_get("created_at")?,
        cancelled_at: r.try_get("cancelled_at")?,
        version: r.try_get("version")?,
    }))
}

pub async fn insert_booking(c: &mut PgConnection, b: &Booking) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO booking (id, hold_id, event_id, customer_ref, status, total_cents, created_at, cancelled_at, version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0)",
    )
    .bind(b.id)
    .bind(b.hold_id)
    .bind(b.event_id)
    .bind(&b.customer_ref)
    .bind(b.status.as_str())
    .bind(b.total_cents)
    .bind(pg_round(b.created_at))
    .bind(b.cancelled_at.map(pg_round))
    .execute(c)
    .await?;
    Ok(())
}

pub async fn update_booking(c: &mut PgConnection, b: &Booking) -> Result<(), WriteError> {
    let done = sqlx::query(
        "UPDATE booking SET status = $1, cancelled_at = $2, version = version + 1 WHERE id = $3 AND version = $4",
    )
    .bind(b.status.as_str())
    .bind(b.cancelled_at.map(pg_round))
    .bind(b.id)
    .bind(b.version)
    .execute(c)
    .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(WriteError::Lost)
    }
}

// ---- idempotency -------------------------------------------------------------------------

pub enum Claim {
    Claimed,
    Replay { status: i32, content_type: Option<String>, location: Option<String>, body: Option<String> },
    InProgress(OffsetDateTime),
    FingerprintMismatch,
}

/// `JdbcIdempotencyStore.claim`: the primary key arbitrates concurrent first requests.
pub async fn claim_key(c: &mut PgConnection, key: &str, fingerprint: &str, now: OffsetDateTime) -> Result<Claim, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO idempotency_record (idempotency_key, request_fingerprint, state, created_at) \
         VALUES ($1, $2, 'IN_PROGRESS', $3) ON CONFLICT (idempotency_key) DO NOTHING",
    )
    .bind(key)
    .bind(fingerprint)
    .bind(pg_round(now))
    .execute(&mut *c)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Claim::Claimed);
    }
    let row = sqlx::query(
        "SELECT request_fingerprint, state, created_at, response_status, response_content_type, \
                response_location, response_body FROM idempotency_record WHERE idempotency_key = $1",
    )
    .bind(key)
    .fetch_optional(&mut *c)
    .await?;
    let Some(r) = row else {
        // Released between INSERT and SELECT (a 5xx): tell the client to retry.
        return Ok(Claim::InProgress(now));
    };
    let stored: String = r.try_get("request_fingerprint")?;
    if stored.trim_end() != fingerprint {
        return Ok(Claim::FingerprintMismatch);
    }
    let state: String = r.try_get("state")?;
    if state == "IN_PROGRESS" {
        return Ok(Claim::InProgress(r.try_get("created_at")?));
    }
    Ok(Claim::Replay {
        status: r.try_get::<Option<i32>, _>("response_status")?.unwrap_or(200),
        content_type: r.try_get("response_content_type")?,
        location: r.try_get("response_location")?,
        body: r.try_get("response_body")?,
    })
}

pub async fn complete_key(
    c: &mut PgConnection,
    key: &str,
    status: u16,
    content_type: Option<&str>,
    location: Option<&str>,
    body: &str,
    now: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE idempotency_record SET state = 'COMPLETED', response_status = $1, response_content_type = $2, \
         response_location = $3, response_body = $4, completed_at = $5 \
         WHERE idempotency_key = $6 AND state = 'IN_PROGRESS'",
    )
    .bind(i32::from(status))
    .bind(content_type)
    .bind(location)
    .bind(body)
    .bind(pg_round(now))
    .bind(key)
    .execute(c)
    .await?;
    Ok(())
}

pub async fn release_key(c: &mut PgConnection, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM idempotency_record WHERE idempotency_key = $1 AND state = 'IN_PROGRESS'")
        .bind(key)
        .execute(c)
        .await?;
    Ok(())
}

pub async fn delete_keys_created_before(c: &mut PgConnection, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM idempotency_record WHERE created_at < $1")
        .bind(pg_round(cutoff))
        .execute(c)
        .await?
        .rows_affected())
}
