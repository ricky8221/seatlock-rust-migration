//! `Idempotency-Key` for POST requests under `/api/` (seatlock's `IdempotencyFilter`).
//!
//! * No header: the request passes through untouched.
//! * New key: claim it (`INSERT … ON CONFLICT DO NOTHING`), run the request, store the response.
//! * Known key, same request: replay the stored response with `Idempotent-Replayed: true`.
//! * Known key, different request: 422. Still in progress: 409 with `Retry-After` (or, for a claim
//!   older than 30 s whose response was never recorded, 409 without it).
//!
//! 5xx responses are not stored: the claim is dropped so the client can retry.

use crate::java::{java_is_blank, java_len};
use crate::problem::Problem;
use crate::{instant, repo, AppState};
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use time::Duration;

pub const HEADER: &str = "idempotency-key";
pub const REPLAYED_HEADER: &str = "idempotent-replayed";
const MAX_KEY_LENGTH: usize = 255;
const MAX_BODY_BYTES: usize = 64 * 1024;
const STALE_CLAIM_AFTER: Duration = Duration::seconds(30);

/// SHA-256 over method, path and body: the same key must always mean the same request.
pub fn fingerprint(method: &str, path: &str, body: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(method.as_bytes());
    h.update(b" ");
    h.update(path.as_bytes());
    h.update(b"\n");
    h.update(body);
    hex::encode(h.finalize())
}

pub async fn idempotency(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if req.method() != Method::POST || !req.uri().path().starts_with("/api/") {
        return next.run(req).await;
    }
    let Some(raw_key) = req.headers().get(HEADER) else {
        return next.run(req).await;
    };
    // Header values are bytes; Tomcat decodes them as ISO-8859-1.
    let key: String = raw_key.as_bytes().iter().map(|&b| char::from(b)).collect();
    if java_is_blank(&key) || java_len(&key) > MAX_KEY_LENGTH {
        return Problem::filter(
            StatusCode::BAD_REQUEST,
            "invalid-idempotency-key",
            format!("Idempotency-Key must be 1-{MAX_KEY_LENGTH} characters"),
        )
        .into_response_with("");
    }

    let too_large = || {
        Problem::filter(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload-too-large",
            format!("Requests with an Idempotency-Key may have at most {MAX_BODY_BYTES} body bytes"),
        )
        .into_response_with("")
    };
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return too_large();
    }
    let (parts, body) = req.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY_BYTES).await else {
        return too_large();
    };
    let path = parts.uri.path().to_owned();
    let fp = fingerprint(parts.method.as_str(), &path, &body);

    let now = state.clock.now();
    let claim = {
        let mut c = match state.pool.acquire().await {
            Ok(c) => c,
            Err(e) => return crate::problem::ApiError::from(e).to_problem().into_response_with(&path),
        };
        repo::claim_key(&mut c, &key, &fp, now).await
    };
    let claim = match claim {
        Ok(c) => c,
        Err(e) => return crate::problem::ApiError::from(e).to_problem().into_response_with(&path),
    };

    match claim {
        repo::Claim::Claimed => {
            let resp = next.run(Request::from_parts(parts, Body::from(body))).await;
            record_or_release(&state, &key, resp).await
        }
        repo::Claim::Replay { status, content_type, location, body } => {
            state.metrics.idempotency_replays.inc();
            let bytes = body.unwrap_or_default().into_bytes();
            let mut resp = Response::new(Body::from(bytes));
            *resp.status_mut() = StatusCode::from_u16(status as u16).unwrap_or(StatusCode::OK);
            let h = resp.headers_mut();
            h.insert(REPLAYED_HEADER, HeaderValue::from_static("true"));
            if let Some(ct) = content_type.and_then(|v| HeaderValue::from_str(&v).ok()) {
                h.insert(header::CONTENT_TYPE, ct);
            }
            if let Some(loc) = location.and_then(|v| HeaderValue::from_str(&v).ok()) {
                h.insert(header::LOCATION, loc);
            }
            resp
        }
        repo::Claim::InProgress(claimed_at) => {
            if claimed_at + STALE_CLAIM_AFTER > state.clock.now() {
                let mut p = Problem::filter(
                    StatusCode::CONFLICT,
                    "idempotency-key-in-progress",
                    "A request with this Idempotency-Key is still being processed",
                );
                p.extra_headers.push((header::RETRY_AFTER, HeaderValue::from_static("1")));
                return p.into_response_with("");
            }
            Problem::filter(
                StatusCode::CONFLICT,
                "idempotency-key-in-progress",
                format!(
                    "The request with this Idempotency-Key started at {} but its response was not recorded. \
                     Check the resource's state before retrying with a new key",
                    instant::format(claimed_at)
                ),
            )
            .into_response_with("")
        }
        repo::Claim::FingerprintMismatch => Problem::filter(
            StatusCode::UNPROCESSABLE_ENTITY,
            "idempotency-key-reused",
            "Idempotency-Key was already used for a different request",
        )
        .into_response_with(""),
    }
}

/// Buffers the downstream response, stores it for replay (or drops the claim on a 5xx), and
/// hands the same bytes to the client. If storing fails the business change has already
/// committed, so the claim is *kept* IN_PROGRESS: a retry must not run the request again.
async fn record_or_release(state: &AppState, key: &str, resp: Response) -> Response {
    let (mut parts, body) = resp.into_parts();
    let bytes: Bytes = match to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = ?e, "could not buffer response body");
            release(state, key).await;
            return Response::from_parts(parts, Body::empty());
        }
    };
    if parts.status.is_server_error() {
        release(state, key).await;
    } else {
        let content_type = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
        let location = parts.headers.get(header::LOCATION).and_then(|v| v.to_str().ok());
        let body_text = String::from_utf8_lossy(&bytes);
        let stored = async {
            let mut c = state.pool.acquire().await?;
            repo::complete_key(&mut c, key, parts.status.as_u16(), content_type, location, &body_text, state.clock.now())
                .await
        }
        .await;
        if let Err(e) = stored {
            tracing::error!(error = ?e, "Could not record the response for an Idempotency-Key; the key stays IN_PROGRESS");
        }
    }
    parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    Response::from_parts(parts, Body::from(bytes))
}

async fn release(state: &AppState, key: &str) {
    let result = async {
        let mut c = state.pool.acquire().await?;
        repo::release_key(&mut c, key).await
    }
    .await;
    if let Err(e) = result {
        tracing::error!(error = ?e, "could not release Idempotency-Key claim");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_method_space_path_newline_body() {
        let mut h = Sha256::new();
        h.update(b"POST /api/v1/events\n{}");
        assert_eq!(fingerprint("POST", "/api/v1/events", b"{}"), hex::encode(h.finalize()));
    }
}
