//! Operational endpoints (Actuator health/Prometheus, the OpenAPI document) and the test-only
//! hooks that let the reference integration tests drive time and background jobs over HTTP.

use crate::service::{self, ExpireOutcome};
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
use serde_json::{json, Value};
use std::sync::Arc;

type St = State<Arc<AppState>>;

pub fn routes(test_hooks: bool) -> Router<Arc<AppState>> {
    let mut r = Router::new()
        .route("/actuator/health", get(health))
        .route("/actuator/health/liveness", get(probe_up))
        .route("/actuator/health/readiness", get(probe_up))
        .route("/actuator/info", get(|h: HeaderMap| async move { actuator_json(&h, Json(json!({})).into_response()) }))
        .route("/actuator/prometheus", get(prometheus))
        .route("/v3/api-docs", get(api_docs));
    if test_hooks {
        r = r
            .route("/__test/clock", post(set_clock))
            .route("/__test/jobs/expire", post(run_expiry))
            .route("/__test/jobs/idempotency-cleanup", post(run_cleanup))
            .route("/__test/holds/{holdId}/expire", post(expire_one))
            .route("/__test/scheduling", post(set_scheduling));
    }
    r
}

/// Spring Boot's `/actuator/health` with probes enabled and details hidden. A struct, not
/// `json!`: serde_json's map sorts keys alphabetically, but Jackson writes them in declaration
/// order (`{"status":…,"groups":…}`). Verified byte-for-byte against parity/java-health.json.
#[derive(Serialize)]
struct Health {
    status: &'static str,
    groups: [&'static str; 2],
}

async fn health(State(state): St, headers: HeaderMap) -> Response {
    let up = sqlx::query("SELECT 1").execute(&state.pool).await.is_ok();
    let code = if up { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    let body = Json(Health { status: if up { "UP" } else { "DOWN" }, groups: ["liveness", "readiness"] });
    actuator_json(&headers, (code, body).into_response())
}

#[derive(Serialize)]
struct Probe {
    status: &'static str,
}

async fn probe_up(headers: HeaderMap) -> Response {
    actuator_json(&headers, Json(Probe { status: "UP" }).into_response())
}

/// Actuator negotiates its own media type: `application/vnd.spring-boot.actuator.v3+json` unless
/// the client asks for plain `application/json` (and not `*/*`).
fn actuator_json(headers: &HeaderMap, mut resp: Response) -> Response {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let plain = accept.contains("application/json") && !accept.contains("*/*") && !accept.contains("actuator");
    let ct = if plain { "application/json" } else { "application/vnd.spring-boot.actuator.v3+json" };
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    resp
}

async fn prometheus(State(state): St) -> Response {
    let size = state.pool.size();
    let idle = state.pool.num_idle() as u32;
    let body = state.metrics.render(size.saturating_sub(idle), idle, state.pool.options().get_max_connections());
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain;version=0.0.4;charset=utf-8"),
    );
    resp
}

/// springdoc's document, captured from the Java service at the pinned commit and served as text
/// (re-serializing through `serde_json::Value` would sort its keys). springdoc fills
/// `servers[0].url` from the request, so that one value is substituted per request.
async fn api_docs(State(state): St, headers: HeaderMap) -> Response {
    const CAPTURED: &str = "\"url\":\"http://localhost:8080\"";
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost:8080");
    let body = state.openapi.replacen(CAPTURED, &format!("\"url\":\"http://{host}\""), 1);
    let mut resp = body.into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

// ---- test hooks (SEATLOCK_TEST_HOOKS=1 only) -----------------------------------------------

/// `{"instant":"2030-01-01T00:00:00Z"}` pins the clock; `{"advanceSeconds":121}` moves it.
async fn set_clock(State(state): St, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let ok = if let Some(s) = v.get("instant").and_then(Value::as_str) {
        instant::parse_jackson_string(s).map(|t| state.clock.set(t)).unwrap_or(false)
    } else if let Some(secs) = v.get("advanceSeconds").and_then(Value::as_f64) {
        state.clock.advance(time::Duration::seconds_f64(secs))
    } else {
        false
    };
    let status = if ok { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    (status, Json(json!({ "now": instant::format(state.clock.now()) }))).into_response()
}

async fn run_expiry(State(state): St) -> Response {
    match service::sweep_expired_holds(&state).await {
        Ok(n) => Json(json!({ "expired": n })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn run_cleanup(State(state): St) -> Response {
    match service::purge_idempotency_records(&state).await {
        Ok(n) => Json(json!({ "deleted": n })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `ReservationService.expireHold(holdId, at)` for the race tests: `?at=` defaults to now.
async fn expire_one(State(state): St, Path(hold_id): Path<String>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let Some(id) = crate::java::java_uuid_from_str(&hold_id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let at = q
        .iter()
        .find(|(k, _)| k == "at")
        .and_then(|(_, v)| instant::parse_jackson_string(v))
        .unwrap_or_else(|| state.clock.now());
    let result = match service::expire_hold(&state, id, at).await {
        Ok(ExpireOutcome::Expired) => "expired",
        Ok(ExpireOutcome::NothingToDo) => "not-active",
        Ok(ExpireOutcome::LostRace) => "conflict",
        Err(e) => return e.into_response(),
    };
    Json(json!({ "result": result })).into_response()
}

/// `{"enabled":true,"intervalMillis":200}`: the runtime equivalent of
/// `seats.scheduling.enabled=true` + `seats.expiry-sweep-interval=PT0.2S` for one test class.
async fn set_scheduling(State(state): St, body: Bytes) -> Response {
    use std::sync::atomic::Ordering::Relaxed;
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let enabled = v.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let interval = v.get("intervalMillis").and_then(Value::as_u64).unwrap_or(200);
    state.test_sweeper.interval_ms.store(interval, Relaxed);
    state.test_sweeper.enabled.store(enabled, Relaxed);
    Json(json!({ "enabled": enabled, "intervalMillis": interval })).into_response()
}
