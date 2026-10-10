//! seatlock-rs — phase-1 skeleton.
//!
//! Only what the benchmark pipeline needs to boot and be health-checked: config parity with the
//! Java service (same env vars), a PostgreSQL pool of the same size as Hikari's, Actuator-shaped
//! health endpoints, and graceful shutdown. The API itself lands in later phases.

mod config;

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::time::Duration;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = config::Config::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(cfg.pool_size)
        .acquire_timeout(Duration::from_secs(30)) // Hikari connectionTimeout default
        .connect_with(cfg.connect_options()?)
        .await?;

    let app = Router::new()
        .route("/actuator/health", get(health))
        .route("/actuator/health/liveness", get(probe_up))
        .route("/actuator/health/readiness", get(readiness))
        .with_state(pool);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    info!(port = cfg.port, pool_size = cfg.pool_size, "seatlock-rs listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Mirrors Spring Boot's `/actuator/health` with probes enabled and details hidden.
/// TODO(parity): confirm the exact body against the Java oracle capture.
async fn health(State(pool): State<PgPool>) -> (StatusCode, Json<Value>) {
    let status = db_status(&pool).await;
    let code = if status == "UP" { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(json!({ "groups": ["liveness", "readiness"], "status": status })))
}

async fn probe_up() -> Json<Value> {
    Json(json!({ "status": "UP" }))
}

async fn readiness(State(pool): State<PgPool>) -> (StatusCode, Json<Value>) {
    let status = db_status(&pool).await;
    let code = if status == "UP" { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(json!({ "status": status })))
}

async fn db_status(pool: &PgPool) -> &'static str {
    match sqlx::query("SELECT 1").execute(pool).await {
        Ok(_) => "UP",
        Err(_) => "DOWN",
    }
}

/// `server.shutdown: graceful` equivalent: stop accepting, drain in-flight requests.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    info!("shutdown signal received, draining");
}
