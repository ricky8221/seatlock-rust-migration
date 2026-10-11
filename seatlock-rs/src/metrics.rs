//! Prometheus text exposition in Micrometer's format (`/actuator/prometheus`): the seven
//! business counters, the idempotency replay counter, `http_server_requests_seconds` per route
//! template, and the pool gauge. Hand-rolled on atomics instead of a metrics crate: the series set
//! is small and fixed, and Micrometer's exact naming/formatting (`1.0` for counters) is part of
//! what the reference integration test scrapes.

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct Timer {
    count: u64,
    sum_secs: f64,
    max_secs: f64,
}

#[derive(Default)]
pub struct Metrics {
    pub holds_placed: Counter,
    pub hold_conflicts: Counter,
    pub holds_released: Counter,
    pub holds_expired: Counter,
    pub bookings_confirmed: Counter,
    pub bookings_cancelled: Counter,
    pub expiry_failures: Counter,
    pub idempotency_replays: Counter,
    /// (method, uri template, status) -> timer. A BTreeMap keeps the scrape output stable.
    http: Mutex<BTreeMap<(String, String, u16), Timer>>,
}

fn outcome(status: u16) -> &'static str {
    match status {
        100..=199 => "INFORMATIONAL",
        200..=299 => "SUCCESS",
        300..=399 => "REDIRECTION",
        400..=499 => "CLIENT_ERROR",
        500..=599 => "SERVER_ERROR",
        _ => "UNKNOWN",
    }
}

impl Metrics {
    pub fn record_http(&self, method: &str, uri: &str, status: u16, secs: f64) {
        let mut map = self.http.lock().expect("metrics mutex poisoned");
        let t = map.entry((method.to_string(), uri.to_string(), status)).or_default();
        t.count += 1;
        t.sum_secs += secs;
        if secs > t.max_secs {
            t.max_secs = secs;
        }
    }

    pub fn render(&self, pool_active: u32, pool_idle: u32, pool_max: u32) -> String {
        let mut out = String::with_capacity(4096);
        let counters: [(&str, &str, &Counter); 8] = [
            ("seats_bookings_cancelled_total", "Bookings cancelled", &self.bookings_cancelled),
            ("seats_bookings_confirmed_total", "Holds confirmed", &self.bookings_confirmed),
            ("seats_holds_conflicts_total", "Hold attempts rejected: seat already taken", &self.hold_conflicts),
            ("seats_holds_expired_total", "Holds expired by the sweeper", &self.holds_expired),
            ("seats_holds_expiry_failures_total", "Holds the sweeper failed to expire", &self.expiry_failures),
            ("seats_holds_placed_total", "Holds created", &self.holds_placed),
            ("seats_holds_released_total", "Holds released by the client", &self.holds_released),
            (
                "seats_idempotency_replays_total",
                "Responses replayed for a retried Idempotency-Key",
                &self.idempotency_replays,
            ),
        ];
        for (name, help, c) in counters {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name}{{application=\"seatlock\"}} {:.1}", c.get() as f64);
        }

        for (name, help, v) in [
            ("hikaricp_connections_active", "Active connections", pool_active),
            ("hikaricp_connections_idle", "Idle connections", pool_idle),
            ("hikaricp_connections_max", "Max connections", pool_max),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            let _ = writeln!(out, "{name}{{application=\"seatlock\",pool=\"seatlock-rs\"}} {:.1}", f64::from(v));
        }

        let map = self.http.lock().expect("metrics mutex poisoned");
        let _ = writeln!(out, "# HELP http_server_requests_seconds  ");
        let _ = writeln!(out, "# TYPE http_server_requests_seconds summary");
        for ((method, uri, status), t) in map.iter() {
            let labels = format!(
                "application=\"seatlock\",error=\"none\",exception=\"none\",method=\"{method}\",outcome=\"{}\",status=\"{status}\",uri=\"{uri}\"",
                outcome(*status)
            );
            let _ = writeln!(out, "http_server_requests_seconds_count{{{labels}}} {}", t.count);
            let _ = writeln!(out, "http_server_requests_seconds_sum{{{labels}}} {}", t.sum_secs);
        }
        let _ = writeln!(out, "# HELP http_server_requests_seconds_max  ");
        let _ = writeln!(out, "# TYPE http_server_requests_seconds_max gauge");
        for ((method, uri, status), t) in map.iter() {
            let _ = writeln!(
                out,
                "http_server_requests_seconds_max{{application=\"seatlock\",error=\"none\",exception=\"none\",method=\"{method}\",outcome=\"{}\",status=\"{status}\",uri=\"{uri}\"}} {}",
                outcome(*status),
                t.max_secs
            );
        }
        out
    }
}

/// Times every request under its route template (`/api/v1/events/{eventId}`), like Spring's
/// `http.server.requests` observation. Unmatched paths are tagged `/**` (Spring's resource
/// handler).
pub async fn track_http(State(metrics): State<std::sync::Arc<Metrics>>, req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().as_str().to_owned();
    let uri = crate::web::route_of(req.uri().path()).map(|(t, _)| t).unwrap_or("/**");
    let resp = next.run(req).await;
    if !uri.starts_with("/actuator") {
        metrics.record_http(&method, uri, resp.status().as_u16(), start.elapsed().as_secs_f64());
    }
    resp
}
