//! RFC 7807 `application/problem+json` responses, byte-compatible with Spring's `ProblemDetail`
//! as seatlock's `ApiExceptionHandler` configures it.
//!
//! Handlers and extractors return an [`ApiError`]. Its `IntoResponse` does not know the request
//! path, which Spring puts in `instance`, so it stashes the problem in a response extension and
//! [`render_problems`] (an outer middleware that does know the path) writes the body.

use crate::domain::DomainError;
use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use uuid::Uuid;

pub const PROBLEM_JSON: &str = "application/problem+json";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self { field: field.into(), message: message.into() }
    }
}

#[derive(Debug)]
pub enum ApiError {
    Domain(DomainError),
    /// `MethodArgumentNotValidException` (@Valid request body).
    BodyValidation(Vec<FieldError>),
    /// `HandlerMethodValidationException` (@Min/@Max on query parameters).
    ParamValidation(Vec<FieldError>),
    /// `MethodArgumentTypeMismatchException` (path/query conversion).
    TypeMismatch { name: &'static str, value: String },
    /// `HttpMessageNotReadableException`: empty, malformed or wrongly-typed JSON.
    Unreadable,
    /// `HttpMediaTypeNotSupportedException`.
    UnsupportedMediaType(String),
    /// Optimistic-locking failure or unique violation that the service did not translate.
    ConcurrentModification,
    /// Anything else: logged, then a generic 500.
    Internal(anyhow::Error),
}

impl From<DomainError> for ApiError {
    fn from(e: DomainError) -> Self {
        ApiError::Domain(e)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if is_unique_violation(&e) {
            return ApiError::ConcurrentModification;
        }
        ApiError::Internal(e.into())
    }
}

pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// The pieces of a problem body. Field order is Jackson's for `ProblemDetail`: type, title,
/// status, detail, instance, then the extension properties.
#[derive(Debug, Clone)]
pub struct Problem {
    pub status: StatusCode,
    pub type_uri: String,
    pub title: String,
    pub detail: String,
    pub errors: Option<Vec<FieldError>>,
    pub seat_ids: Option<Vec<Uuid>>,
    /// Spring's filter-level problems (written by `IdempotencyFilter`) carry no `instance`.
    pub with_instance: bool,
    pub extra_headers: Vec<(header::HeaderName, HeaderValue)>,
}

fn seatlock_type(slug: &str) -> String {
    format!("urn:seatlock:problem:{slug}")
}

impl Problem {
    pub fn new(status: StatusCode, type_uri: String, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            type_uri,
            title: title.into(),
            detail: detail.into(),
            errors: None,
            seat_ids: None,
            with_instance: true,
            extra_headers: Vec::new(),
        }
    }

    /// Spring's default `ProblemDetail` for framework exceptions: `about:blank` + reason phrase.
    pub fn about_blank(status: StatusCode, detail: impl Into<String>) -> Self {
        Self::new(status, "about:blank".into(), reason_phrase(status), detail)
    }

    /// A problem written outside the MVC layer (the idempotency filter): no `instance`, title is
    /// the reason phrase.
    pub fn filter(status: StatusCode, slug: &str, detail: impl Into<String>) -> Self {
        let mut p = Self::new(status, seatlock_type(slug), reason_phrase(status), detail);
        p.with_instance = false;
        p
    }

    pub fn body(&self, instance: &str) -> Vec<u8> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(rename = "type")]
            type_uri: &'a str,
            title: &'a str,
            status: u16,
            detail: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            instance: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            errors: Option<&'a [FieldError]>,
            #[serde(rename = "seatIds", skip_serializing_if = "Option::is_none")]
            seat_ids: Option<&'a [Uuid]>,
        }
        serde_json::to_vec(&Body {
            type_uri: &self.type_uri,
            title: &self.title,
            status: self.status.as_u16(),
            detail: &self.detail,
            instance: self.with_instance.then_some(instance),
            errors: self.errors.as_deref(),
            seat_ids: self.seat_ids.as_deref(),
        })
        .expect("problem serializes")
    }

    pub fn into_response_with(self, instance: &str) -> Response {
        let mut resp = Response::new(Body::from(self.body(instance)));
        *resp.status_mut() = self.status;
        resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        for (k, v) in self.extra_headers {
            resp.headers_mut().insert(k, v);
        }
        resp
    }
}

