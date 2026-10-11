//! Entities and their state machines, mirroring seatlock's `dev.seatlock.domain` package. The
//! transition methods keep the Java messages verbatim because they are part of the API: they end up
//! in problem+json `detail` fields.

use crate::instant;
use crate::java::java_list;
use time::OffsetDateTime;
use uuid::Uuid;

macro_rules! status_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => stringify!($variant)),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $(stringify!($variant) => Some(Self::$variant),)+ _ => None }
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
    };
}

status_enum!(SeatStatus { AVAILABLE, HELD, BOOKED });
status_enum!(HoldStatus { ACTIVE, CONFIRMED, RELEASED, EXPIRED });
status_enum!(BookingStatus { CONFIRMED, CANCELLED });

/// Business-rule failures (`DomainException` and its sealed subclasses).
#[derive(Debug, Clone, PartialEq)]
pub enum DomainError {
    NotFound { resource: &'static str, id: Uuid },
    SeatUnavailable(Vec<Uuid>),
    EventStarted { event_id: Uuid, starts_at: OffsetDateTime },
    HoldExpired(Uuid),
    InvalidState(String),
    InvalidRequest(String),
}

impl DomainError {
    pub fn problem_type(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not-found",
            Self::SeatUnavailable(_) => "seat-unavailable",
            Self::EventStarted { .. } => "event-started",
            Self::HoldExpired(_) => "hold-expired",
            Self::InvalidState(_) => "invalid-state",
            Self::InvalidRequest(_) => "invalid-request",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::InvalidRequest(_) => 422,
            _ => 409,
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "Resource not found",
            Self::SeatUnavailable(_) => "Seat unavailable",
            Self::EventStarted { .. } => "Event already started",
            Self::HoldExpired(_) => "Hold expired",
            Self::InvalidState(_) => "Invalid state transition",
            Self::InvalidRequest(_) => "Invalid request",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NotFound { resource, id } => format!("{resource} {id} was not found"),
            Self::SeatUnavailable(ids) => format!("Seats are no longer available: {}", java_list(ids)),
            Self::EventStarted { event_id, starts_at } => {
                format!("Event {event_id} started at {}", instant::format(*starts_at))
            }
            Self::HoldExpired(id) => format!("Hold {id} has expired; place a new hold"),
            Self::InvalidState(m) | Self::InvalidRequest(m) => m.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub id: Uuid,
    pub name: String,
    pub venue: String,
    pub starts_at: OffsetDateTime,
    pub capacity: i32,
    pub created_at: OffsetDateTime,
}

impl Event {
    /// Holds, confirmations and cancellations are only allowed before the event starts.
    pub fn require_not_started_at(&self, now: OffsetDateTime) -> Result<(), DomainError> {
        if now >= self.starts_at {
            return Err(DomainError::EventStarted { event_id: self.id, starts_at: self.starts_at });
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Seat {
    pub id: Uuid,
    #[allow(dead_code)] // part of the row; queries filter on it in SQL
    pub event_id: Uuid,
    pub section: String,
    pub row_index: i32,
    pub row_label: String,
    pub seat_number: i32,
    pub price_cents: i64,
    pub status: SeatStatus,
    pub hold_id: Option<Uuid>,
    pub version: i64,
}

impl Seat {
    pub fn is_available(&self) -> bool {
        self.status == SeatStatus::AVAILABLE
    }

    pub fn hold_for(&mut self, hold: Uuid) -> Result<(), DomainError> {
        if self.status != SeatStatus::AVAILABLE {
            return Err(DomainError::SeatUnavailable(vec![self.id]));
        }
        self.status = SeatStatus::HELD;
        self.hold_id = Some(hold);
        Ok(())
    }

    pub fn book(&mut self, hold: Uuid) -> Result<(), DomainError> {
        if self.status != SeatStatus::HELD || self.hold_id != Some(hold) {
            return Err(DomainError::InvalidState(format!("Seat {} is not held by hold {hold}", self.id)));
        }
        self.status = SeatStatus::BOOKED;
        Ok(())
    }

    pub fn release_from(&mut self, hold: Uuid) -> Result<(), DomainError> {
        if self.status == SeatStatus::AVAILABLE || self.hold_id != Some(hold) {
            return Err(DomainError::InvalidState(format!("Seat {} is not claimed by hold {hold}", self.id)));
        }
        self.status = SeatStatus::AVAILABLE;
        self.hold_id = None;
        Ok(())
    }

    pub fn label(&self) -> String {
        format!("{}-{}{}", self.section, self.row_label, self.seat_number)
    }
}

/// Seat-map order as Java's `Comparator.comparing(Seat::getSection)...` computes it in memory:
/// `String.compareTo`, i.e. UTF-16 code-unit order. That is *not* the database collation used by
/// the seat listing endpoint (`ORDER BY section`), so `B` sorts before `a` here but may not there.
pub fn seat_map_order(a: &Seat, b: &Seat) -> std::cmp::Ordering {
    a.section
        .encode_utf16()
        .cmp(b.section.encode_utf16())
        .then(a.row_index.cmp(&b.row_index))
        .then(a.seat_number.cmp(&b.seat_number))
}

#[derive(Debug, Clone)]
pub struct Hold {
    pub id: Uuid,
    pub event_id: Uuid,
    pub customer_ref: String,
    pub status: HoldStatus,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub seat_ids: Vec<Uuid>,
    pub version: i64,
}

impl Hold {
    /// A hold is expired from its `expires_at` instant onwards, even before the sweeper runs.
    pub fn is_expired_at(&self, now: OffsetDateTime) -> bool {
        now >= self.expires_at
    }

    pub fn confirm(&mut self, now: OffsetDateTime) -> Result<(), DomainError> {
        if self.status == HoldStatus::EXPIRED || (self.status == HoldStatus::ACTIVE && self.is_expired_at(now)) {
            return Err(DomainError::HoldExpired(self.id));
        }
        self.require_active("confirm")?;
        self.status = HoldStatus::CONFIRMED;
        Ok(())
    }

    pub fn release(&mut self) -> Result<(), DomainError> {
        self.require_active("release")?;
        self.status = HoldStatus::RELEASED;
        Ok(())
    }

    pub fn expire(&mut self, now: OffsetDateTime) -> Result<(), DomainError> {
        self.require_active("expire")?;
        if !self.is_expired_at(now) {
            return Err(DomainError::InvalidState(format!(
                "Hold {} does not expire until {}",
                self.id,
                instant::format(self.expires_at)
            )));
        }
        self.status = HoldStatus::EXPIRED;
        Ok(())
    }

    fn require_active(&self, action: &str) -> Result<(), DomainError> {
        if self.status != HoldStatus::ACTIVE {
            return Err(DomainError::InvalidState(format!(
                "Cannot {action} hold {} because it is {}",
                self.id,
                self.status.as_str()
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Booking {
    pub id: Uuid,
    pub hold_id: Uuid,
    pub event_id: Uuid,
    pub customer_ref: String,
    pub status: BookingStatus,
    pub total_cents: i64,
    pub created_at: OffsetDateTime,
    pub cancelled_at: Option<OffsetDateTime>,
    pub version: i64,
}

impl Booking {
    pub fn cancel(&mut self, now: OffsetDateTime) -> Result<(), DomainError> {
        if self.status != BookingStatus::CONFIRMED {
            return Err(DomainError::InvalidState(format!(
                "Booking {} is already {}",
                self.id,
                self.status.as_str()
            )));
        }
        self.status = BookingStatus::CANCELLED;
        self.cancelled_at = Some(now);
        Ok(())
    }
}

/// One rectangular block of a seat map (`SectionSpec`).
#[derive(Debug, Clone)]
pub struct SectionSpec {
    pub name: String,
    pub rows: i32,
    pub seats_per_row: i32,
    pub price_cents: i64,
}

pub const MAX_PRICE_CENTS: i64 = 100_000_000;
pub const MAX_SEATS_PER_EVENT: i64 = 10_000;

impl SectionSpec {
    /// The domain re-checks its invariants instead of trusting request validation.
    pub fn new(name: String, rows: i32, seats_per_row: i32, price_cents: i64) -> Result<Self, DomainError> {
        if crate::java::java_is_blank(&name) {
            return Err(DomainError::InvalidRequest("Section name must not be blank".into()));
        }
        if rows < 1 || seats_per_row < 1 {
            return Err(DomainError::InvalidRequest(format!(
                "Section {name} needs at least one row and one seat per row"
            )));
        }
        if !(0..=MAX_PRICE_CENTS).contains(&price_cents) {
            return Err(DomainError::InvalidRequest(format!(
                "Section {name} price must be between 0 and {MAX_PRICE_CENTS} cents"
            )));
        }
        Ok(Self { name, rows, seats_per_row, price_cents })
    }
}

/// Validates the layout (`SeatMapLayout.generate`) and returns the total seat count. Seats are
/// generated in SQL from the same specs, so the 10,000-row expansion never exists in memory.
pub fn validate_seat_map(sections: &[SectionSpec]) -> Result<i64, DomainError> {
    if sections.is_empty() {
        return Err(DomainError::InvalidRequest("A seat map needs at least one section".into()));
    }
    let mut names = std::collections::HashSet::with_capacity(sections.len());
    let mut total: i64 = 0;
    for s in sections {
        if !names.insert(s.name.as_str()) {
            return Err(DomainError::InvalidRequest(format!("Duplicate section name: {}", s.name)));
        }
        total += i64::from(s.rows) * i64::from(s.seats_per_row);
    }
    if total > MAX_SEATS_PER_EVENT {
        return Err(DomainError::InvalidRequest(format!(
            "Seat map has {total} seats; the maximum is {MAX_SEATS_PER_EVENT}"
        )));
    }
    Ok(total)
}

/// Spreadsheet-style row labels: 1 -> A, 26 -> Z, 27 -> AA. The service generates labels in SQL
/// (`repo::insert_seat_map`); this is the reference implementation the SQL is tested against.
#[cfg_attr(not(test), allow(dead_code))]
pub fn row_label(index: i32) -> String {
    let mut n = index;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(b'A' + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_labels() {
        assert_eq!(row_label(1), "A");
        assert_eq!(row_label(26), "Z");
        assert_eq!(row_label(27), "AA");
        assert_eq!(row_label(200), "GR");
    }

    #[test]
    fn seat_map_order_is_utf16_not_collation() {
        let seat = |section: &str| Seat {
            id: Uuid::nil(),
            event_id: Uuid::nil(),
            section: section.into(),
            row_index: 1,
            row_label: "A".into(),
            seat_number: 1,
            price_cents: 0,
            status: SeatStatus::AVAILABLE,
            hold_id: None,
            version: 0,
        };
        assert!(seat_map_order(&seat("B"), &seat("a")).is_lt());
    }
}
