//! The service's notion of "now" (Java's injected `java.time.Clock`). Production reads the system
//! clock; with test hooks enabled, integration tests can pin or advance it over HTTP, which is how
//! the reference test suite drives hold expiry without sleeping.

use std::sync::Mutex;
use time::{Duration, OffsetDateTime};

pub enum Clock {
    System,
    /// A clock that only moves when told to (`MutableClock` in the Java tests).
    Manual(Mutex<OffsetDateTime>),
}

impl Clock {
    pub fn now(&self) -> OffsetDateTime {
        match self {
            Clock::System => OffsetDateTime::now_utc(),
            Clock::Manual(t) => *t.lock().expect("clock mutex poisoned"),
        }
    }

    pub fn set(&self, to: OffsetDateTime) -> bool {
        match self {
            Clock::System => false,
            Clock::Manual(t) => {
                *t.lock().expect("clock mutex poisoned") = to;
                true
            }
        }
    }

    pub fn advance(&self, by: Duration) -> bool {
        match self {
            Clock::System => false,
            Clock::Manual(t) => {
                let mut guard = t.lock().expect("clock mutex poisoned");
                *guard += by;
                true
            }
        }
    }
}