/// `HttpStatus.getReasonPhrase()` for the codes this service can return.
pub fn reason_phrase(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => status.canonical_reason().unwrap_or(""),
    }
}

impl ApiError {
    pub fn to_problem(&self) -> Problem {
        match self {
            ApiError::Domain(e) => {
                let mut p = Problem::new(
                    StatusCode::from_u16(e.status()).expect("valid status"),
                    seatlock_type(e.problem_type()),
                    e.title(),
                    e.message(),
                );
                if let DomainError::SeatUnavailable(ids) = e {
                    p.seat_ids = Some(ids.clone());
                }
                p
            }
            ApiError::BodyValidation(errors) => {
                let mut p = Problem::new(
                    StatusCode::BAD_REQUEST,
                    seatlock_type("validation"),
                    "Bad Request",
                    "Request body failed validation",
                );
                p.errors = Some(errors.clone());
                p
            }
            ApiError::ParamValidation(errors) => {
                let mut p =
                    Problem::new(StatusCode::BAD_REQUEST, seatlock_type("validation"), "Bad Request", "Validation failure");
                p.errors = Some(errors.clone());
                p
            }
            ApiError::TypeMismatch { name, value } => Problem::about_blank(
                StatusCode::BAD_REQUEST,
                format!("Failed to convert '{name}' with value: '{value}'"),
            ),
            ApiError::Unreadable => Problem::about_blank(StatusCode::BAD_REQUEST, "Failed to read request"),
            ApiError::UnsupportedMediaType(ct) => Problem::about_blank(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("Content-Type '{ct}' is not supported."),
            ),
            ApiError::ConcurrentModification => Problem::new(
                StatusCode::CONFLICT,
                seatlock_type("concurrent-modification"),
                "Concurrent modification",
                "The resource was modified concurrently; reload and retry",
            ),
            ApiError::Internal(e) => {
                tracing::error!(error = ?e, "Unhandled exception");
                Problem::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    seatlock_type("internal-error"),
                    "Internal server error",
                    "An unexpected error occurred",
                )
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let problem = self.to_problem();
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = problem.status;
        resp.extensions_mut().insert(problem);
        resp
    }
}

/// Fallback for unmatched paths: Spring's static-resource handler answers these.
pub async fn no_route(req: Request) -> Response {
    let path = req.uri().path().trim_matches('/');
    Problem::about_blank(StatusCode::NOT_FOUND, format!("No static resource {path}."))
        .into_response_with(req.uri().path())
}

/// Writes problem bodies (with `instance` = request path) and produces Spring's answers for a
/// method a route does not map: 405 with a problem body and `Allow: DELETE, GET` (sorted,
/// comma-space), or for `OPTIONS` a 200 with `Allow: DELETE,GET,HEAD,OPTIONS`. Runs around the
/// whole router.
pub async fn render_problems(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let mut resp = next.run(req).await;

    if let Some(problem) = resp.extensions_mut().remove::<Problem>() {
        let headers = std::mem::take(resp.headers_mut());
        let mut rendered = problem.into_response_with(&path);
        for (k, v) in headers.iter() {
            if k != header::CONTENT_TYPE && k != header::CONTENT_LENGTH {
                rendered.headers_mut().append(k, v.clone());
            }
        }
        return rendered;
    }

    if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
        let Some((_, methods)) = crate::web::route_of(&path) else { return resp };
        let mut sorted: Vec<&str> = methods.to_vec();
        sorted.sort_unstable();
        if method == Method::OPTIONS {
            let mut listed: Vec<&str> = sorted.clone();
            if listed.contains(&"GET") {
                listed.push("HEAD");
                listed.sort_unstable();
            }
            listed.push("OPTIONS");
            let mut ok = Response::new(Body::empty());
            let h = ok.headers_mut();
            h.insert(header::ALLOW, HeaderValue::from_str(&listed.join(",")).expect("ascii"));
            h.insert("accept-patch", HeaderValue::from_static(""));
            h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            return ok;
        }
        let mut p = Problem::about_blank(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("Method '{}' is not supported.", method.as_str()),
        );
        p.extra_headers.push((header::ALLOW, HeaderValue::from_str(&sorted.join(", ")).expect("ascii")));
        return p.into_response_with(&path);
    }
    resp
}
